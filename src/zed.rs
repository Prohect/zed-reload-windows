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
/// 3. `%LOCALAPPDATA%\Programs\Zed Preview\Zed.exe`
/// 4. `%LOCALAPPDATA%\Programs\Zed\Zed.exe`
/// 5. Every directory on `%PATH%` (first match wins)
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
            r"Programs\Zed Preview\Zed.exe",
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
// target resolution
// ---------------------------------------------------------------------------

/// The Zed instance to restart: which process to stop (`pid`) and which
/// binary to start (`exe`).  Kept separate on purpose — several independent
/// Zed processes can share one exe path, so the exe identifies the binary,
/// never the set of processes to end.
pub struct Target {
    pub exe: PathBuf,
    pub pid: Option<u32>,
}

/// The closest Zed *ancestor* of this process: `(pid, exe path)`.
/// zed-reload is normally invoked from Zed's terminal or agent panel, so
/// the nearest `zed.exe` up the parent chain is the instance to restart.
fn zed_ancestor() -> Option<(u32, PathBuf)> {
    let mut pid = std::process::id();
    // Depth cap guards against parent-PID loops (reused PIDs).
    for _ in 0..32 {
        let Some(parent) = win32::parent_pid(pid) else { break };
        if parent == pid {
            break;
        }
        if let Some(path) = win32::exe_path(parent) {
            if Path::new(&path)
                .file_name()
                .map_or(false, |n| n.to_string_lossy().eq_ignore_ascii_case("zed.exe"))
            {
                return Some((parent, PathBuf::from(path)));
            }
        }
        pid = parent;
    }
    None
}

/// Resolve the restart target from the live system state.
///
/// * `pid` — `override_pid`, else the closest Zed ancestor, else the only
///   running `zed.exe`.  `None` means "stop nothing".
/// * `exe` — `override_path`, else the target process's image path (the
///   exact variant: Release, Preview, Nightly, or a dev build).  Preferred
///   over the on-disk search in `find_exe`, which cannot see dev builds.
///
/// Several running Zeds with no identifiable target is an error — unless
/// the exe was given explicitly: then the start is unambiguous and nothing
/// is stopped.
pub fn resolve_target(
    override_path: &Option<String>,
    override_pid: Option<u32>,
) -> Result<Option<Target>, String> {
    if let Some(pid) = override_pid {
        let exe = match override_path {
            Some(z) => PathBuf::from(z),
            None => {
                let path = win32::exe_path(pid)
                    .ok_or_else(|| format!("cannot query exe path of pid {pid}"))?;
                let is_zed = Path::new(&path)
                    .file_name()
                    .map_or(false, |n| n.to_string_lossy().eq_ignore_ascii_case("zed.exe"));
                if !is_zed {
                    return Err(format!("pid {pid} is not a Zed process ({path})"));
                }
                PathBuf::from(path)
            }
        };
        return Ok(Some(Target { exe, pid: Some(pid) }));
    }
    if let Some((pid, path)) = zed_ancestor() {
        let exe = override_path.as_ref().map_or(path, PathBuf::from);
        return Ok(Some(Target { exe, pid: Some(pid) }));
    }
    let running: Vec<(u32, String)> = win32::find_pids("zed.exe")
        .iter()
        .filter_map(|&pid| win32::exe_path(pid).map(|p| (pid, p)))
        .collect();
    match running.len() {
        0 => Ok(None),
        1 => {
            let (pid, path) = running.into_iter().next().unwrap();
            let exe = override_path.as_ref().map_or(PathBuf::from(path), PathBuf::from);
            Ok(Some(Target { exe, pid: Some(pid) }))
        }
        _ => match override_path {
            Some(z) => Ok(Some(Target { exe: PathBuf::from(z), pid: None })),
            None => Err(format!(
                "no Zed ancestor and several Zed processes are running — cannot \
                 tell which to restart (pass --zed-pid or --zed-path):\n  {}",
                running
                    .iter()
                    .map(|(pid, p)| format!("pid {pid}: {p}"))
                    .collect::<Vec<_>>()
                    .join("\n  "),
            )),
        },
    }
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

/// PIDs of running `zed.exe` processes whose image path is `exe`
/// (case-insensitive).  Processes whose path cannot be queried are
/// excluded — never touch what cannot be identified.
fn pids_for(exe: &Path) -> Vec<u32> {
    let target = exe.to_string_lossy().into_owned();
    win32::find_pids("zed.exe")
        .into_iter()
        .filter(|&pid| win32::exe_path(pid).map_or(false, |p| p.eq_ignore_ascii_case(&target)))
        .collect()
}

/// Visible top-level windows belonging to Zed processes running `exe`.
pub fn windows_for(exe: &Path) -> Vec<Window> {
    let pids = pids_for(exe);
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
// keybinding detection (keymap.json)
// ---------------------------------------------------------------------------

/// Return the virtual-key sequence for `agent::ToggleFocus`.
///
/// Reads `%APPDATA%\Zed\keymap.json` and looks for a custom binding that
/// maps to `"agent::ToggleFocus"`.  If none is found the Zed default
/// (`Ctrl+Shift+/`) is returned.
pub fn detect_toggle_binding() -> Vec<u16> {
    let default: Vec<u16> = vec![
        windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_LCONTROL,
        windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_LSHIFT,
        windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_OEM_2,
    ];

    let Ok(appdata) = env::var("APPDATA") else {
        return default;
    };
    let raw =
        fs::read_to_string(PathBuf::from(appdata).join(r"Zed\keymap.json")).unwrap_or_default();

    // Find the string `"agent::ToggleFocus"` and extract the preceding key.
    let Some(action_pos) = raw.find("\"agent::ToggleFocus\"") else {
        return default;
    };

    // Walk backwards from the action to find the key string before ':'
    let before = &raw[..action_pos];
    let Some(colon) = before.rfind(':') else {
        return default;
    };
    let before_colon = &before[..colon];
    let Some(close_quote) = before_colon.rfind('"') else {
        return default;
    };
    let before_close = &before_colon[..close_quote];
    let Some(open_quote) = before_close.rfind('"') else {
        return default;
    };
    let binding_str = &before_colon[open_quote + 1..close_quote];

    parse_binding(binding_str).unwrap_or(default)
}

/// Parse a Zed keybinding string like `"ctrl-shift-/"` into virtual-key codes.
/// Handles the minus-key edge case (`ctrl-shift--`).
/// Returns `None` if any segment is unrecognised.
pub fn parse_binding(s: &str) -> Option<Vec<u16>> {
    let parts: Vec<&str> = s.split('-').collect();
    let mut keys = Vec::new();
    let mut i = 0;

    // Consume known modifiers from the front.
    while i < parts.len() {
        match parts[i].to_lowercase().as_str() {
            "ctrl" | "control" => {
                keys.push(windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_LCONTROL);
                i += 1;
            }
            "shift" => {
                keys.push(windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_LSHIFT);
                i += 1;
            }
            "alt" | "option" => {
                keys.push(windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_LMENU);
                i += 1;
            }
            "meta" | "cmd" | "super" => {
                keys.push(windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_LWIN);
                i += 1;
            }
            _ => break,
        }
    }

    // Everything after the last modifier is the key, re-joined with '-'.
    // This handles the minus-key edge case: "ctrl-shift--"  → key is "-".
    let key_str = parts[i..].join("-");
    if key_str.is_empty() {
        return None;
    }
    keys.push(key_to_vk(&key_str)?);
    Some(keys)
}

/// Map a single key name (e.g. `"/"`, `"enter"`, `"f5"`) to its virtual-key code.
fn key_to_vk(name: &str) -> Option<u16> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        VK_BACK, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_INSERT,
        VK_LEFT, VK_NEXT, VK_OEM_1, VK_OEM_2, VK_OEM_3, VK_OEM_4, VK_OEM_5,
        VK_OEM_6, VK_OEM_7, VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD,
        VK_OEM_PLUS, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SPACE, VK_TAB, VK_UP,
    };

    let lower = name.to_lowercase();

    // Single character (letters, digits — VK codes match ASCII uppercase).
    if lower.chars().count() == 1 {
        let c = lower.chars().next().unwrap();
        return match c {
            'a'..='z' => Some(c.to_ascii_uppercase() as u16),
            '0'..='9' => Some(c as u16),
            '/' => Some(VK_OEM_2),
            ';' => Some(VK_OEM_1),
            '\'' => Some(VK_OEM_7),
            '[' => Some(VK_OEM_4),
            ']' => Some(VK_OEM_6),
            '\\' => Some(VK_OEM_5),
            ',' => Some(VK_OEM_COMMA),
            '.' => Some(VK_OEM_PERIOD),
            '-' => Some(VK_OEM_MINUS),
            '=' => Some(VK_OEM_PLUS),
            '`' => Some(VK_OEM_3),
            _ => None,
        };
    }

    // Named keys.
    match lower.as_str() {
        "enter" | "return" => Some(VK_RETURN),
        "escape" | "esc" => Some(VK_ESCAPE),
        "tab" => Some(VK_TAB),
        "space" => Some(VK_SPACE),
        "backspace" => Some(VK_BACK),
        "delete" | "del" => Some(VK_DELETE),
        "insert" | "ins" => Some(VK_INSERT),
        "home" => Some(VK_HOME),
        "end" => Some(VK_END),
        "pageup" | "pgup" => Some(VK_PRIOR),
        "pagedown" | "pgdn" => Some(VK_NEXT),
        "up" => Some(VK_UP),
        "down" => Some(VK_DOWN),
        "left" => Some(VK_LEFT),
        "right" => Some(VK_RIGHT),
        s if s.starts_with('f') => {
            if let Ok(n) = s[1..].parse::<u16>() {
                if (1..=15).contains(&n) {
                    // VK_F1 == 112, VK_Fn = 112 + n - 1
                    Some(112u16 + n - 1)
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------

/// Gracefully close the *target* Zed process (`WM_CLOSE` to its windows),
/// then force-kill it after `grace_secs` if it is still running.
///
/// Only the target is touched: other Zed processes — including independent
/// processes of the same variant — keep running.  The target is verified
/// to still be a `zed.exe` process first (guard against PID reuse during
/// the wait); `None` means "nothing to stop".
///
/// Returns whether the target's window was the foreground window when the
/// stop began — the caller guards the minimized relaunch on this: if the
/// user was in Zed, the new window appearing normally is expected; if they
/// were elsewhere, their focus is protected by minimizing the new window.
pub fn stop(log: &Log, grace_secs: u64, target: Option<u32>) -> bool {
    let alive = |pid: u32| win32::find_pids("zed.exe").contains(&pid);
    let Some(pid) = target.filter(|&pid| alive(pid)) else {
        log.info("no live target Zed process — nothing to stop");
        return false;
    };
    let wins = win32::enum_windows(&[pid]);
    // Capture focus before closing anything: after WM_CLOSE the
    // foreground moves to whatever was behind.
    let was_focused = wins.iter().any(|w| win32::get_foreground() == w.hwnd);
    if wins.is_empty() {
        log.info("target has no visible window to close");
    }
    for w in &wins {
        log.info(&format!("WM_CLOSE pid={} title='{}'", w.pid, w.title));
        win32::post_quit(w.hwnd);
    }
    log.info(&format!("old session focused: {was_focused}"));

    let deadline = Instant::now() + Duration::from_secs(grace_secs);
    while Instant::now() < deadline {
        sleep(Duration::from_millis(500));
        if !alive(pid) {
            log.info("Zed exited gracefully");
            return was_focused;
        }
    }

    log.info(&format!(
        "graceful close timed out after {grace_secs}s — force killing pid {pid}"
    ));
    win32::kill(pid);
    sleep(Duration::from_secs(2));
    was_focused
}

// ---------------------------------------------------------------------------
// start
// ---------------------------------------------------------------------------

/// Launch Zed through `explorer.exe` so Zed's parent is the desktop shell.
///
/// A direct `CreateProcessW` spawn puts Zed under a console-process ancestry
/// (the detached worker's own ancestry contains the launcher's console),
/// which makes its terminal-shell auto-detection fall back to PowerShell
/// instead of the user's preferred shell — verified empirically.
/// `explorer.exe` gives Zed the standard desktop parent.
///
/// With a project path, a plain `explorer.exe "Zed.exe" "path"` command line
/// does not work: explorer mangles multi-argument command lines into one path
/// and launches nothing.  The project is therefore carried in a temporary
/// `.lnk` shortcut (target + arguments + working directory in one file) that
/// explorer opens as a single argument.
///
/// The window is NOT minimized at launch (explorer delivers
/// `SW_SHOWDEFAULT` regardless of the shortcut's show state, Zed rejects CLI
/// flags, and gpui ignores `STARTUPINFO.wShowWindow`); the worker minimizes
/// it as soon as it appears instead — see `work::inject`.
///
/// Returns the temporary shortcut path when one was used; the caller removes
/// it once the launch has been confirmed (the worker lives long enough).
pub fn start(
    log: &Log,
    exe: &Path,
    project: &Option<String>,
) -> Result<Option<PathBuf>, String> {
    match project {
        Some(p) => {
            // Remove shortcuts left behind by interrupted runs.
            if let Ok(rd) = fs::read_dir(env::temp_dir()) {
                for entry in rd.flatten() {
                    let file_name = entry.file_name();
                    let name = file_name.to_string_lossy();
                    if name.starts_with("zed-reload-") && name.ends_with(".lnk") {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }

            let lnk = env::temp_dir().join(format!("zed-reload-{}.lnk", std::process::id()));
            if let Err(e) = win32::write_shortcut(&exe, &format!("\"{p}\""), p, &lnk) {
                let _ = fs::remove_file(&lnk);
                return Err(e);
            }
            log.info(&format!(
                "starting {} via explorer shortcut project='{p}'",
                exe.display(),
            ));
            win32::spawn(
                &format!("explorer.exe \"{}\"", lnk.display()),
                CREATE_NEW_PROCESS_GROUP,
            )?;
            Ok(Some(lnk))
        }
        None => {
            let cmdline = format!("explorer.exe \"{}\"", exe.display());
            log.info(&format!(
                "starting {} via explorer.exe (bare, session restore)",
                exe.display(),
            ));
            win32::spawn(&cmdline, CREATE_NEW_PROCESS_GROUP)?;
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// wait-for-window
// ---------------------------------------------------------------------------

/// Substring to match window titles against: an explicit `--window-title`
/// wins; otherwise the project's folder name, because Zed titles windows
/// with the project folder name.  `None` means "any visible window".
fn title_filter(project: &Option<String>, window_title: &Option<String>) -> Option<String> {
    match window_title {
        Some(t) => Some(t.clone()),
        None => project
            .as_ref()
            .and_then(|p| Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned())),
    }
}

/// Block until a window of the *newly started* Zed appears (up to
/// `timeout_secs`).
///
/// Only windows of processes running `exe` that were *created at or after*
/// `not_before` (captured just before the launch) qualify — a surviving
/// process of the same or another variant can hold an equally-titled
/// window, and the injection must never land there.
///
/// The window is matched against `window_title` (explicit substring filter)
/// or, failing that, against the project's folder name — Zed titles windows
/// with the project folder name, so the injection lands in the *calling*
/// session.  This is what makes recovery strict: with several projects
/// open, session restore may bring a different window to the front, but the
/// continue-message is never sent to a window that is not the caller's.
/// If no matching window appears, `None` is returned (the caller fails
/// rather than inject into a random session-restored window).
pub fn wait_for_window(
    project: &Option<String>,
    window_title: &Option<String>,
    timeout_secs: u64,
    log: &Log,
    exe: &Path,
    not_before: u64,
) -> Option<Window> {
    let filter = title_filter(project, window_title);
    if let Some(f) = &filter {
        log.info(&format!("waiting for window title containing '{f}'"));
    }
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    while Instant::now() < deadline {
        let wins: Vec<Window> = windows_for(exe)
            .into_iter()
            .filter(|w| win32::start_time(w.pid).map_or(false, |t| t >= not_before))
            .collect();
        let filtered: Vec<&Window> = match &filter {
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
    match &filter {
        Some(f) => log.error(&format!(
            "no Zed window matching '{f}' within {timeout_secs}s — \
             not injecting (strict: caller's session only; \
             retry with --window-title to override)"
        )),
        None => log.error(&format!("no Zed window within {timeout_secs}s")),
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_filter_prefers_explicit_window_title() {
        let project = Some("F:\\workspace\\zed-reload".into());
        let wt = Some("nightly".into());
        assert_eq!(title_filter(&project, &wt).unwrap(), "nightly");
    }

    #[test]
    fn title_filter_derives_project_folder_name() {
        let project = Some("F:\\workspace\\zed-reload".into());
        assert_eq!(title_filter(&project, &None).unwrap(), "zed-reload");
    }

    #[test]
    fn title_filter_requires_something_to_match() {
        assert_eq!(title_filter(&None, &None), None);
        // A root path has no folder name to match against.
        assert_eq!(title_filter(&Some("C:\\".into()), &None), None);
    }
}
