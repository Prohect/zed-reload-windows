//! Inject a message into Zed's Agent Panel, and the watch-mode loop.

use std::thread::sleep;
use std::time::{Duration, Instant};

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    VK_LCONTROL, VK_LSHIFT, VK_OEM_2, VK_RETURN, VK_V,
};

use crate::log::Log;
use crate::win32;
use crate::zed;

// ---------------------------------------------------------------------------
// inject
// ---------------------------------------------------------------------------

/// Inject `message` into the **already-focused** Agent Panel editor and send
/// it.
///
/// Steps:
/// 1. Wait for a visible Zed window to appear.
/// 2. Settle (give Zed time to finish startup / session-restore).
/// 3. Bring Zed to the foreground.
/// 4. Save the current clipboard text.
/// 5. Set the clipboard to `message`.
/// 6. Press `Ctrl+Shift+/` (agent::ToggleFocus) to open the panel.
/// 7. Press `Ctrl+V` to paste.
/// 8. Press `Enter` (or `Ctrl+Enter` if the setting requires it).
/// 9. Restore the old clipboard text.
///
/// Returns `true` on success.
pub fn inject(
    log: &Log,
    message: &str,
    window_timeout: u64,
    settle_secs: u64,
    window_title: &Option<String>,
    send_key: Option<bool>, // Some(true) = ctrl+enter, Some(false) = enter
) -> bool {
    let Some(w) = zed::wait_for_window(window_title, window_timeout, log) else {
        return false;
    };
    log.info(&format!(
        "window found pid={} title='{}'; settling {settle_secs}s",
        w.pid, w.title,
    ));
    sleep(Duration::from_secs(settle_secs));

    if !win32::force_foreground(w.hwnd) {
        log.error("could not focus Zed window");
        return false;
    }
    sleep(Duration::from_millis(700));

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
        win32::send_combo(&[VK_LCONTROL, VK_LSHIFT, VK_OEM_2]);
        sleep(Duration::from_millis(1600));
        // Paste.
        win32::send_combo(&[VK_LCONTROL, VK_V]);
        sleep(Duration::from_millis(900));
        // Send.
        if ctrl_enter {
            win32::send_combo(&[VK_LCONTROL, VK_RETURN]);
        } else {
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

// ---------------------------------------------------------------------------
// watch
// ---------------------------------------------------------------------------

/// Loop until `watch_timeout` expires, checking for Zed health.
///
/// Triggers revive+inject when:
/// * The Zed process disappears (two consecutive checks miss it).
/// * A Zed window is hung for `unresponsive_secs` (if > 0).
///
/// Returns `true` if the watch period ended without Zed dying.
pub fn watch(
    log: &Log,
    message: &str,
    watch_timeout: u64,
    unresponsive_secs: u64,
    grace_secs: u64,
    window_timeout: u64,
    settle_secs: u64,
    window_title: &Option<String>,
    zed_path: &Option<String>,
    project: &Option<String>,
    send_key: Option<bool>,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(watch_timeout);
    let mut misses = 0u32;
    let mut hung_since: Option<Instant> = None;

    log.info(&format!(
        "watching: timeout={watch_timeout}s unresponsive={}",
        if unresponsive_secs > 0 {
            format!("{unresponsive_secs}s")
        } else {
            "off".into()
        },
    ));

    while Instant::now() < deadline {
        let wins = zed::windows();

        if wins.is_empty() {
            misses += 1;
            log.info(&format!("no Zed window (check {misses}/2)"));
            if misses >= 2 {
                log.info("Zed gone — reviving");
                zed::stop(log, grace_secs); // clean up any windowless leftovers
                return match zed::start(log, zed_path, project) {
                    Ok(()) => inject(
                        log, message, window_timeout, settle_secs, window_title, send_key,
                    ),
                    Err(e) => {
                        log.error(&format!("{e}"));
                        false
                    }
                };
            }
        } else {
            misses = 0;
            if unresponsive_secs > 0 {
                if let Some(hung) = wins.iter().find(|w| w.hung) {
                    match hung_since {
                        None => {
                            hung_since = Some(Instant::now());
                            log.info(&format!(
                                "unresponsive: pid={} title='{}'",
                                hung.pid, hung.title,
                            ));
                        }
                        Some(since)
                            if since.elapsed() >= Duration::from_secs(unresponsive_secs) =>
                        {
                            log.info(&format!(
                                "still unresponsive after {unresponsive_secs}s — restarting",
                            ));
                            zed::stop(log, grace_secs);
                            return match zed::start(log, zed_path, project) {
                                Ok(()) => inject(
                                    log, message, window_timeout, settle_secs, window_title, send_key,
                                ),
                                Err(e) => {
                                    log.error(&format!("{e}"));
                                    false
                                }
                            };
                        }
                        _ => {}
                    }
                } else {
                    hung_since = None;
                }
            }
        }
        sleep(Duration::from_secs(5));
    }

    log.info("watch timeout, Zed stayed healthy");
    true
}
