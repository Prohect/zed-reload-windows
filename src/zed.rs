//! Zed-editor discovery and lifecycle.
//!
//! * Finding the `Zed.exe` binary on disk.
//! * Enumerating visible Zed windows.
//! * Auto-detecting the Agent Panel send-key binding.
//! * Stopping (graceful → force) and starting Zed (via `explorer.exe`).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

use crate::log::Log;
use crate::win32::{self, Window};

// ---------------------------------------------------------------------------
// find exe
// ---------------------------------------------------------------------------

/// Locate `Zed.exe`, respecting an optional user override.
///
/// Search order:
/// 1. `override_path` (if given and exists)
/// 2. `%LOCALAPPDATA%\Programs\Zed Nightly\Zed.exe`
/// 3. `%LOCALAPPDATA%\Programs\Zed\Zed.exe`
/// 4. Every directory on `%PATH%` (first match wins)
pub fn find_exe(override_path: &Option<String>) -> Option<PathBuf> {
    if let Some(p) = override_path {
        let path = Path::new(p);
        if path.exists() {
            return Some(path.to_path_buf());
        }
    }
    if let Ok(local) = env::var("LOCALAPPDATA") {
        for sub in [
            r"Programs\Zed Nightly\Zed.exe",
            r"Programs\Zed\Zed.exe",
        ] {
            let p = PathBuf::from(&local).join(sub);
            if p.exists() {
                return Some(p);
            }
        }
    }
    if let Ok(path_var) = env::var("PATH") {
        for dir in path_var.split(';').filter(|d| !d.is_empty()) {
            for name in ["Zed.exe", "zed.exe"] {
                let p = PathBuf::from(dir).join(name);
                if p.exists() {
                    return Some(p);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// windows
// ---------------------------------------------------------------------------

/// Visible top-level windows belonging to Zed processes.
/// Prefers windows with a non-empty title.
pub fn windows() -> Vec<Window> {
    let pids = win32::find_pids("zed.exe");
    if pids.is_empty() {
        return Vec::new();
    }
    win32::enum_windows(&pids)
}

// ---------------------------------------------------------------------------
// settings detection
// ---------------------------------------------------------------------------

/// Does Zed require `Ctrl+Enter` to send in the Agent Panel?
///
/// Reads `%APPDATA%\Zed\settings.json` and looks for
/// `"use_modifier_to_send": true`.  Line comments (`//`) are stripped before
/// searching — this is a best-effort heuristic, not a full JSON parse.
pub fn detect_ctrl_enter() -> bool {
    let Ok(appdata) = env::var("APPDATA") else {
        return false;
    };
    let raw =
        fs::read_to_string(PathBuf::from(appdata).join(r"Zed\settings.json")).unwrap_or_default();
    let cleaned: String = raw
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let Some(pos) = cleaned.find("\"use_modifier_to_send\"") else {
        return false;
    };
    let rest = &cleaned[pos + "\"use_modifier_to_send\"".len()..];
    let after_colon = rest.trim_start().strip_prefix(':').unwrap_or("");
    after_colon.trim_start().starts_with("true")
}

// ---------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------

/// Gracefully close all Zed windows (`WM_CLOSE`), then force-kill any
/// remaining processes after `grace_secs`.
pub fn stop(log: &Log, grace_secs: u64) {
    let wins = windows();
    if wins.is_empty() {
        log.info("no Zed window to close");
    }
    for w in &wins {
        log.info(&format!("WM_CLOSE pid={} title='{}'", w.pid, w.title));
        win32::post_quit(w.hwnd);
    }

    let deadline = Instant::now() + Duration::from_secs(grace_secs);
    while Instant::now() < deadline {
        sleep(Duration::from_millis(500));
        if win32::find_pids("zed.exe").is_empty() {
            log.info("Zed exited gracefully");
            return;
        }
    }

    log.info(&format!(
        "graceful close timed out after {grace_secs}s — force killing"
    ));
    for pid in win32::find_pids("zed.exe") {
        win32::kill(pid);
    }
    sleep(Duration::from_secs(2));
}

// ---------------------------------------------------------------------------
// start
// ---------------------------------------------------------------------------

/// Launch Zed through `explorer.exe` so Zed's parent is the desktop shell.
///
/// A direct `CreateProcessW` spawn puts Zed under a console-process ancestry,
/// which causes its terminal-shell auto-detection to fall back to PowerShell
/// instead of the user's preferred shell.  Using `explorer.exe` mimics a
/// normal desktop launch.
pub fn start(
    log: &Log,
    override_path: &Option<String>,
    project: &Option<String>,
) -> Result<(), String> {
    let exe = find_exe(override_path).ok_or("Zed.exe not found")?;
    let cmdline = match project {
        Some(p) => format!("explorer.exe \"{}\" \"{}\"", exe.display(), p),
        None => format!("explorer.exe \"{}\"", exe.display()),
    };
    log.info(&format!(
        "starting {} via explorer.exe {}",
        exe.display(),
        if project.is_some() {
            format!("project='{}'", project.as_ref().unwrap())
        } else {
            "(bare, session restore)".to_string()
        }
    ));
    win32::spawn(&cmdline, CREATE_NEW_PROCESS_GROUP)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// wait-for-window
// ---------------------------------------------------------------------------

/// Block until a visible Zed window appears (up to `timeout_secs`).
/// Respects `window_title` as an optional substring filter.
pub fn wait_for_window(
    window_title: &Option<String>,
    timeout_secs: u64,
    log: &Log,
) -> Option<Window> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    while Instant::now() < deadline {
        let wins = windows();
        let filtered: Vec<&Window> = match window_title {
            Some(sub) => {
                let sub = sub.to_lowercase();
                wins.iter()
                    .filter(|w| w.title.to_lowercase().contains(&sub))
                    .collect()
            }
            None => wins.iter().collect(),
        };
        if let Some(w) = filtered.into_iter().next() {
            return Some(Window {
                hwnd: w.hwnd,
                pid: w.pid,
                title: w.title.clone(),
                hung: w.hung,
            });
        }
        sleep(Duration::from_millis(500));
    }
    log.error(&format!("no Zed window within {timeout_secs}s"));
    None
}
