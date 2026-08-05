//! Message-injection workflow.
//!
//! 1. Wait for the Zed window, settle (session-restore completes).
//! 2. **Windows notification** — a tray balloon warns the user that
//!    keystrokes are about to be injected.  Unlike a focus grab it never
//!    steals focus and works even when Zed is already the foreground
//!    window (where the old focus-based heads-up was a no-op).
//! 3. **Heads-up delay** — the user has time to stop any keyboard/mouse
//!    activity that could break the keystroke injection.
//! 4. **Single focus** — bring Zed to the foreground (restoring it from
//!    the minimized launch), then inject.

use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_LCONTROL, VK_RETURN, VK_V};

use crate::log::Log;
use crate::win32;
use crate::zed;

// ---------------------------------------------------------------------------
// inject
// ---------------------------------------------------------------------------

/// Inject `message` into the Agent Panel.
///
/// Returns `true` on success.
pub fn inject(
    log: &Log,
    message: &str,
    window_timeout: u64,
    settle_secs: u64,
    heads_up_secs: u64,
    window_title: &Option<String>,
    project: &Option<String>,
    exe: &Path,
    not_before: u64,
    old_was_focused: bool,
    send_key: Option<bool>, // Some(true)=ctrl+enter, Some(false)=enter
) -> bool {
    // ── wait for the new Zed window ────────────────────────────────

    let Some(w) = zed::wait_for_window(project, window_title, window_timeout, log, exe, not_before)
    else {
        return false;
    };
    log.info(&format!(
        "window found pid={} title='{}'; settling {settle_secs}s",
        w.pid, w.title,
    ));
    // Minimize the window as soon as it appears — guarded: only when the
    // previous session was NOT focused.  If the user was in Zed, the new
    // window appearing is expected; if they were elsewhere, protect their
    // focus from the relaunch.  (The launch cannot create the window
    // minimized; the injection focus restores it via SW_RESTORE.)
    if old_was_focused {
        log.info("old session was focused — leaving window normal");
    } else if win32::is_minimized(w.hwnd) {
        log.info("window already minimized");
    } else {
        win32::minimize(w.hwnd);
        log.info("window minimized (launch focus-safe)");
    }
    sleep(Duration::from_secs(settle_secs));

    // ── heads-up: Windows notification (never steals focus) ────────

    let notify = win32::notify_balloon(
        "zed-reload",
        &format!("About to type into Zed in {heads_up_secs}s — stop typing!"),
    );
    if let Err(e) = notify {
        log.error(&format!(
            "notification failed: {e}; falling back to focus heads-up"
        ));
        log.info("--- fallback: heads-up focus ---");
        if !win32::force_foreground(log, w.hwnd) {
            log.error("could not focus Zed window (heads-up)");
            return false;
        }
        sleep(Duration::from_millis(500));
    }

    // ── heads-up delay: user stops interacting ────────────────────

    log.info(&format!("heads-up delay: {heads_up_secs}s  (stop typing!)"));
    sleep(Duration::from_secs(heads_up_secs));

    // ── single focus: restore from minimized, then inject ──────────

    log.info("--- focusing Zed (injecting) ---");
    if !win32::force_foreground(log, w.hwnd) {
        log.error("could not focus Zed window (injection)");
        return false;
    }
    sleep(Duration::from_millis(700));

    // ── keystroke injection ────────────────────────────────────────

    let ctrl_enter = send_key.unwrap_or_else(zed::detect_ctrl_enter);
    log.info(&format!(
        "send key: {}",
        if ctrl_enter { "ctrl+enter" } else { "enter" },
    ));

    let old_clip = win32::clipboard_get();
    let ok = (|| -> bool {
        if let Err(e) = win32::clipboard_set(message) {
            log.error(&format!("clipboard: {e}"));
            return false;
        }
        // Toggle agent panel focus (use the user's custom binding if any).
        let toggle_keys = zed::detect_toggle_binding();
        log.info(&format!("sending agent::ToggleFocus  ({:?})", toggle_keys));
        win32::send_combo(&toggle_keys);
        sleep(Duration::from_millis(1600));
        // Paste.
        log.info("sending ctrl+v  (paste)");
        win32::send_combo(&[VK_LCONTROL, VK_V]);
        sleep(Duration::from_millis(900));
        // Send.
        if ctrl_enter {
            log.info("sending ctrl+enter  (send)");
            win32::send_combo(&[VK_LCONTROL, VK_RETURN]);
        } else {
            log.info("sending enter  (send)");
            win32::send_combo(&[VK_RETURN]);
        }
        true
    })();

    if ok {
        log.info(&format!("injected {} chars", message.chars().count()));
    }
    if let Some(old) = old_clip {
        sleep(Duration::from_millis(400));
        let _ = win32::clipboard_set(&old);
    }
    ok
}
