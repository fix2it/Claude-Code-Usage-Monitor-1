//! Qoder CLI usage.
//!
//! Qoder has no plain HTTP usage endpoint: its quota requests are signed by
//! a bundled WASM module and the responses are encrypted. The only stable
//! source is the CLI's own `/usage` panel, so each probe starts `qodercli` in
//! a pseudo-console, types `/usage`, reads the screen and kills the process
//! tree. A probe costs no credits but runs a ~220 MB CLI for ~11 s, so probes
//! are rate limited to one per [`MIN_PROBE_INTERVAL`] whatever the global
//! poll interval is; polls in between return the last reading.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::PollError;
use crate::diagnose;
use crate::models::{CreditPool, CreditsSection, UsageData, UsageLimit, UsageSection};

/// Agreed with the owner: launching the CLI is too heavy for 5-minute polls.
const MIN_PROBE_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Hard cap on one probe; the process tree is killed when it is reached.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const BOOT_TIMEOUT: Duration = Duration::from_secs(20);

struct ProbeMemo {
    at: Instant,
    result: Result<UsageData, PollError>,
}

static LAST_PROBE: Mutex<Option<ProbeMemo>> = Mutex::new(None);

/// `%USERPROFILE%\.qoder\bin\qodercli\qodercli.exe`; the CLI is not on PATH.
pub fn qodercli_path() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(crate::known_folders::home_dir)?;
    let path = home
        .join(".qoder")
        .join("bin")
        .join("qodercli")
        .join("qodercli.exe");
    path.is_file().then_some(path)
}

fn auth_file() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(crate::known_folders::home_dir)?;
    Some(home.join(".qoder").join(".auth").join("user"))
}

pub(super) fn poll_qoder() -> Result<UsageData, PollError> {
    let Some(exe) = qodercli_path() else {
        diagnose::log("Qoder usage poll skipped: qodercli is not installed");
        return Err(PollError::NoCredentials);
    };
    let mut memo = LAST_PROBE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(last) = memo.as_ref() {
        if last.at.elapsed() < MIN_PROBE_INTERVAL {
            return last.result.clone();
        }
    }
    let result = probe(&exe).and_then(|screen| {
        parse_usage_screen(&screen)
            .and_then(|usage| usage_data(&usage))
            .ok_or_else(|| {
                diagnose::log("Qoder /usage screen contained no usable figures");
                PollError::UnexpectedResponse
            })
    });
    *memo = Some(ProbeMemo {
        at: Instant::now(),
        result: result.clone(),
    });
    result
}

pub(super) fn credential_watch_snapshot(_all_sources: bool) -> Vec<String> {
    match auth_file() {
        Some(path) => vec![crate::accounts::file_signature(&path)],
        None => vec!["qoder|missing".into()],
    }
}

// ---------------------------------------------------------------- parsing --

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct QoderUsage {
    pub plan: Option<String>,
    pub plan_expires_at: Option<SystemTime>,
    pub plan_credits: Option<(f64, f64)>,
    pub add_on_credits: Option<(f64, f64)>,
    pub ultimate_credits: Option<(f64, f64)>,
    pub ultimate_expires_at: Option<SystemTime>,
}

/// Remove escape sequences. The CLI positions text with cursor moves, so
/// spaces largely vanish; every matcher below ignores whitespace.
fn strip_ansi(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                // CSI: parameters/intermediates until a final byte @..~.
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                // OSC: until BEL or ESC \.
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' {
                        chars.next();
                        break;
                    }
                }
            }
            Some('(' | ')') => {
                chars.next();
            }
            _ => {}
        }
    }
    out
}

/// Parse the `/usage` panel. Every field is optional; the screen is redrawn
/// several times, so the last occurrence of a field that parses wins.
///
/// Cursor moves remove both the spaces and the line breaks, so the panel is
/// read as one whitespace-free string: each value runs from its label to the
/// next known label.
pub(super) fn parse_usage_screen(raw: &str) -> Option<QoderUsage> {
    let text: String = strip_ansi(raw)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let mut usage = QoderUsage::default();
    for value in label_values(&text, "QoderPlan:") {
        if !value.is_empty() {
            usage.plan = Some(value.to_string());
        }
    }
    usage.plan_expires_at = last_parsed(&text, "PlanExpiresAt:", parse_date);
    usage.plan_credits = last_parsed(&text, "PlanCreditsUsed:", parse_ratio);
    usage.add_on_credits = last_parsed(&text, "Add-onCreditsUsed:", parse_ratio);
    usage.ultimate_credits = last_parsed(&text, "UltimateExclusiveCredits", |value| {
        parse_ratio(value.strip_prefix(':').unwrap_or(value))
    });
    // `Available · 478 remaining · Expires Nov 14, 2026` under the Ultimate pool.
    usage.ultimate_expires_at = text
        .match_indices("Expires")
        .filter(|(index, _)| {
            let before = &text[..*index];
            let start = before.char_indices().rev().nth(15).map_or(0, |(i, _)| i);
            before[start..].to_ascii_lowercase().contains("remaining")
        })
        .filter_map(|(index, label)| parse_date(value_after(&text, index + label.len())))
        .last();
    (usage != QoderUsage::default()).then_some(usage)
}

const LABELS: [&str; 14] = [
    "QoderPlan:",
    "PlanExpiresAt:",
    "PlanCreditsUsed:",
    "Add-onCreditsUsed:",
    "OrgResourcePackage",
    "TotalDuration",
    "TotalCodeChanges",
    "DedicatedResourcePackages",
    "UltimateExclusiveCredits",
    "Available",
    "Promotions",
    "UltimateTrialPack",
    "Tab/",
    "APIquota",
];

/// Text from `start` up to the next known label (or the end).
fn value_after(text: &str, start: usize) -> &str {
    let rest = &text[start..];
    let end = LABELS
        .iter()
        .filter_map(|label| rest.find(label))
        .min()
        .unwrap_or(rest.len());
    &rest[..end]
}

fn last_parsed<T>(text: &str, label: &str, parse: impl Fn(&str) -> Option<T>) -> Option<T> {
    label_values(text, label).filter_map(parse).last()
}

fn label_values<'a>(text: &'a str, label: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    text.match_indices(label)
        .map(move |(index, _)| value_after(text, index + label.len()))
}

/// `25/300`, optionally followed by text (`22/500credits`).
fn parse_ratio(text: &str) -> Option<(f64, f64)> {
    let (used, rest) = text.split_once('/')?;
    let used = parse_number(used)?;
    let total_text: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
        .collect();
    let total = parse_number(&total_text)?;
    (total > 0.0 && used >= 0.0).then_some((used, total))
}

fn parse_number(text: &str) -> Option<f64> {
    let value = text.replace(',', "").parse::<f64>().ok()?;
    value.is_finite().then_some(value)
}

/// `Nov25,2026,21:35:12GMT+2` or `Nov14,2026` (taken as midnight UTC).
fn parse_date(text: &str) -> Option<SystemTime> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS
        .iter()
        .position(|name| text.get(..3).is_some_and(|m| m.eq_ignore_ascii_case(name)))?
        as u32
        + 1;
    let mut parts = text[3..].split(',');
    let day: u32 = parts.next()?.parse().ok()?;
    let year_text: String = parts
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let year: i64 = year_text.parse().ok()?;
    if !(1..=31).contains(&day) || !(2000..=2100).contains(&year) {
        return None;
    }
    let mut seconds = days_from_civil(year, month, day) * 86_400;
    if let Some(time) = parts.next() {
        let (clock, zone) = match time.find("GMT").or_else(|| time.find("UTC")) {
            Some(index) => (&time[..index], Some(&time[index + 3..])),
            None => (time, None),
        };
        let fields: Vec<i64> = clock
            .split(':')
            .map(|field| field.parse().ok())
            .collect::<Option<_>>()?;
        let (h, m, s) = match fields.as_slice() {
            [h, m] => (*h, *m, 0),
            [h, m, s] => (*h, *m, *s),
            _ => return None,
        };
        seconds += h * 3600 + m * 60 + s;
        if let Some(zone) = zone.filter(|zone| !zone.is_empty()) {
            seconds -= parse_offset(zone)?;
        }
    }
    u64::try_from(seconds)
        .ok()
        .map(|secs| UNIX_EPOCH + Duration::from_secs(secs))
}

/// `+2`, `-5`, `+5:30`, `+0530` → seconds east of UTC.
fn parse_offset(zone: &str) -> Option<i64> {
    let (sign, digits) = match zone.as_bytes().first()? {
        b'+' => (1, &zone[1..]),
        b'-' => (-1, &zone[1..]),
        _ => return None,
    };
    let (hours, minutes) = match digits.split_once(':') {
        Some((h, m)) => (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?),
        None if digits.len() > 2 => (
            digits[..digits.len() - 2].parse().ok()?,
            digits[digits.len() - 2..].parse().ok()?,
        ),
        None => (digits.parse().ok()?, 0),
    };
    Some(sign * (hours * 3600 + minutes * 60))
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let month = month as i64;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn used_percentage((used, total): (f64, f64)) -> f64 {
    (used / total * 100.0).clamp(0.0, 100.0)
}

/// Cache shape (see `docs/qoder-provider.md`): plan credits fill both
/// `weekly` (what widgets and tray read, labelled "Plan") and `monthly`
/// (what `limits.py` reads); add-on credits are `credits`; every pool is also
/// listed with absolute figures in `credit_pools`.
pub(super) fn usage_data(usage: &QoderUsage) -> Option<UsageData> {
    let plan = usage.plan_credits.map(|ratio| UsageSection {
        available: true,
        percentage: used_percentage(ratio),
        resets_at: usage.plan_expires_at,
    });
    let pool = |key: &str, label: &str, ratio: Option<(f64, f64)>, expires| {
        ratio.map(|(used, total)| CreditPool {
            key: key.into(),
            label: label.into(),
            used,
            total,
            expires_at: expires,
        })
    };
    let credit_pools: Vec<CreditPool> = [
        pool(
            "plan",
            "Plan Credits",
            usage.plan_credits,
            usage.plan_expires_at,
        ),
        pool("add_on", "Add-on Credits", usage.add_on_credits, None),
        pool(
            "ultimate_exclusive",
            "Ultimate Exclusive Credits",
            usage.ultimate_credits,
            usage.ultimate_expires_at,
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    if credit_pools.is_empty() {
        return None;
    }
    let limits = usage
        .ultimate_credits
        .map(|ratio| UsageLimit {
            key: "ultimate_exclusive".into(),
            kind: "credits".into(),
            label: "Ultimate Exclusive Credits".into(),
            model: None,
            model_id: None,
            scope: None,
            is_active: ratio.0 > 0.0,
            usage: UsageSection {
                available: true,
                percentage: used_percentage(ratio),
                resets_at: usage.ultimate_expires_at,
            },
        })
        .into_iter()
        .collect();
    Some(UsageData {
        weekly: plan.clone().unwrap_or_default(),
        weekly_label: plan.as_ref().map(|_| "Plan".into()),
        monthly: plan,
        credits: usage.add_on_credits.map(|(used, total)| CreditsSection {
            percentage: used_percentage((used, total)),
            remaining: (total - used).max(0.0),
            total,
        }),
        limits,
        plan_name: usage.plan.clone(),
        credit_pools,
        ..Default::default()
    })
}

// ------------------------------------------------------------------ probe --

fn probe(exe: &std::path::Path) -> Result<String, PollError> {
    let workdir = std::env::temp_dir().join("ClaudeCodeUsageMonitor-qoder");
    let _ = std::fs::create_dir_all(&workdir);
    let started = Instant::now();
    let result = pty::run_usage(exe, &workdir);
    match &result {
        Ok(screen) => diagnose::log(format!(
            "Qoder probe finished in {:.1}s ({} bytes)",
            started.elapsed().as_secs_f32(),
            screen.len()
        )),
        Err(error) => diagnose::log(format!(
            "Qoder probe failed after {:.1}s: {error:?}",
            started.elapsed().as_secs_f32()
        )),
    }
    result
}

fn screen_has(buffer: &Mutex<Vec<u8>>, from: usize, needle: &str) -> bool {
    let bytes = buffer.lock().unwrap_or_else(|poison| poison.into_inner());
    let text = strip_ansi(&String::from_utf8_lossy(&bytes[from.min(bytes.len())..]));
    let text: String = text
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    text.contains(needle)
}

mod pty {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
    use windows::Win32::System::Console::{ClosePseudoConsole, CreatePseudoConsole, COORD, HPCON};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Pipes::CreatePipe;
    use windows::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
        ResumeThread, UpdateProcThreadAttribute, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
        EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    };

    use super::{screen_has, PollError, BOOT_TIMEOUT, PROBE_TIMEOUT};

    /// A handle that closes itself; raw handles are plain kernel references
    /// and safe to move across threads.
    struct Owned(HANDLE);
    unsafe impl Send for Owned {}
    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    /// Kills the whole process tree when dropped, on every exit path. The job
    /// is created with kill-on-close, so if the monitor itself exits or
    /// crashes mid-probe Windows closes the handle and kills qodercli too.
    struct Job(Owned);
    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                let _ = TerminateJobObject((self.0).0, 1);
            }
        }
    }

    struct Console(HPCON);
    impl Drop for Console {
        fn drop(&mut self) {
            unsafe { ClosePseudoConsole(self.0) };
        }
    }

    fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
        text.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn fail<E: std::fmt::Debug>(step: &str) -> impl FnOnce(E) -> PollError + '_ {
        move |error| {
            crate::diagnose::log(format!("Qoder probe: {step} failed: {error:?}"));
            PollError::RequestFailed
        }
    }

    fn write_all(input: &Owned, bytes: &[u8]) -> Result<(), PollError> {
        let mut written = 0u32;
        unsafe { WriteFile(input.0, Some(bytes), Some(&mut written), None) }
            .map_err(fail("console write"))
    }

    pub(super) fn run_usage(exe: &Path, workdir: &Path) -> Result<String, PollError> {
        let deadline = Instant::now() + PROBE_TIMEOUT;
        unsafe {
            let job = CreateJobObjectW(None, PCWSTR::null()).map_err(fail("job"))?;
            let job = Job(Owned(job));
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                (job.0).0,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .map_err(fail("job limits"))?;

            let (mut in_read, mut in_write) = (HANDLE::default(), HANDLE::default());
            CreatePipe(&mut in_read, &mut in_write, None, 0).map_err(fail("input pipe"))?;
            let (in_read, in_write) = (Owned(in_read), Owned(in_write));
            let (mut out_read, mut out_write) = (HANDLE::default(), HANDLE::default());
            CreatePipe(&mut out_read, &mut out_write, None, 0).map_err(fail("output pipe"))?;
            let (out_read, out_write) = (Owned(out_read), Owned(out_write));

            let console = CreatePseudoConsole(COORD { X: 160, Y: 50 }, in_read.0, out_write.0, 0)
                .map_err(fail("pseudo console"))?;
            let console = Console(console);
            // The console holds its own references now.
            drop(in_read);
            drop(out_write);

            let mut size = 0usize;
            let _ = InitializeProcThreadAttributeList(None, 1, None, &mut size);
            let mut attribute_buffer = vec![0u8; size];
            let attributes =
                LPPROC_THREAD_ATTRIBUTE_LIST(attribute_buffer.as_mut_ptr() as *mut c_void);
            InitializeProcThreadAttributeList(Some(attributes), 1, None, &mut size)
                .map_err(fail("attribute list"))?;
            let attribute_result = UpdateProcThreadAttribute(
                attributes,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                Some(console.0 .0 as *const c_void),
                std::mem::size_of::<HPCON>(),
                None,
                None,
            );
            if let Err(error) = attribute_result {
                DeleteProcThreadAttributeList(attributes);
                return Err(fail("pseudo console attribute")(error));
            }

            let mut startup = STARTUPINFOEXW::default();
            startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            startup.lpAttributeList = attributes;
            // Without this a console child copies the parent's redirected
            // std handles and writes past the pseudo console.
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            let mut command =
                wide(format!("\"{}\" -w \"{}\"", exe.display(), workdir.display()).as_ref());
            let application = wide(exe.as_os_str());
            let directory = wide(workdir.as_os_str());
            let mut process = PROCESS_INFORMATION::default();
            let created = CreateProcessW(
                PCWSTR(application.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                false,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
                None,
                PCWSTR(directory.as_ptr()),
                &startup.StartupInfo,
                &mut process,
            );
            DeleteProcThreadAttributeList(attributes);
            created.map_err(fail("process start"))?;
            let process_handle = Owned(process.hProcess);
            let thread_handle = Owned(process.hThread);
            if let Err(error) = AssignProcessToJobObject((job.0).0, process_handle.0) {
                let _ = windows::Win32::System::Threading::TerminateProcess(process_handle.0, 1);
                return Err(fail("job assignment")(error));
            }
            ResumeThread(thread_handle.0);
            drop(thread_handle);

            // Blocking pipe reads stay on their own thread; it ends once the
            // pseudo console closes the other end.
            let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
            let reader_buffer = Arc::clone(&buffer);
            let reader = std::thread::spawn(move || {
                let out_read = out_read;
                let mut chunk = vec![0u8; 65536];
                loop {
                    let mut read = 0u32;
                    let ok = ReadFile(out_read.0, Some(&mut chunk), Some(&mut read), None);
                    if ok.is_err() || read == 0 {
                        break;
                    }
                    reader_buffer
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .extend_from_slice(&chunk[..read as usize]);
                }
            });

            let result = drive(&buffer, &in_write, deadline);

            // Kill the tree first, then close the console (which unblocks
            // the reader); the reader is joined only briefly.
            drop(job);
            drop(in_write);
            drop(console);
            drop(process_handle);
            let join_deadline = Instant::now() + Duration::from_secs(2);
            while !reader.is_finished() && Instant::now() < join_deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if reader.is_finished() {
                let _ = reader.join();
            }
            result
        }
    }

    fn wait_until(deadline: Instant, mut ready: impl FnMut() -> bool) -> bool {
        while Instant::now() < deadline {
            if ready() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        ready()
    }

    fn drive(
        buffer: &Mutex<Vec<u8>>,
        input: &Owned,
        deadline: Instant,
    ) -> Result<String, PollError> {
        // The prompt shows a `>` or the hint line; boot takes ~8 s.
        let boot_deadline = deadline.min(Instant::now() + BOOT_TIMEOUT);
        let booted = wait_until(boot_deadline, || {
            screen_has(buffer, 0, ">") || screen_has(buffer, 0, "try")
        });
        if !booted {
            crate::diagnose::log("Qoder probe: CLI prompt did not appear");
            return Err(PollError::RequestFailed);
        }
        std::thread::sleep(Duration::from_secs(2));
        let mark = buffer.lock().unwrap_or_else(|p| p.into_inner()).len();
        write_all(input, b"/usage")?;
        std::thread::sleep(Duration::from_millis(800));
        write_all(input, b"\r")?;
        if !wait_until(deadline, || screen_has(buffer, mark, "remaining")) {
            crate::diagnose::log("Qoder probe: /usage panel did not appear");
            return Err(PollError::UnexpectedResponse);
        }
        // Let the rest of the panel finish drawing.
        std::thread::sleep(Duration::from_millis(1500));
        let bytes = buffer.lock().unwrap_or_else(|p| p.into_inner());
        Ok(String::from_utf8_lossy(&bytes[mark.min(bytes.len())..]).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Text captured from qodercli 1.1.87 after ANSI stripping, where cursor
    /// moves swallowed the spaces.
    const CAPTURED: &str = "\u{1b}[2J\u{1b}[1;1H Qoder CLI \u{b7} Usage  Status\r\n\
        APIquotaandsessiontokenusageforthecurrentconversation.\r\n\
        \u{1b}[1mQoderPlan:\u{1b}[0mProTrial\r\n\
        PlanExpiresAt:Nov25,2026,21:35:12GMT+2\r\n\
        PlanCreditsUsed:25/300\r\n\
        Add-onCreditsUsed:0/100\r\n\
        OrgResourcePackage:N/A\r\n\
        DedicatedResourcePackages\r\n\
        UltimateExclusiveCredits22/500credits\r\n\
        Available\u{b7}478remaining\u{b7}ExpiresNov14,2026\r\n\
        UltimateExclusiveCredits:usedfirstwhenyouselecttheUltimatetier.\r\n";

    fn unix(time: SystemTime) -> u64 {
        time.duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    #[test]
    fn captured_usage_screen_is_parsed() {
        let usage = parse_usage_screen(CAPTURED).unwrap();
        assert_eq!(usage.plan.as_deref(), Some("ProTrial"));
        assert_eq!(usage.plan_credits, Some((25.0, 300.0)));
        assert_eq!(usage.add_on_credits, Some((0.0, 100.0)));
        assert_eq!(usage.ultimate_credits, Some((22.0, 500.0)));
        // 2026-11-25 21:35:12 GMT+2 = 19:35:12 UTC.
        assert_eq!(unix(usage.plan_expires_at.unwrap()), 1_795_635_312);
        // 2026-11-14 00:00 UTC.
        assert_eq!(unix(usage.ultimate_expires_at.unwrap()), 1_794_614_400);
    }

    /// Live capture: the panel arrives without any line breaks.
    const CAPTURED_JOINED: &str = "/usage \u{276f} usage   Show usage statistics > /usage \r\nLoading...\r\n\
        Qoder CLI \u{b7} Usage  Status \r\n\u{2500}\u{2500}APIquotaandsessiontokenusageforthecurrentconversation.\
        QoderPlan:ProTrialPlanExpiresAt:Nov25,2026,21:35:12GMT+2PlanCreditsUsed:25/300\
        Add-onCreditsUsed:0/100OrgResourcePackage:N/ATotalDuration(API):0.0sTotalDuration(wall):3.5s\
        TotalCodeChanges:0linesadded,0linesremovedDedicatedResourcePackages\
        UltimateExclusiveCredits22/500creditsAvailable\u{b7}478remaining\u{b7}ExpiresNov14,2026\
        UltimateExclusiveCredits:usedfirstwhenyouselecttheUltimatetierinthemodelselector.\
        PromotionsUltimateTrialPack/claimTab/\u{2190}\u{2192}switch\u{b7}Escback";

    #[test]
    fn joined_live_screen_parses_like_the_line_capture() {
        assert_eq!(
            parse_usage_screen(CAPTURED_JOINED),
            parse_usage_screen(CAPTURED)
        );
    }

    #[test]
    fn spaced_text_parses_the_same() {
        let spaced = "Qoder Plan: Pro Trial\nPlan Expires At: Nov 25, 2026, 21:35:12 GMT+2\n\
            Plan Credits Used: 25 / 300\nAdd-on Credits Used: 0 / 100\n\
            Ultimate Exclusive Credits 22 / 500 credits\n\
            Available \u{b7} 478 remaining \u{b7} Expires Nov 14, 2026\n";
        assert_eq!(parse_usage_screen(spaced), parse_usage_screen(CAPTURED));
    }

    #[test]
    fn captured_screen_maps_to_cache_shape() {
        let data = usage_data(&parse_usage_screen(CAPTURED).unwrap()).unwrap();
        let monthly = data.monthly.as_ref().unwrap();
        assert!(monthly.available);
        assert!((monthly.percentage - 25.0 / 3.0).abs() < 1e-9);
        assert_eq!(unix(monthly.resets_at.unwrap()), 1_795_635_312);
        assert_eq!(data.weekly, *monthly);
        assert_eq!(data.weekly_label.as_deref(), Some("Plan"));
        assert!(!data.session.available);
        let credits = data.credits.as_ref().unwrap();
        assert_eq!(
            (credits.percentage, credits.remaining, credits.total),
            (0.0, 100.0, 100.0)
        );
        assert_eq!(data.plan_name.as_deref(), Some("ProTrial"));
        let keys: Vec<&str> = data.credit_pools.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, ["plan", "add_on", "ultimate_exclusive"]);
        let ultimate = &data.credit_pools[2];
        assert_eq!((ultimate.used, ultimate.total), (22.0, 500.0));
        assert!((data.limits[0].usage.percentage - 4.4).abs() < 1e-9);

        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(
            json["monthly"]["resets_at"]["secs_since_epoch"],
            1_795_635_312
        );
        assert_eq!(json["credit_pools"][2]["used"], 22.0);
    }

    #[test]
    fn unrelated_or_broken_screen_yields_nothing() {
        assert_eq!(
            parse_usage_screen("Loading...\r\nError: not logged in\r\n"),
            None
        );
        let broken = "PlanCreditsUsed:abc/300\nPlanExpiresAt:Foo99,2026\n";
        assert_eq!(parse_usage_screen(broken), None);
    }

    #[test]
    fn partial_screen_keeps_known_fields_and_unknowns_stay_empty() {
        let usage = parse_usage_screen("PlanCreditsUsed:300/300\nPlanExpiresAt:garbage\n").unwrap();
        assert_eq!(usage.plan_expires_at, None);
        let data = usage_data(&usage).unwrap();
        assert_eq!(data.weekly.percentage, 100.0);
        assert_eq!(data.weekly.resets_at, None);
        assert!(data.credits.is_none());
        assert!(data.limits.is_empty());
    }

    #[test]
    fn plan_name_alone_is_not_usage() {
        let usage = parse_usage_screen("QoderPlan:Free\n").unwrap();
        assert!(usage_data(&usage).is_none());
    }
}

#[cfg(test)]
mod live {
    /// Runs the real CLI: `cargo test qoder_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn qoder_live_probe() {
        let exe = super::qodercli_path().expect("qodercli installed");
        let screen = super::probe(&exe).expect("probe");
        let usage = super::parse_usage_screen(&screen).expect("parsed");
        println!("{usage:#?}");
        let data = super::usage_data(&usage).expect("usage data");
        println!("{}", serde_json::to_string_pretty(&data).unwrap());
    }
}
