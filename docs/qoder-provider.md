# Qoder provider

Qoder has no plain HTTP usage API (quota requests are WASM-signed, responses
encrypted), so the monitor reads the CLI's own `/usage` panel.

## How a probe works

`src/poller/qoder.rs`:

1. Find `%USERPROFILE%\.qoder\bin\qodercli\qodercli.exe` (not on PATH). Missing
   CLI: the provider counts as off and polls report `NoCredentials`.
2. Start it as `qodercli.exe -w %TEMP%\ClaudeCodeUsageMonitor-qoder` in a
   Windows pseudo console (ConPTY), inside a job object with
   kill-on-close. The output pipe is read on its own thread.
3. Wait for the prompt (up to 20 s), type `/usage`, wait 0.8 s, press Enter,
   wait for `remaining`, wait 1.5 s more, read the screen.
4. Terminate the job (the whole process tree), close the console. The whole
   probe is capped at 30 s. If the monitor exits or crashes mid-probe, Windows
   closes the job handle and kills qodercli too.

A probe takes about 11 s and costs no credits. Probes run at most once every
15 minutes whatever the global interval: at 5 min the other providers poll
every 5 minutes and Qoder returns its last result in between; at 60 min Qoder
follows the 60-minute cycle. A failed probe is also remembered for 15 minutes;
the poller then shows the last good reading marked `stale`.

## Parsing

Cursor moves remove spaces and line breaks, so the panel is read as one
whitespace-free string and each value runs from its label to the next known
label (`QoderPlan:`, `PlanExpiresAt:`, `PlanCreditsUsed:`,
`Add-onCreditsUsed:`, `UltimateExclusiveCredits`, `… remaining · Expires …`).
A field that does not parse stays unknown; nothing panics.

## Cache shape (`usage-cache.json` → `data.qoder`)

| Field | Meaning |
|---|---|
| `weekly` | Plan Credits: `percentage` used, `resets_at` = plan expiry. `weekly_label` = `"Plan"`. Widgets and tray read this. |
| `monthly` | Same as `weekly`, for `limits.py`, which reads `monthly` like the OpenCode entry. |
| `session` | `available: false` (Qoder has no short window). |
| `credits` | Add-on credits: `percentage`, `remaining`, `total`. |
| `limits[]` | `ultimate_exclusive`: Ultimate Exclusive Credits as a percentage with its expiry. |
| `plan_name` | e.g. `"ProTrial"`. |
| `credit_pools[]` | Absolute figures: `{key, label, used, total, expires_at?}` for `plan`, `add_on`, `ultimate_exclusive`. |

Example (real probe, 2026-11-12):

```json
"qoder": {
  "session": {"available": false, "percentage": null, "resets_at": null},
  "weekly":  {"available": true, "percentage": 8.33, "resets_at": {"secs_since_epoch": 1795635312, "nanos_since_epoch": 0}},
  "weekly_label": "Plan",
  "monthly": {"available": true, "percentage": 8.33, "resets_at": {"secs_since_epoch": 1795635312, "nanos_since_epoch": 0}},
  "credits": {"percentage": 0.0, "remaining": 100.0, "total": 100.0},
  "limits": [{"key": "ultimate_exclusive", "kind": "credits", "usage": {"available": true, "percentage": 6.2, "resets_at": {"secs_since_epoch": 1794614400, "nanos_since_epoch": 0}}}],
  "plan_name": "ProTrial",
  "credit_pools": [
    {"key": "plan", "label": "Plan Credits", "used": 25.0, "total": 300.0, "expires_at": {"secs_since_epoch": 1795635312, "nanos_since_epoch": 0}},
    {"key": "add_on", "label": "Add-on Credits", "used": 0.0, "total": 100.0},
    {"key": "ultimate_exclusive", "label": "Ultimate Exclusive Credits", "used": 31.0, "total": 500.0, "expires_at": {"secs_since_epoch": 1794614400, "nanos_since_epoch": 0}}
  ]
}
```

`limits.py` caveat: its `window()` assumes a 30-day `monthly` span for pace.
The Qoder plan period is the plan term, so pace from `limits.py` is only a
rough figure; `percentage` and `resets_at` are exact.

Live check: `cargo test --locked qoder_live -- --ignored --nocapture` runs a
real probe and prints the parsed data.
