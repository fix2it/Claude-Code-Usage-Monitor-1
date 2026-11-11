use super::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_NONCLIENT, TME_QUERY};

pub(super) fn surface_index_for_window(state: &AppState, hwnd: HWND) -> Option<usize> {
    if state.hwnd.to_hwnd() == hwnd {
        return Some(0);
    }
    if let Some(index) = state
        .mirror_hwnds
        .iter()
        .position(|window| window.to_hwnd() == hwnd)
    {
        return Some(index + 1);
    }
    state
        .desktop_hwnds
        .iter()
        .position(|window| window.is_some_and(|window| window.to_hwnd() == hwnd))
}

pub(super) fn mouse_client_point(lparam: LPARAM) -> (f64, f64) {
    let packed = lparam.0 as u32;
    let x = (packed as u16 as i16) as f64;
    let y = ((packed >> 16) as u16 as i16) as f64;
    (x, y)
}

pub(super) fn mouse_target_at(hwnd: HWND, lparam: LPARAM) -> Option<(usize, String)> {
    let state = lock_state();
    let state = state.as_ref()?;
    let surface_index = surface_index_for_window(state, hwnd)?;
    let theme = effective_theme_from_state(state)?;
    let scale = theme_surface_scale(&theme, surface_index).max(0.01);
    let runtime = theme_runtime_for_surface(&theme, surface_index, theme_runtime_from_state(state));
    let (x, y) = mouse_client_point(lparam);
    let object_id = theme_engine::hit_test_mouse_event(
        &theme,
        surface_index,
        x / scale,
        y / scale,
        state.data.as_ref(),
        runtime,
    )?;
    Some((surface_index, object_id))
}

pub(super) fn mouse_handler_exists(
    surface_index: usize,
    object_id: &str,
    event: MouseEventKind,
) -> bool {
    let state = lock_state();
    state.as_ref().is_some_and(|state| {
        state.active_theme.as_ref().is_some_and(|theme| {
            theme_engine::mouse_event_script(theme, surface_index, object_id, event).is_some()
        })
    })
}

pub(super) fn dispatch_mouse_event(
    surface_index: usize,
    object_id: &str,
    event: MouseEventKind,
) -> bool {
    let source = {
        let state = lock_state();
        let Some(state) = state.as_ref() else {
            return false;
        };
        let Some(theme) = state.active_theme.as_ref() else {
            return false;
        };
        let Some(source) = theme_engine::mouse_event_script(theme, surface_index, object_id, event)
            .map(str::to_string)
        else {
            return false;
        };
        source
    };
    execute_mouse_action_source(surface_index, object_id, &source)
}

pub(super) fn execute_mouse_action_source(
    surface_index: usize,
    object_id: &str,
    source: &str,
) -> bool {
    let result = {
        let mut state = lock_state();
        let Some(state) = state.as_mut() else {
            return false;
        };
        let Some(theme) = state.active_theme.clone() else {
            return false;
        };
        let data = state.data.clone();
        let runtime =
            theme_runtime_for_surface(&theme, surface_index, theme_runtime_from_state(state));
        theme_engine::execute_mouse_actions(
            &theme,
            surface_index,
            object_id,
            source,
            data.as_ref(),
            runtime,
            &mut state.mouse_action_overrides,
        )
    };
    match result {
        Ok(effects) => {
            render_layered();
            let owner = lock_state()
                .as_ref()
                .map(|state| state.hwnd.to_hwnd())
                .unwrap_or_default();
            sync_tray_icon(owner);
            for effect in effects {
                match effect {
                    MouseActionEffect::ShowDashboard => {
                        crate::dashboard::show(controller_for(owner))
                    }
                    MouseActionEffect::ToggleDashboard => {
                        crate::dashboard::toggle(controller_for(owner))
                    }
                    MouseActionEffect::OpenUrl(url) => {
                        open_web_url(owner, &url, "mouse action URL could not be opened")
                    }
                    MouseActionEffect::ShowContextMenu(menu) => show_context_menu_document(
                        owner,
                        menu.as_deref(),
                        Some((surface_index, object_id.to_string())),
                    ),
                }
            }
            true
        }
        Err(error) => {
            diagnose::log(format!(
                "mouse action failed surface={surface_index} layer={object_id}: {error}"
            ));
            false
        }
    }
}

pub(super) fn schedule_or_dispatch_click(hwnd: HWND, surface_index: usize, object_id: String) {
    let has_click = mouse_handler_exists(surface_index, &object_id, MouseEventKind::Click);
    if !has_click {
        return;
    }
    if mouse_handler_exists(surface_index, &object_id, MouseEventKind::DoubleClick) {
        let owner = {
            let mut state = lock_state();
            let Some(state) = state.as_mut() else {
                return;
            };
            state.pending_mouse_click = Some(PendingMouseClick {
                surface_index,
                object_id,
            });
            state.hwnd.to_hwnd()
        };
        unsafe {
            let _ = kill_controller_timer(Some(owner), TIMER_MOUSE_CLICK);
            set_controller_timer(
                Some(owner),
                TIMER_MOUSE_CLICK,
                GetDoubleClickTime().max(1),
                None,
            );
        }
    } else {
        let _ = dispatch_mouse_event(surface_index, &object_id, MouseEventKind::Click);
    }
    let _ = hwnd;
}

pub(super) fn dispatch_double_click(hwnd: HWND, surface_index: usize, object_id: String) {
    if !mouse_handler_exists(surface_index, &object_id, MouseEventKind::DoubleClick) {
        return;
    }
    let owner = {
        let mut state = lock_state();
        let Some(state) = state.as_mut() else {
            return;
        };
        state.pending_mouse_click = None;
        state.suppress_next_left_up = true;
        state.hwnd.to_hwnd()
    };
    unsafe {
        let _ = kill_controller_timer(Some(owner), TIMER_MOUSE_CLICK);
    }
    let _ = dispatch_mouse_event(surface_index, &object_id, MouseEventKind::DoubleClick);
    let _ = hwnd;
}

/// A theme-authored `click` action keeps its old routing (including the
/// double-click delay). Only a click over an object with no click action
/// bounces the global view mode; for such objects the wheel is the sole mode
/// input. Keeping this decision pure makes the priority rule testable.
pub(super) fn click_changes_view_mode(has_click_handler: bool) -> bool {
    !has_click_handler
}

/// Route a left-button release that did not start a drag. Both the primary
/// surface and every mirror/desktop surface call this, so a click on any
/// widget advances the ONE global view mode. Outside the widget the common
/// hit-test returns `None` and nothing changes.
pub(super) fn handle_view_mode_click(hwnd: HWND, lparam: LPARAM) {
    let Some((surface, object)) = mouse_target_at(hwnd, lparam) else {
        return;
    };
    let has_click = mouse_handler_exists(surface, &object, MouseEventKind::Click);
    if click_changes_view_mode(has_click) {
        advance_view_mode_by_click(hwnd);
    } else {
        schedule_or_dispatch_click(hwnd, surface, object);
    }
}

/// A click bounces `1 -> 2 -> 3 -> 2 -> 1` from the stored direction. It is
/// handled directly in `WM_LBUTTONUP`, so there is no `GetDoubleClickTime()`
/// wait before the mode changes.
pub(super) fn advance_view_mode_by_click(hwnd: HWND) {
    let next = {
        let state = lock_state();
        state
            .as_ref()
            .map(|state| super::next_view_mode(state.view_mode, state.mode_direction))
    };
    if let Some((mode, direction)) = next {
        super::apply_view_mode(hwnd, mode, direction);
    }
}

/// Win32 `WHEEL_DELTA`: one physical wheel notch is 120 units. High-resolution
/// wheels split that notch into several smaller `WM_MOUSEWHEEL` messages.
const WHEEL_DELTA: i32 = 120;

/// Accumulates partial wheel deltas so one physical notch advances the view
/// mode exactly once. The state is global, like the view mode itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct WheelAccumulator {
    remainder: i32,
}

impl WheelAccumulator {
    /// A partial remainder expires after this gap, so a stale scroll cannot
    /// combine with a later gesture.
    const RESET_AFTER: Duration = Duration::from_millis(250);

    /// Fold one wheel message. Returns true when the message contains at least
    /// one complete notch. A direction reversal or an expired remainder drops
    /// the partial delta. All complete notches in the message are consumed, so
    /// one message advances at most one mode and cannot run away.
    pub(super) fn push(&mut self, delta: i32, elapsed: Duration) -> bool {
        if delta == 0 {
            return false;
        }
        if elapsed >= Self::RESET_AFTER || delta.signum() != self.remainder.signum() {
            self.remainder = 0;
        }
        self.remainder += delta;
        let notches = self.remainder.abs() / WHEEL_DELTA;
        if notches == 0 {
            return false;
        }
        self.remainder -= self.remainder.signum() * notches * WHEEL_DELTA;
        true
    }
}

static WHEEL_ACCUM: OnceLock<Mutex<(WheelAccumulator, Option<Instant>)>> = OnceLock::new();

fn wheel_notch_ready(delta: i32) -> bool {
    let cache = WHEEL_ACCUM.get_or_init(|| Mutex::new((WheelAccumulator::default(), None)));
    let mut entry = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = Instant::now();
    let elapsed = entry
        .1
        .map(|last| now.duration_since(last))
        .unwrap_or(WheelAccumulator::RESET_AFTER);
    entry.1 = Some(now);
    entry.0.push(delta, elapsed)
}

/// Wheel down (negative delta) drives towards mode 3, wheel up towards mode 1.
/// The wheel sets the direction explicitly and is ignored at a boundary. It is
/// gated on the cursor being over the widget via the common hit-test, so a
/// wheel delivered to this window while the pointer is elsewhere is ignored.
pub(super) fn handle_mouse_wheel(hwnd: HWND, wparam: WPARAM) {
    let delta = ((wparam.0 >> 16) & 0xffff) as u16 as i16;
    if delta == 0 || !cursor_over_widget(hwnd) {
        return;
    }
    // Partial deltas from a high-resolution wheel must not each advance a mode;
    // only a complete accumulated notch may switch the view.
    if !wheel_notch_ready(i32::from(delta)) {
        return;
    }
    let toward: i8 = if delta > 0 { -1 } else { 1 };
    let next = {
        let state = lock_state();
        state
            .as_ref()
            .and_then(|state| super::wheel_view_mode(state.view_mode, toward))
    };
    if let Some((mode, direction)) = next {
        super::apply_view_mode(hwnd, mode, direction);
    }
}

/// Rebuild a client-space `LPARAM` from the live cursor. `WM_MOUSEWHEEL`
/// carries its position in screen coordinates, so its own `lParam` cannot be
/// handed to `mouse_target_at`; the cursor position is authoritative anyway.
fn cursor_client_lparam(hwnd: HWND) -> Option<LPARAM> {
    let mut point = POINT::default();
    if unsafe { GetCursorPos(&mut point) }.is_err() {
        return None;
    }
    let mut client = [point];
    unsafe { MapWindowPoints(None, Some(hwnd), &mut client) };
    let packed = ((client[0].y as u32 & 0xffff) << 16) | (client[0].x as u32 & 0xffff);
    Some(LPARAM(packed as isize))
}

/// The root surface carries a non-empty `mouse_events` (the right-click menu),
/// so a hit-test returning any object — including `"main"` — means the cursor
/// is over the widget. That single fact is enough to gate a global switch.
fn cursor_over_widget(hwnd: HWND) -> bool {
    let Some(lparam) = cursor_client_lparam(hwnd) else {
        return false;
    };
    mouse_target_at(hwnd, lparam).is_some()
}

/// A view-mode transition keeps the widget's right screen edge pinned and
/// moves its left edge, so a stationary cursor can end up outside the window.
/// Win32 does not post `WM_MOUSELEAVE` when the window itself moves away, which
/// would leave the tooltip anchor and the four hover crossfades frozen until
/// the next physical mouse move. Re-evaluate hover against the settled
/// geometry: tear the stale state down when the cursor left, or re-arm
/// tracking and recompute the provider hover when it stayed over the widget.
pub(super) fn revalidate_mouse_hover(hwnd: HWND) {
    reset_mouse_move_throttle();
    let Some(lparam) = cursor_client_lparam(hwnd) else {
        clear_mouse_hover(hwnd);
        return;
    };
    if mouse_target_at(hwnd, lparam).is_some() {
        update_mouse_hover(hwnd, lparam);
    } else {
        clear_mouse_hover(hwnd);
    }
}

/// Theme object whose hover reveals the Antigravity Claude/GPT pool.
pub(super) const ANTIGRAVITY_HOVER_OBJECT: &str = "antigravity-provider";
pub(super) const CLAUDE_HOVER_OBJECT: &str = "claude-provider";
pub(super) const CODEX_HOVER_OBJECT: &str = "codex-provider";
pub(super) const OPENCODE_HOVER_OBJECT: &str = "opencode-provider";
pub(super) const QODER_HOVER_OBJECT: &str = "qoder-provider";

fn tile_hover_at(
    state: &super::AppState,
    hwnd: HWND,
    lparam: LPARAM,
    rect: Option<(f64, f64, f64, f64)>,
) -> bool {
    if surface_index_for_window(state, hwnd) != Some(0) {
        return false;
    }
    let Some(rect) = rect else {
        return false;
    };
    let Some(theme) = state.active_theme.as_ref() else {
        return false;
    };
    let scale = theme_surface_scale(theme, 0).max(0.01);
    let (x, y) = mouse_client_point(lparam);
    let lx = x / scale;
    let ly = y / scale;
    lx >= rect.0 && lx < rect.0 + rect.2 && ly >= rect.1 && ly < rect.1 + rect.3
}

/// A duplicate `WM_MOUSEMOVE` at the same client point cannot change any
/// hover state, but it used to re-run four hit-tests, a `TME_QUERY` and up to
/// four `mouse_target_at` passes. Coalescing them keeps the message queue
/// short so animation ticks arrive on time. `WM_MOUSELEAVE` clears the cache.
static LAST_MOUSE_MOVE: OnceLock<Mutex<Option<(Instant, isize)>>> = OnceLock::new();

pub(super) fn reset_mouse_move_throttle() {
    let cache = LAST_MOUSE_MOVE.get_or_init(|| Mutex::new(None));
    *cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

/// `now` is injected by the caller so the coalescing window is unit-testable
/// without depending on wall-clock timing.
fn duplicate_mouse_move(lparam: LPARAM, now: Instant) -> bool {
    let cache = LAST_MOUSE_MOVE.get_or_init(|| Mutex::new(None));
    let mut last = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let duplicate = last.as_ref().is_some_and(|(at, point)| {
        *point == lparam.0 && now.duration_since(*at) < Duration::from_millis(8)
    });
    *last = Some((now, lparam.0));
    duplicate
}

pub(super) fn update_mouse_hover(hwnd: HWND, lparam: LPARAM) {
    if duplicate_mouse_move(lparam, Instant::now()) {
        return;
    }
    track_mouse_hover_with(hwnd, widget_tooltip::visible_for(hwnd), |tracking| unsafe {
        TrackMouseEvent(tracking)
    });
    let (antigravity_hovered, claude_hovered, codex_hovered, opencode_hovered, qoder_hovered) = {
        let state = lock_state();
        if let Some(state) = state.as_ref() {
            (
                tile_hover_at(state, hwnd, lparam, state.antigravity_tile_rect),
                tile_hover_at(state, hwnd, lparam, state.claude_tile_rect),
                tile_hover_at(state, hwnd, lparam, state.codex_tile_rect),
                tile_hover_at(state, hwnd, lparam, state.opencode_tile_rect),
                tile_hover_at(state, hwnd, lparam, state.qoder_tile_rect),
            )
        } else {
            (false, false, false, false, false)
        }
    };
    set_antigravity_hover(hwnd, antigravity_hovered);
    super::set_claude_hover(hwnd, claude_hovered);
    super::set_codex_hover(hwnd, codex_hovered);
    // `mouse_target_at` only returns objects with non-empty `mouse_events`,
    // and the provider wrappers have none, so the OpenCode tile is detected
    // with the same cached `object_bounds` rect as the other three providers.
    super::set_opencode_hover(hwnd, opencode_hovered);
    super::set_qoder_hover(hwnd, qoder_hovered);
    // No tick here: the animation timer drives every frame. Ticking on mouse
    // move used to produce a second full render right after `set_*_hover`
    // already rendered the first frame, and its 15 ms throttle could swallow
    // real timer ticks.
    for _ in 0..4 {
        let target = mouse_target_at(hwnd, lparam);
        let previous = {
            let mut state = lock_state();
            let Some(state) = state.as_mut() else {
                return;
            };
            if state.hovered_mouse_layer == target {
                return;
            }
            std::mem::replace(&mut state.hovered_mouse_layer, target.clone())
        };
        if let Some((surface, object)) = previous {
            let _ = dispatch_mouse_event(surface, &object, MouseEventKind::MouseLeave);
        }
        if let Some((surface, object)) = &target {
            let _ = dispatch_mouse_event(*surface, object, MouseEventKind::MouseEnter);
        }
    }
    diagnose::log("mouse enter/leave actions did not stabilize after four transitions");
    if let Some(state) = lock_state().as_mut() {
        state.hovered_mouse_layer = None;
    }
}

fn track_mouse_hover_with(
    hwnd: HWND,
    tooltip_visible: bool,
    mut track: impl FnMut(&mut TRACKMOUSEEVENT) -> windows::core::Result<()>,
) {
    let mut tracking = TRACKMOUSEEVENT {
        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_QUERY,
        ..Default::default()
    };
    let active = if track(&mut tracking).is_ok()
        && tracking.hwndTrack == hwnd
        && !tracking.dwFlags.contains(TME_NONCLIENT)
    {
        tracking.dwFlags
    } else {
        Default::default()
    };
    let desired = if tooltip_visible {
        TME_LEAVE
    } else {
        TME_LEAVE | TME_HOVER
    };
    let missing = desired & !active;
    if missing.0 != 0 {
        // Re-requesting TME_HOVER resets its timer, even for synthetic mouse
        // moves caused by repainting/repositioning a stationary widget. Let
        // Windows keep the hover rectangle and deadline until hover or leave.
        tracking.dwFlags = missing;
        tracking.hwndTrack = hwnd;
        tracking.dwHoverTime = HOVER_DEFAULT;
        let _ = track(&mut tracking);
    }
}

pub(super) fn clear_mouse_hover(hwnd: HWND) {
    reset_mouse_move_throttle();
    widget_tooltip::hide_for(hwnd);
    set_antigravity_hover(hwnd, false);
    super::set_claude_hover(hwnd, false);
    super::set_codex_hover(hwnd, false);
    super::set_opencode_hover(hwnd, false);
    super::set_qoder_hover(hwnd, false);
    let previous = {
        let mut state = lock_state();
        let Some(state) = state.as_mut() else {
            return;
        };
        let surface = surface_index_for_window(state, hwnd);
        if state
            .hovered_mouse_layer
            .as_ref()
            .is_none_or(|(hovered, _)| Some(*hovered) != surface)
        {
            return;
        }
        state.hovered_mouse_layer.take()
    };
    if let Some((surface, object)) = previous {
        let _ = dispatch_mouse_event(surface, &object, MouseEventKind::MouseLeave);
    }
}

pub(super) fn update_tray_mouse_hover(hwnd: HWND, surface_index: usize, object_id: String) {
    let target = Some((surface_index, object_id));
    let previous = {
        let mut state = lock_state();
        let Some(state) = state.as_mut() else {
            return;
        };
        if state.hovered_mouse_layer == target {
            unsafe {
                set_controller_timer(Some(hwnd), TIMER_TRAY_HOVER, 100, None);
            }
            return;
        }
        std::mem::replace(&mut state.hovered_mouse_layer, target.clone())
    };
    if let Some((surface, object)) = previous {
        let _ = dispatch_mouse_event(surface, &object, MouseEventKind::MouseLeave);
    }
    if let Some((surface, object)) = target {
        let _ = dispatch_mouse_event(surface, &object, MouseEventKind::MouseEnter);
    }
    unsafe {
        set_controller_timer(Some(hwnd), TIMER_TRAY_HOVER, 100, None);
    }
}

pub(super) fn clear_tray_mouse_hover_if_left(hwnd: HWND) {
    let hovered = {
        let state = lock_state();
        let Some(state) = state.as_ref() else {
            return;
        };
        state.hovered_mouse_layer.clone().map(|target| {
            let is_tray = state.active_theme.as_ref().is_some_and(|theme| {
                theme.surfaces.get(target.0).is_some_and(|surface| {
                    surface
                        .placement
                        .nest
                        .resolve(surface.placement.reference.region)
                        == SurfaceNest::TrayIcon
                })
            });
            (target, is_tray)
        })
    };
    let Some((target, true)) = hovered else {
        unsafe {
            let _ = kill_controller_timer(Some(hwnd), TIMER_TRAY_HOVER);
        }
        return;
    };
    if tray_icon::cursor_over_themed_icon(controller_for(hwnd), target.0) {
        return;
    }
    let previous = lock_state().as_mut().and_then(|state| {
        (state.hovered_mouse_layer.as_ref() == Some(&target))
            .then(|| state.hovered_mouse_layer.take())
            .flatten()
    });
    unsafe {
        let _ = kill_controller_timer(Some(hwnd), TIMER_TRAY_HOVER);
    }
    if let Some((surface, object)) = previous {
        let _ = dispatch_mouse_event(surface, &object, MouseEventKind::MouseLeave);
    }
}

/// Answer WM_SETCURSOR without DefWindowProc. For a child surface it first
/// sends WM_SETCURSOR to the parent, which for a taskbar-hosted surface is a
/// synchronous call into Explorer on every mouse move.
pub(super) unsafe fn set_surface_cursor(hwnd: HWND) {
    let cursor = if surface_cursor_is_clickable(hwnd) {
        IDC_HAND
    } else {
        IDC_ARROW
    };
    SetCursor(Some(LoadCursorW(None, cursor).unwrap_or_default()));
}

unsafe fn surface_cursor_is_clickable(hwnd: HWND) -> bool {
    let Some(lparam) = cursor_client_lparam(hwnd) else {
        return false;
    };
    let Some((surface, object)) = mouse_target_at(hwnd, lparam) else {
        return false;
    };
    [
        MouseEventKind::Click,
        MouseEventKind::DoubleClick,
        MouseEventKind::RightClick,
    ]
    .into_iter()
    .any(|event| mouse_handler_exists(surface, &object, event))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_click_actions_keep_priority_over_view_mode_switching() {
        assert!(
            click_changes_view_mode(false),
            "a plain click over a widget with no click action must bounce the mode"
        );
        assert!(
            !click_changes_view_mode(true),
            "an authored JSON click keeps its old routing; only the wheel changes the mode"
        );
    }

    #[test]
    fn a_standard_wheel_notch_advances_exactly_once() {
        let mut up = WheelAccumulator::default();
        assert!(up.push(120, Duration::from_millis(1)));
        assert_eq!(up.remainder, 0);
        assert!(!up.push(0, Duration::from_millis(1)));

        let mut down = WheelAccumulator::default();
        assert!(down.push(-120, Duration::from_millis(1)));
        assert_eq!(down.remainder, 0);
    }

    #[test]
    fn high_resolution_wheel_deltas_accumulate_to_one_notch() {
        let mut acc = WheelAccumulator::default();
        assert!(!acc.push(40, Duration::from_millis(1)));
        assert!(!acc.push(40, Duration::from_millis(1)));
        assert_eq!(acc.remainder, 80);
        assert!(
            acc.push(40, Duration::from_millis(1)),
            "three partial deltas complete exactly one notch"
        );
        assert_eq!(acc.remainder, 0);
        // A second physical notch again needs three partial messages.
        assert!(!acc.push(40, Duration::from_millis(1)));
        assert!(!acc.push(40, Duration::from_millis(1)));
        assert!(acc.push(40, Duration::from_millis(1)));

        // A partial remainder carries within one continuous gesture.
        let mut carry = WheelAccumulator::default();
        assert!(!carry.push(80, Duration::from_millis(1)));
        assert!(carry.push(120, Duration::from_millis(1)));
        assert_eq!(carry.remainder, 80);
        assert!(carry.push(40, Duration::from_millis(1)));
        assert_eq!(carry.remainder, 0);
    }

    #[test]
    fn wheel_accumulator_resets_on_direction_change_and_idle_gap() {
        let mut acc = WheelAccumulator::default();
        assert!(!acc.push(80, Duration::from_millis(1)));
        assert!(
            !acc.push(-40, Duration::from_millis(1)),
            "reversing direction drops the partial remainder"
        );
        assert_eq!(acc.remainder, -40);
        assert!(!acc.push(-40, Duration::from_millis(1)));
        assert!(acc.push(-40, Duration::from_millis(1)));
        assert_eq!(acc.remainder, 0);

        let mut stale = WheelAccumulator::default();
        assert!(!stale.push(80, Duration::from_millis(1)));
        assert!(
            !stale.push(40, Duration::from_millis(300)),
            "a stale partial must not complete a notch"
        );
        assert_eq!(stale.remainder, 40);
    }

    #[test]
    fn one_wheel_message_never_advances_more_than_one_mode() {
        let mut acc = WheelAccumulator::default();
        assert!(
            acc.push(360, Duration::from_millis(1)),
            "three coalesced notches still emit one step"
        );
        assert_eq!(acc.remainder, 0, "all complete notches are consumed");
        assert!(
            !acc.push(40, Duration::from_millis(1)),
            "no leftover notch carries into the next message"
        );
    }

    #[test]
    fn duplicate_mouse_moves_are_coalesced_but_real_moves_are_not() {
        reset_mouse_move_throttle();
        let start = Instant::now();
        let point = LPARAM(0x0001_0002);
        assert!(
            !duplicate_mouse_move(point, start),
            "the first move at a point must be processed"
        );
        assert!(
            duplicate_mouse_move(point, start + Duration::from_millis(1)),
            "a duplicate at the same point within 8 ms must be skipped"
        );
        assert!(
            !duplicate_mouse_move(point, start + Duration::from_millis(9)),
            "the same point after the 8 ms window must be processed"
        );
        let elsewhere = LPARAM(0x0003_0004);
        assert!(
            !duplicate_mouse_move(elsewhere, start + Duration::from_millis(10)),
            "a move to different coordinates must be processed"
        );
        // WM_MOUSELEAVE clears the throttle so re-entry at the same point works.
        reset_mouse_move_throttle();
        assert!(
            !duplicate_mouse_move(point, start + Duration::from_millis(11)),
            "re-entry after a leave must be processed"
        );
    }

    #[test]
    fn repeated_mouse_moves_preserve_the_pending_hover_deadline() {
        let hwnd = HWND(std::ptr::dangling_mut());
        let mut active = TRACKMOUSEEVENT::default();
        let mut deadline = None;
        // Model Win32's documented timer reset on each TME_HOVER request.
        // Repaints can generate mouse moves even with a stationary cursor.
        for elapsed in (0..=500).step_by(50) {
            track_mouse_hover_with(hwnd, false, |tracking| {
                if tracking.dwFlags == TME_QUERY {
                    tracking.hwndTrack = active.hwndTrack;
                    tracking.dwFlags = active.dwFlags;
                } else {
                    assert_eq!(tracking.dwHoverTime, HOVER_DEFAULT);
                    if tracking.dwFlags.contains(TME_HOVER) {
                        deadline = Some(elapsed + 400);
                    }
                    active.hwndTrack = tracking.hwndTrack;
                    active.dwFlags |= tracking.dwFlags;
                }
                Ok(())
            });
        }
        assert_eq!(deadline, Some(400), "mouse moves postponed the hover");
        assert_eq!(active.dwFlags, TME_HOVER | TME_LEAVE);
    }

    #[test]
    fn hover_tracking_rearms_after_hover_leave_and_window_changes() {
        let hwnd = HWND(std::ptr::dangling_mut());
        let mirror = HWND(2 as *mut _);
        for (tracked, active, visible, expected) in [
            // A visible tooltip needs leave notifications, without rearming hover.
            (hwnd, TME_LEAVE, true, None),
            (hwnd, TME_HOVER | TME_LEAVE, true, None),
            // Windows cancels hover after delivering WM_MOUSEHOVER. A dismissed
            // popup can be shown again on a later mouse move.
            (hwnd, TME_LEAVE, false, Some(TME_HOVER)),
            // Adding leave tracking must not reset an existing hover timer.
            (hwnd, TME_HOVER, false, Some(TME_LEAVE)),
            // WM_MOUSELEAVE cancels all tracking; re-entry needs both flags.
            (hwnd, Default::default(), false, Some(TME_HOVER | TME_LEAVE)),
            // Tracking another surface or its nonclient area is not reusable.
            (
                mirror,
                TME_HOVER | TME_LEAVE,
                false,
                Some(TME_HOVER | TME_LEAVE),
            ),
            (
                hwnd,
                TME_NONCLIENT | TME_HOVER | TME_LEAVE,
                false,
                Some(TME_HOVER | TME_LEAVE),
            ),
        ] {
            let mut requests = Vec::new();
            track_mouse_hover_with(hwnd, visible, |tracking| {
                if tracking.dwFlags == TME_QUERY {
                    tracking.hwndTrack = tracked;
                    tracking.dwFlags = active;
                } else {
                    assert_eq!(tracking.hwndTrack, hwnd);
                    requests.push(tracking.dwFlags);
                }
                Ok(())
            });
            assert_eq!(requests, expected.into_iter().collect::<Vec<_>>());
        }
    }

    #[test]
    fn failed_tracking_query_still_arms_hover() {
        let hwnd = HWND(std::ptr::dangling_mut());
        let mut requested = false;
        track_mouse_hover_with(hwnd, false, |tracking| {
            if tracking.dwFlags == TME_QUERY {
                return Err(windows::core::Error::from_hresult(E_FAIL));
            }
            assert_eq!(tracking.hwndTrack, hwnd);
            assert_eq!(tracking.dwFlags, TME_HOVER | TME_LEAVE);
            requested = true;
            Ok(())
        });
        assert!(requested);
    }
}
