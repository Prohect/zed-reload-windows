//! Message-injection workflow — two-focus approach.
//!
//! 1. Wait for the Zed window, settle (session-restore completes).
//! 2. **First focus** — heads-up: bring Zed to foreground so the user knows
//!    something is about to happen.  They stop interacting.
//! 3. **Heads-up delay** — user has time to stop any keyboard/mouse activity
//!    that could break the keystroke injection.
//! 4. **Second focus** — bring Zed to foreground again, then inject.

use std::thread::sleep;
use std::time::Duration;

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    VK_LCONTROL, VK_LSHIFT, VK_OEM_2, VK_RETURN, VK_V,
};

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
    send_key: Option<bool>, // Some(true)=ctrl+enter, Some(false)=enter
) -> bool {
    // ── wait for the new Zed window ────────────────────────────────

    let Some(w) = zed::wait_for_window(window_title, window_timeout, log) else {
        return false;
    };
    log.info(&format!(
        "window found pid={} title='{}'; settling {settle_secs}s",
        w.pid, w.title,
    ));
    sleep(Duration::from_secs(settle_secs));

    // ── first focus: heads-up ──────────────────────────────────────

    log.info("--- first focus (heads-up) ---");
    if !win32::force_foreground(log, w.hwnd) {
        log.error("could not focus Zed window (heads-up)");
        return false;
    }
    // Brief pause so the user *sees* Zed come to the foreground.
    sleep(Duration::from_millis(500));

    // ── heads-up delay: user stops interacting ────────────────────

    log.info(&format!("heads-up delay: {heads_up_secs}s  (stop typing!)"));
    sleep(Duration::from_secs(heads_up_secs));

    // ── second focus: do the injection ─────────────────────────────

    log.info("--- second focus (injecting) ---");
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
        // Toggle agent panel focus.
        log.info("sending ctrl+shift+/  (agent::ToggleFocus)");
        win32::send_combo(&[VK_LCONTROL, VK_LSHIFT, VK_OEM_2]);
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
