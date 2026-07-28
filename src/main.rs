// zed-reload - restart Zed and inject a message into the Agent Panel (Windows).
//
// Rust port of zed-reload.ps1. One binary, two roles:
//   launcher (default)  - writes the message to a temp file, re-spawns itself
//                         DETACHED (survives Zed's death), returns immediately.
//   worker   (--worker) - does the work: kill/relaunch Zed, wait for window,
//                         force foreground, ctrl+shift+/ (agent::ToggleFocus),
//                         paste via clipboard, send with enter/ctrl+enter.
//
// Zed facts relied on (verified against zed source):
//   ctrl+shift+/ = agent::ToggleFocus; panel never focused right after start,
//     so one press lands focus in the message editor.
//   enter sends, unless settings.json has "use_modifier_to_send": true
//     (then ctrl+enter). Auto-detected; override with --send-enter/--send-ctrl-enter.
//   restore_on_startup defaults to last_session: a bare relaunch reopens the
//     previous workspace incl. the Agent Panel.
// Foreground: background processes may not steal it (foreground lock). We use
//   SwitchToThisWindow + AttachThreadInput (same problem Zed's own activate()
//   solves with an ALT SendInput hack).
// Caveats: needs an unlocked interactive desktop; clipboard is used for
//   pasting and restored afterwards (text only).

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, BOOL, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, SYSTEMTIME, WPARAM,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::{
    AttachThreadInput, CreateProcessW, GetCurrentProcessId, GetCurrentThreadId, OpenProcess,
    TerminateProcess, CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
    DETACHED_PROCESS, PROCESS_INFORMATION, PROCESS_TERMINATE, STARTUPINFOW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_F15, VK_LCONTROL,
    VK_LSHIFT, VK_OEM_2, VK_RETURN,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetForegroundWindow, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsHungAppWindow, IsIconic, IsWindowVisible,
    PostMessageW, SetForegroundWindow, ShowWindow, SwitchToThisWindow, SW_RESTORE,
};

const WM_CLOSE: u32 = 0x0010;
const CF_UNICODETEXT: u32 = 13;
const VK_V: u16 = 0x56; // 'V' key
const TRUE: BOOL = 1;
const FALSE: BOOL = 0;

// ---------------------------------------------------------------- args ----

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Restart,
    Send,
    Watch,
    Check,
}

struct Args {
    mode: Mode,
    worker: bool,
    wait: u64,
    settle: u64,
    grace: u64,
    window_timeout: u64,
    watch_timeout: u64,
    unresponsive: u64,
    project: Option<String>,
    window_title: Option<String>,
    zed_path: Option<String>,
    send_key: Option<bool>, // Some(true)=ctrl+enter, Some(false)=enter
    message_file: Option<String>,
    message: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        mode: Mode::Restart,
        worker: false,
        wait: 6,
        settle: 10,
        grace: 20,
        window_timeout: 90,
        watch_timeout: 3600,
        unresponsive: 0,
        project: None,
        window_title: None,
        zed_path: None,
        send_key: None,
        message_file: None,
        message: None,
    };
    let mut parts: Vec<String> = Vec::new();
    let mut it = env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut take = |name: &str| -> Result<String, String> {
            it.next().ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--restart" => a.mode = Mode::Restart,
            "--send" => a.mode = Mode::Send,
            "--watch" => a.mode = Mode::Watch,
            "--check" => a.mode = Mode::Check,
            "--worker" => a.worker = true,
            "--wait" => a.wait = take("--wait")?.parse().map_err(|_| "bad --wait")?,
            "--settle" => a.settle = take("--settle")?.parse().map_err(|_| "bad --settle")?,
            "--grace" => a.grace = take("--grace")?.parse().map_err(|_| "bad --grace")?,
            "--window-timeout" => {
                a.window_timeout = take("--window-timeout")?.parse().map_err(|_| "bad --window-timeout")?
            }
            "--watch-timeout" => {
                a.watch_timeout = take("--watch-timeout")?.parse().map_err(|_| "bad --watch-timeout")?
            }
            "--unresponsive" => {
                a.unresponsive = take("--unresponsive")?.parse().map_err(|_| "bad --unresponsive")?
            }
            "--project" => a.project = Some(take("--project")?),
            "--window-title" => a.window_title = Some(take("--window-title")?),
            "--zed-path" => a.zed_path = Some(take("--zed-path")?),
            "--message-file" => a.message_file = Some(take("--message-file")?),
            "--send-enter" => a.send_key = Some(false),
            "--send-ctrl-enter" => a.send_key = Some(true),
            "-h" | "--help" => {
                print_usage();
                exit(0);
            }
            other => parts.push(other.to_string()),
        }
    }
    if !parts.is_empty() {
        a.message = Some(parts.join(" "));
    }
    Ok(a)
}

fn print_usage() {
    println!(
        "zed-reload - restart Zed and inject a message into the Agent Panel

USAGE:
  zed-reload [message...]     restart Zed, then send message (default: \"continue\")
  zed-reload --send [msg...]  send to the running Zed (no restart)
  zed-reload --watch [msg...] wait for Zed to die/hang, then revive + send
  zed-reload --check          diagnostics (foreground, no side effects)

OPTIONS:
  --wait N            seconds before acting (default 6)
  --settle N          seconds to wait after the window appears (default 10)
  --grace N           graceful-quit budget before force-kill (default 20)
  --window-timeout N  wait for Zed window (default 90)
  --watch-timeout N   watch mode: give up after N seconds (default 3600)
  --unresponsive N    watch mode: also revive if window hangs N seconds
  --project PATH      open PATH instead of relying on session restore
  --window-title SUB  only target a window whose title contains SUB
  --zed-path PATH     explicit Zed.exe location
  --send-enter / --send-ctrl-enter   force send key (default: auto-detect)

Log: zed-reload.log next to the exe."
    );
}

// ---------------------------------------------------------------- logging ----

struct Log {
    path: PathBuf,
    mode: String,
}

impl Log {
    fn new(mode: &str) -> Self {
        let dir = env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));
        Log {
            path: dir.join("zed-reload.log"),
            mode: mode.to_string(),
        }
    }

    fn line(&self, msg: &str) {
        let ts = unsafe {
            let mut st: SYSTEMTIME = std::mem::zeroed();
            GetLocalTime(&mut st);
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
            )
        };
        let entry = format!(
            "[{ts}] [{mode}] [pid {pid}] {msg}",
            mode = self.mode,
            pid = unsafe { GetCurrentProcessId() }
        );
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(f, "{entry}");
        }
        // stdout may not exist in the detached worker; ignore write errors.
        let _ = writeln!(std::io::stdout(), "{entry}");
        let _ = std::io::stdout().flush();
    }
}

// ---------------------------------------------------------------- win helpers ----

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

struct WinInfo {
    hwnd: HWND,
    pid: u32,
    title: String,
    hung: bool,
}

fn zed_pids() -> Vec<u32> {
    let mut pids = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE || snap.is_null() {
            return pids;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut entry) == TRUE {
            loop {
                if wide_to_string(&entry.szExeFile).eq_ignore_ascii_case("zed.exe") {
                    pids.push(entry.th32ProcessID);
                }
                if Process32NextW(snap, &mut entry) != TRUE {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    pids
}

unsafe extern "system" fn enum_windows_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = &mut *(lparam as *mut (Vec<u32>, Vec<WinInfo>));
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid != 0 && state.0.contains(&pid) && IsWindowVisible(hwnd) == TRUE {
        let len = GetWindowTextLengthW(hwnd);
        let title = if len > 0 {
            let mut buf = vec![0u16; (len + 1) as usize];
            GetWindowTextW(hwnd, buf.as_mut_ptr(), len + 1);
            wide_to_string(&buf)
        } else {
            String::new()
        };
        state.1.push(WinInfo {
            hwnd,
            pid,
            title,
            hung: IsHungAppWindow(hwnd) == TRUE,
        });
    }
    TRUE
}

/// Visible windows belonging to any Zed process. Prefers windows with a
/// non-empty title when both kinds exist.
fn zed_windows() -> Vec<WinInfo> {
    let pids = zed_pids();
    if pids.is_empty() {
        return Vec::new();
    }
    let mut state: (Vec<u32>, Vec<WinInfo>) = (pids, Vec::new());
    unsafe {
        let _ = EnumWindows(
            Some(enum_windows_cb),
            &mut state as *mut _ as LPARAM,
        );
    }
    let wins = state.1;
    if wins.iter().any(|w| !w.title.is_empty()) {
        wins.into_iter().filter(|w| !w.title.is_empty()).collect()
    } else {
        wins
    }
}

fn find_zed_exe(override_path: &Option<String>) -> Option<PathBuf> {
    if let Some(p) = override_path {
        if Path::new(p).exists() {
            return Some(PathBuf::from(p));
        }
    }
    if let Ok(local) = env::var("LOCALAPPDATA") {
        for cand in [
            r"Programs\Zed Nightly\Zed.exe",
            r"Programs\Zed\Zed.exe",
        ] {
            let p = PathBuf::from(&local).join(cand);
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

fn detect_ctrl_enter() -> bool {
    let Ok(appdata) = env::var("APPDATA") else {
        return false;
    };
    let raw = fs::read_to_string(PathBuf::from(appdata).join(r"Zed\settings.json"))
        .unwrap_or_default();
    let cleaned = raw
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let Some(pos) = cleaned.find("\"use_modifier_to_send\"") else {
        return false;
    };
    let rest = cleaned[pos + "\"use_modifier_to_send\"".len()..].trim_start();
    match rest.strip_prefix(':') {
        Some(v) => v.trim_start().starts_with("true"),
        None => false,
    }
}

// ---------------------------------------------------------------- process control ----

fn stop_zed(log: &Log, grace_secs: u64) {
    let wins = zed_windows();
    if wins.is_empty() {
        log.line("no Zed window to close");
    }
    for w in &wins {
        log.line(&format!("WM_CLOSE pid={} title='{}'", w.pid, w.title));
        unsafe {
            let _ = PostMessageW(w.hwnd, WM_CLOSE, 0 as WPARAM, 0 as LPARAM);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(grace_secs);
    while Instant::now() < deadline {
        sleep(Duration::from_millis(500));
        if zed_pids().is_empty() {
            log.line("Zed exited gracefully");
            return;
        }
    }
    log.line(&format!(
        "graceful close timed out after {grace_secs}s (modal prompt?) - force killing"
    ));
    for pid in zed_pids() {
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, FALSE, pid);
            if !h.is_null() {
                let _ = TerminateProcess(h, 1);
                let _ = CloseHandle(h);
            }
        }
    }
    sleep(Duration::from_secs(2));
}

fn spawn_process(cmdline: &str, creation_flags: u32) -> Result<u32, String> {
    let mut cmd = to_wide(cmdline);
    unsafe {
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessW(
            std::ptr::null(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            FALSE,
            creation_flags,
            std::ptr::null(),
            std::ptr::null(),
            &si,
            &mut pi,
        );
        if ok == FALSE {
            let err = GetLastError();
            return Err(format!("CreateProcessW failed gle={err}"));
        }
        let _ = CloseHandle(pi.hProcess);
        let _ = CloseHandle(pi.hThread);
        Ok(pi.dwProcessId)
    }
}

fn start_zed(log: &Log, args: &Args) -> Result<(), String> {
    let exe = find_zed_exe(&args.zed_path).ok_or("Zed.exe not found")?;
    let cmdline = match &args.project {
        Some(p) => format!("\"{}\" \"{}\"", exe.display(), p),
        None => format!("\"{}\"", exe.display()),
    };
    log.line(&format!(
        "starting {} {}",
        exe.display(),
        if args.project.is_some() {
            format!("project='{}'", args.project.as_ref().unwrap())
        } else {
            "(bare, session restore)".to_string()
        }
    ));
    spawn_process(&cmdline, CREATE_NEW_PROCESS_GROUP)?;
    Ok(())
}

// ---------------------------------------------------------------- foreground + input ----

fn force_foreground(log: &Log, hwnd: HWND) -> bool {
    for attempt in 1..=15u32 {
        unsafe {
            if GetForegroundWindow() == hwnd {
                log.line(&format!("foreground=true after {} attempt(s)", attempt - 1));
                return true;
            }
            if IsIconic(hwnd) == TRUE {
                // only un-minimize; SW_RESTORE would un-maximize a maximized window
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            SwitchToThisWindow(hwnd, TRUE);
            sleep(Duration::from_millis(200));
            if GetForegroundWindow() == hwnd {
                log.line(&format!("foreground=true via SwitchToThisWindow (attempt {attempt})"));
                return true;
            }
            let fg = GetForegroundWindow();
            let mut fg_pid: u32 = 0;
            let fg_thread = GetWindowThreadProcessId(fg, &mut fg_pid);
            let my_thread = GetCurrentThreadId();
            if fg_thread != 0 && fg_thread != my_thread {
                let _ = AttachThreadInput(my_thread, fg_thread, TRUE);
                let _ = SetForegroundWindow(hwnd);
                let _ = BringWindowToTop(hwnd);
                let _ = AttachThreadInput(my_thread, fg_thread, FALSE);
            } else {
                let _ = SetForegroundWindow(hwnd);
            }
            sleep(Duration::from_millis(200));
            if GetForegroundWindow() == hwnd {
                log.line(&format!("foreground=true via AttachThreadInput (attempt {attempt})"));
                return true;
            }
            // phantom F15: counts as input for the foreground lock, apps ignore it
            send_combo(&[VK_F15]);
            let _ = SetForegroundWindow(hwnd);
            sleep(Duration::from_millis(250));
            if GetForegroundWindow() == hwnd {
                log.line(&format!("foreground=true via F15+SetForegroundWindow (attempt {attempt})"));
                return true;
            }
        }
    }
    log.line("foreground=false after 15 attempts");
    false
}

/// Press vks down in order, release in reverse (e.g. ctrl+shift+/).
fn send_combo(vks: &[u16]) {
    let mut inputs: Vec<INPUT> = Vec::with_capacity(vks.len() * 2);
    for &vk in vks {
        inputs.push(key_input(vk, 0));
    }
    for &vk in vks.iter().rev() {
        inputs.push(key_input(vk, KEYEVENTF_KEYUP));
    }
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
    }
}

fn key_input(vk: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

// ---------------------------------------------------------------- clipboard ----

fn clipboard_set(text: &str) -> Result<(), String> {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(std::ptr::null_mut()) == TRUE {
                opened = true;
                break;
            }
            sleep(Duration::from_millis(100));
        }
        if !opened {
            return Err("OpenClipboard failed".into());
        }
        let result = (|| -> Result<(), String> {
            if EmptyClipboard() != TRUE {
                return Err("EmptyClipboard failed".into());
            }
            let bytes = wide.len() * 2;
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if h.is_null() {
                return Err("GlobalAlloc failed".into());
            }
            let ptr = GlobalLock(h);
            if ptr.is_null() {
                return Err("GlobalLock failed".into());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr as *mut u16, wide.len());
            let _ = GlobalUnlock(h);
            if SetClipboardData(CF_UNICODETEXT, h as HANDLE).is_null() {
                return Err("SetClipboardData failed".into());
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

fn clipboard_get_text() -> Option<String> {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) != TRUE {
            return None;
        }
        let out = (|| {
            let h = GetClipboardData(CF_UNICODETEXT);
            if h.is_null() {
                return None;
            }
            let ptr = GlobalLock(h as _);
            if ptr.is_null() {
                return None;
            }
            let size = GlobalSize(h as _) / 2;
            let mut s = String::new();
            for i in 0..size {
                let c = *(ptr as *const u16).add(i);
                if c == 0 {
                    break;
                }
                s.push(char::from_u32(c as u32).unwrap_or('\u{FFFD}'));
            }
            let _ = GlobalUnlock(h as _);
            Some(s)
        })();
        let _ = CloseClipboard();
        out
    }
}

// ---------------------------------------------------------------- inject ----

fn wait_zed_window(args: &Args, log: &Log) -> Option<WinInfo> {
    let deadline = Instant::now() + Duration::from_secs(args.window_timeout);
    while Instant::now() < deadline {
        let wins = zed_windows();
        let filtered: Vec<&WinInfo> = match &args.window_title {
            Some(sub) => {
                let sub = sub.to_lowercase();
                wins.iter()
                    .filter(|w| w.title.to_lowercase().contains(&sub))
                    .collect()
            }
            None => wins.iter().collect(),
        };
        if let Some(w) = filtered.into_iter().next() {
            // copy the fields we need (HWND is a raw pointer, Copy)
            return Some(WinInfo {
                hwnd: w.hwnd,
                pid: w.pid,
                title: w.title.clone(),
                hung: w.hung,
            });
        }
        sleep(Duration::from_millis(500));
    }
    log.line(&format!(
        "ERROR: no Zed window within {}s",
        args.window_timeout
    ));
    None
}

fn inject(args: &Args, log: &Log, message: &str) -> bool {
    let Some(w) = wait_zed_window(args, log) else {
        return false;
    };
    log.line(&format!(
        "window found pid={} title='{}'; settling {}s",
        w.pid, w.title, args.settle
    ));
    sleep(Duration::from_secs(args.settle));
    if !force_foreground(log, w.hwnd) {
        log.line("ERROR: could not focus Zed window");
        return false;
    }
    sleep(Duration::from_millis(700));

    let ctrl_enter = args.send_key.unwrap_or_else(detect_ctrl_enter);
    log.line(&format!(
        "send key: {}",
        if ctrl_enter { "ctrl+enter" } else { "enter" }
    ));

    let old_clip = clipboard_get_text();
    let ok = (|| -> bool {
        if let Err(e) = clipboard_set(message) {
            log.line(&format!("ERROR: clipboard: {e}"));
            return false;
        }
        send_combo(&[VK_LCONTROL, VK_LSHIFT, VK_OEM_2]); // agent::ToggleFocus
        sleep(Duration::from_millis(1600));
        send_combo(&[VK_LCONTROL, VK_V]); // paste
        sleep(Duration::from_millis(900));
        if ctrl_enter {
            send_combo(&[VK_LCONTROL, VK_RETURN]);
        } else {
            send_combo(&[VK_RETURN]);
        }
        true
    })();
    if ok {
        log.line(&format!("injected {} chars", message.chars().count()));
    }
    if let Some(old) = old_clip {
        sleep(Duration::from_millis(400));
        let _ = clipboard_set(&old);
    }
    ok
}

// ---------------------------------------------------------------- worker flow ----

fn run_worker(args: &Args, log: &Log) -> i32 {
    let message = if let Some(f) = &args.message_file {
        match fs::read_to_string(f) {
            Ok(m) => {
                let _ = fs::remove_file(f);
                m
            }
            Err(e) => {
                log.line(&format!("ERROR reading message file '{f}': {e}"));
                return 2;
            }
        }
    } else {
        args.message.clone().unwrap_or_else(|| "continue".into())
    };
    log.line(&format!(
        "=== start: msgLen={} wait={}s settle={}s grace={}s ===",
        message.chars().count(),
        args.wait,
        args.settle,
        args.grace
    ));

    let ok = match args.mode {
        Mode::Send => {
            sleep(Duration::from_secs(args.wait));
            inject(args, log, &message)
        }
        Mode::Restart => {
            sleep(Duration::from_secs(args.wait));
            stop_zed(log, args.grace);
            match start_zed(log, args) {
                Ok(()) => inject(args, log, &message),
                Err(e) => {
                    log.line(&format!("ERROR: {e}"));
                    false
                }
            }
        }
        Mode::Watch => run_watch(args, log, &message),
        Mode::Check => true, // handled before respawn
    };
    log.line(&format!("=== done: ok={ok} ==="));
    if ok {
        0
    } else {
        3
    }
}

fn run_watch(args: &Args, log: &Log, message: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(args.watch_timeout);
    let mut misses = 0u32;
    let mut bad_since: Option<Instant> = None;
    log.line(&format!(
        "watching: timeout={}s unresponsive={}",
        args.watch_timeout,
        if args.unresponsive > 0 {
            format!("{}s", args.unresponsive)
        } else {
            "off".into()
        }
    ));
    while Instant::now() < deadline {
        let wins = zed_windows();
        if wins.is_empty() {
            misses += 1;
            log.line(&format!("no Zed window (check {misses}/2)"));
            if misses >= 2 {
                log.line("Zed gone - reviving");
                stop_zed(log, args.grace); // clean up windowless leftovers
                return match start_zed(log, args) {
                    Ok(()) => inject(args, log, message),
                    Err(e) => {
                        log.line(&format!("ERROR: {e}"));
                        false
                    }
                };
            }
        } else {
            misses = 0;
            if args.unresponsive > 0 {
                if let Some(hung) = wins.iter().find(|w| w.hung) {
                    match bad_since {
                        None => {
                            bad_since = Some(Instant::now());
                            log.line(&format!(
                                "unresponsive: pid={} title='{}'",
                                hung.pid, hung.title
                            ));
                        }
                        Some(since) if since.elapsed() >= Duration::from_secs(args.unresponsive) => {
                            log.line(&format!(
                                "still unresponsive after {}s - restarting",
                                args.unresponsive
                            ));
                            stop_zed(log, args.grace);
                            return match start_zed(log, args) {
                                Ok(()) => inject(args, log, message),
                                Err(e) => {
                                    log.line(&format!("ERROR: {e}"));
                                    false
                                }
                            };
                        }
                        _ => {}
                    }
                } else {
                    bad_since = None;
                }
            }
        }
        sleep(Duration::from_secs(5));
    }
    log.line("watch timeout, Zed stayed healthy");
    true
}

// ---------------------------------------------------------------- check ----

fn run_check(args: &Args) -> i32 {
    let exe = find_zed_exe(&args.zed_path);
    println!("zed-reload check (rust)");
    println!("  self       : {}", env::current_exe().map(|p| p.display().to_string()).unwrap_or_default());
    match &exe {
        Some(p) => println!("  zed exe    : {}", p.display()),
        None => println!("  zed exe    : NOT FOUND"),
    }
    let wins = zed_windows();
    println!("  zed windows: {}", wins.len());
    for w in &wins {
        println!(
            "    pid={} hung={} title='{}'",
            w.pid, w.hung, w.title
        );
    }
    println!(
        "  send key   : {} (auto-detected)",
        if detect_ctrl_enter() { "ctrl+enter" } else { "enter" }
    );
    let log = Log::new("check");
    println!("  log file   : {}", log.path.display());
    if exe.is_some() {
        0
    } else {
        4
    }
}

// ---------------------------------------------------------------- main ----

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("zed-reload: {e}");
            exit(2);
        }
    };

    if args.mode == Mode::Check {
        exit(run_check(&args));
    }

    if args.worker {
        let mode_name = match args.mode {
            Mode::Restart => "restart",
            Mode::Send => "send",
            Mode::Watch => "watch",
            Mode::Check => "check",
        };
        let log = Log::new(mode_name);
        exit(run_worker(&args, &log));
    }

    // ---- launcher: write message file, respawn self detached, return ----
    let message = args.message.clone().unwrap_or_else(|| "continue".into());
    let msg_file = env::temp_dir().join(format!(
        "zed-reload-{}-{}.msg",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    if let Err(e) = fs::write(&msg_file, &message) {
        eprintln!("zed-reload: cannot write {}: {e}", msg_file.display());
        exit(2);
    }

    let exe = match env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("zed-reload: current_exe: {e}");
            exit(2);
        }
    };

    let mode_arg = match args.mode {
        Mode::Restart => "--restart",
        Mode::Send => "--send",
        Mode::Watch => "--watch",
        Mode::Check => unreachable!(),
    };
    let mut cmdline = format!(
        "\"{}\" --worker {} --message-file \"{}\" --wait {} --settle {} --grace {} --window-timeout {} --watch-timeout {} --unresponsive {}",
        exe.display(),
        mode_arg,
        msg_file.display(),
        args.wait,
        args.settle,
        args.grace,
        args.window_timeout,
        args.watch_timeout,
        args.unresponsive
    );
    if let Some(p) = &args.project {
        cmdline.push_str(&format!(" --project \"{p}\""));
    }
    if let Some(t) = &args.window_title {
        cmdline.push_str(&format!(" --window-title \"{t}\""));
    }
    if let Some(z) = &args.zed_path {
        cmdline.push_str(&format!(" --zed-path \"{z}\""));
    }
    match args.send_key {
        Some(true) => cmdline.push_str(" --send-ctrl-enter"),
        Some(false) => cmdline.push_str(" --send-enter"),
        None => {}
    }

    let flags = DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB;
    let spawned = spawn_process(&cmdline, flags).or_else(|_| {
        // job may not permit breakaway; retry without it
        spawn_process(
            &cmdline,
            DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP,
        )
    });
    match spawned {
        Ok(pid) => {
            let log = Log::new("launcher");
            println!(
                "zed-reload: launched detached worker pid={} (mode={:?}, wait={}s, settle={}s)",
                pid, args.mode, args.wait, args.settle
            );
            println!("zed-reload: log -> {}", log.path.display());
        }
        Err(e) => {
            eprintln!("zed-reload: LAUNCH FAILED: {e}");
            exit(1);
        }
    }
}
