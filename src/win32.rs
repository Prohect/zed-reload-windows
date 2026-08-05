//! Safe wrappers around the Windows API calls needed by zed-reload.
//!
//! All `unsafe` FFI is confined to this module.  Public functions take and
//! return plain Rust types where practical.

use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use windows_sys::core::{GUID, HRESULT, PCWSTR, PWSTR};

use crate::log::Log;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows_sys::Win32::System::Threading::{
    AttachThreadInput, CreateProcessW, GetCurrentThreadId, GetProcessTimes, GetStartupInfoW,
    OpenProcess, QueryFullProcessImageNameW, TerminateProcess, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, STARTUPINFOW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// ---------------------------------------------------------------------------
// wide-string helpers
// ---------------------------------------------------------------------------

pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

// ---------------------------------------------------------------------------
// process enumeration
// ---------------------------------------------------------------------------

/// Return PIDs of every process whose `.exe` name equals `name`
/// (case-insensitive).
pub fn find_pids(name: &str) -> Vec<u32> {
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
                if from_wide(&entry.szExeFile).eq_ignore_ascii_case(name) {
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

/// Parent PID of `pid` (`None` when the process is gone).
pub fn parent_pid(pid: u32) -> Option<u32> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE || snap.is_null() {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut parent = None;
        if Process32FirstW(snap, &mut entry) == TRUE {
            loop {
                if entry.th32ProcessID == pid {
                    parent = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32NextW(snap, &mut entry) != TRUE {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        parent
    }
}

/// Full executable path of a process (e.g. `C:\...\Zed.exe`), or `None`
/// when the process cannot be opened.
pub fn exe_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
        let _ = CloseHandle(h);
        if ok == FALSE {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// Current time as FILETIME ticks (100 ns since 1601-01-01 UTC).
pub fn now_filetime() -> u64 {
    unsafe {
        let mut ft: FILETIME = std::mem::zeroed();
        GetSystemTimeAsFileTime(&mut ft);
        ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
    }
}

/// Creation time of a process as FILETIME ticks (same clock as
/// `now_filetime`), or `None` when the process cannot be opened.
pub fn start_time(pid: u32) -> Option<u64> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
        if h.is_null() {
            return None;
        }
        let mut creation: FILETIME = std::mem::zeroed();
        let mut exit: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        let ok = GetProcessTimes(h, &mut creation, &mut exit, &mut kernel, &mut user);
        let _ = CloseHandle(h);
        if ok == FALSE {
            return None;
        }
        Some(((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64)
    }
}

// ---------------------------------------------------------------------------
// window enumeration
// ---------------------------------------------------------------------------

pub struct Window {
    pub hwnd: HWND,
    pub pid: u32,
    pub title: String,
    pub hung: bool,
}

/// Enumerate visible top-level windows owned by any of `pids`.
/// Prefers windows with a non-empty title when both kinds exist.
pub fn enum_windows(pids: &[u32]) -> Vec<Window> {
    struct Ctx {
        pids: Vec<u32>,
        wins: Vec<Window>,
    }

    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam as *mut Ctx);
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != 0 && ctx.pids.contains(&pid) && IsWindowVisible(hwnd) == TRUE {
            let len = GetWindowTextLengthW(hwnd);
            let title = if len > 0 {
                let mut buf = vec![0u16; (len + 1) as usize];
                GetWindowTextW(hwnd, buf.as_mut_ptr(), len + 1);
                from_wide(&buf)
            } else {
                String::new()
            };
            ctx.wins.push(Window {
                hwnd,
                pid,
                title,
                hung: IsHungAppWindow(hwnd) == TRUE,
            });
        }
        TRUE
    }

    let mut ctx = Ctx {
        pids: pids.to_vec(),
        wins: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(Some(callback), &mut ctx as *mut Ctx as LPARAM);
    }
    // Prefer titled windows.
    if ctx.wins.iter().any(|w| !w.title.is_empty()) {
        ctx.wins.retain(|w| !w.title.is_empty());
    }
    ctx.wins
}

// ---------------------------------------------------------------------------
// clipboard (Unicode text only)
// ---------------------------------------------------------------------------

/// Standard clipboard format constant (not in `Win32_System_DataExchange`).
const CF_UNICODETEXT: u32 = 13;

pub fn clipboard_get() -> Option<String> {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) != TRUE {
            return None;
        }
        let result = (|| {
            let h = GetClipboardData(CF_UNICODETEXT);
            if h.is_null() {
                return None;
            }
            let ptr = GlobalLock(h as _);
            if ptr.is_null() {
                return None;
            }
            let size = GlobalSize(h as _) as usize / 2;
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
        result
    }
}

/// Set the clipboard to `text`.  Retries up to 10 times if another process
/// has the clipboard open.
pub fn clipboard_set(text: &str) -> Result<(), String> {
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

// ---------------------------------------------------------------------------
// keyboard input
// ---------------------------------------------------------------------------

/// Press each key down in order, then release in reverse.
pub fn send_combo(vks: &[u16]) {
    let n = vks.len();
    let mut inputs: Vec<INPUT> = Vec::with_capacity(n * 2);
    for &vk in vks {
        inputs.push(keybd(vk, 0));
    }
    for &vk in vks.iter().rev() {
        inputs.push(keybd(vk, KEYEVENTF_KEYUP));
    }
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
    }
}

fn keybd(vk: u16, flags: u32) -> INPUT {
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

// ---------------------------------------------------------------------------
// startup info
// ---------------------------------------------------------------------------

/// The `wShowWindow` value this process was launched with
/// (`STARTUPINFO`), or 0 if the launcher did not set one.
pub fn startup_show_cmd() -> u16 {
    unsafe {
        let mut info: STARTUPINFOW = std::mem::zeroed();
        info.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        GetStartupInfoW(&mut info);
        info.wShowWindow
    }
}

// ---------------------------------------------------------------------------
// .lnk shortcut writing (IShellLinkW / IPersistFile, raw vtables)
//
// `explorer.exe` mangles multi-argument command lines (it joins the
// arguments into a single path and silently launches nothing), so a project
// launch must pass target + arguments as a single explorer argument: a
// temporary .lnk shortcut.  The shortcut's "Run: Minimized" is NOT honored
// by explorer (it delivers SW_SHOWDEFAULT — see
// `shortcut_delivers_show_cmd_and_workdir`); the worker minimizes the Zed
// window after it appears instead.  windows-sys does not generate COM
// interface definitions, so the vtables are declared by hand.

const CLSID_SHELL_LINK: GUID = GUID::from_u128(0x00021401_0000_0000_c000_000000000046);
const IID_ISHELLLINKW: GUID = GUID::from_u128(0x000214F9_0000_0000_c000_000000000046);
const IID_IPERSISTFILE: GUID = GUID::from_u128(0x0000010B_0000_0000_c000_000000000046);

type HrFn3 = unsafe extern "system" fn(*mut core::ffi::c_void) -> HRESULT;

/// `IShellLinkW` vtable (IUnknown + 18 IShellLink methods, SDK order).
#[repr(C)]
struct IShellLinkWVtbl {
    query_interface:
        unsafe extern "system" fn(*mut core::ffi::c_void, *const GUID, *mut *mut core::ffi::c_void) -> HRESULT,
    add_ref: HrFn3,
    release: HrFn3,
    get_path: unsafe extern "system" fn(*mut core::ffi::c_void, PWSTR, u32, *mut core::ffi::c_void, u32) -> HRESULT,
    get_id_list: unsafe extern "system" fn(*mut core::ffi::c_void, *mut *mut core::ffi::c_void) -> HRESULT,
    set_id_list: unsafe extern "system" fn(*mut core::ffi::c_void, *const core::ffi::c_void) -> HRESULT,
    get_description: unsafe extern "system" fn(*mut core::ffi::c_void, PWSTR, u32) -> HRESULT,
    set_description: unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR) -> HRESULT,
    get_working_directory: unsafe extern "system" fn(*mut core::ffi::c_void, PWSTR, u32) -> HRESULT,
    set_working_directory: unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR) -> HRESULT,
    get_arguments: unsafe extern "system" fn(*mut core::ffi::c_void, PWSTR, u32) -> HRESULT,
    set_arguments: unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR) -> HRESULT,
    get_hotkey: unsafe extern "system" fn(*mut core::ffi::c_void, *mut u16) -> HRESULT,
    set_hotkey: unsafe extern "system" fn(*mut core::ffi::c_void, u16) -> HRESULT,
    get_show_cmd: unsafe extern "system" fn(*mut core::ffi::c_void, *mut i32) -> HRESULT,
    set_show_cmd: unsafe extern "system" fn(*mut core::ffi::c_void, i32) -> HRESULT,
    get_icon_location: unsafe extern "system" fn(*mut core::ffi::c_void, PWSTR, u32, *mut i32) -> HRESULT,
    set_icon_location: unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR, i32) -> HRESULT,
    set_relative_path: unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR, u32) -> HRESULT,
    resolve: unsafe extern "system" fn(*mut core::ffi::c_void, HWND, u32) -> HRESULT,
    set_path: unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR) -> HRESULT,
}

/// `IPersistFile` vtable (IUnknown + IPersist::GetClassID + 5 IPersistFile methods).
#[repr(C)]
struct IPersistFileVtbl {
    query_interface:
        unsafe extern "system" fn(*mut core::ffi::c_void, *const GUID, *mut *mut core::ffi::c_void) -> HRESULT,
    add_ref: HrFn3,
    release: HrFn3,
    get_class_id: unsafe extern "system" fn(*mut core::ffi::c_void, *mut GUID) -> HRESULT,
    is_dirty: HrFn3,
    load: unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR, u32) -> HRESULT,
    save: unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR, BOOL) -> HRESULT,
    save_completed: unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR) -> HRESULT,
    get_cur_file: unsafe extern "system" fn(*mut core::ffi::c_void, *mut *mut u16) -> HRESULT,
}

/// Write a `.lnk` shortcut for `exe` with `args` and working directory
/// `workdir` to `lnk_path`.
pub fn write_shortcut(
    exe: &Path,
    args: &str,
    workdir: &str,
    lnk_path: &Path,
) -> Result<(), String> {
    use windows_sys::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };

    unsafe {
        let hr = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        if hr != 0 && hr != 1 {
            return Err(format!("CoInitializeEx failed, HRESULT={hr:#010x}"));
        }
        let result = (|| -> Result<(), String> {
            let mut link_raw: *mut core::ffi::c_void = std::ptr::null_mut();
            let hr = CoCreateInstance(
                &CLSID_SHELL_LINK,
                std::ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_ISHELLLINKW,
                &mut link_raw,
            );
            if hr != 0 {
                return Err(format!("CoCreateInstance(ShellLink) failed, HRESULT={hr:#010x}"));
            }
            let link_vtbl = &**(link_raw as *const *const IShellLinkWVtbl);

            let exe_wide = to_wide(&exe.to_string_lossy());
            let args_wide = to_wide(args);
            let workdir_wide = to_wide(workdir);
            let hr = (link_vtbl.set_path)(link_raw, exe_wide.as_ptr());
            if hr != 0 {
                return Err(format!("IShellLink::SetPath failed, HRESULT={hr:#010x}"));
            }
            let hr = (link_vtbl.set_arguments)(link_raw, args_wide.as_ptr());
            if hr != 0 {
                return Err(format!("IShellLink::SetArguments failed, HRESULT={hr:#010x}"));
            }
            let hr = (link_vtbl.set_working_directory)(link_raw, workdir_wide.as_ptr());
            if hr != 0 {
                return Err(format!(
                    "IShellLink::SetWorkingDirectory failed, HRESULT={hr:#010x}"
                ));
            }

            let mut persist_raw: *mut core::ffi::c_void = std::ptr::null_mut();
            let hr = (link_vtbl.query_interface)(link_raw, &IID_IPERSISTFILE, &mut persist_raw);
            if hr != 0 {
                return Err(format!(
                    "QueryInterface(IPersistFile) failed, HRESULT={hr:#010x}"
                ));
            }
            let persist_vtbl = &**(persist_raw as *const *const IPersistFileVtbl);
            let lnk_wide = to_wide(&lnk_path.to_string_lossy());
            let hr = (persist_vtbl.save)(persist_raw, lnk_wide.as_ptr(), 1);
            (persist_vtbl.release)(persist_raw);
            if hr != 0 {
                return Err(format!("IPersistFile::Save failed, HRESULT={hr:#010x}"));
            }
            (link_vtbl.release)(link_raw);
            Ok(())
        })();
        CoUninitialize();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

    /// Smoke test for `write_shortcut` + explorer launch: a shortcut for
    /// `wscript.exe` runs a `.vbs` that drops a marker file.  Needs a real
    /// interactive desktop, so it is ignored by default.
    #[test]
    #[ignore = "launches processes on the real desktop"]
    fn shortcut_launch_via_explorer() {
        let dir = std::env::temp_dir().join(format!("zed-reload-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("marker.txt");
        let vbs = dir.join("mark.vbs");
        fs::write(
            &vbs,
            format!(
                "Set fso = CreateObject(\"Scripting.FileSystemObject\")\n\
                 Set f = fso.CreateTextFile(\"{path}\", True)\n\
                 f.Write \"ok\"\n\
                 f.Close\n",
                path = marker.to_string_lossy(),
            ),
        )
        .unwrap();

        let lnk = dir.join("test.lnk");
        write_shortcut(
            Path::new(r"C:\Windows\System32\wscript.exe"),
            &format!("\"{}\"", vbs.display()),
            &dir.to_string_lossy(),
            &lnk,
        )
        .unwrap();
        spawn(
            &format!("explorer.exe \"{}\"", lnk.display()),
            CREATE_NEW_PROCESS_GROUP,
        )
        .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !marker.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "marker file never appeared — explorer shortcut launch failed"
            );
            sleep(Duration::from_millis(250));
        }
        assert_eq!(fs::read_to_string(&marker).unwrap(), "ok");
        sleep(Duration::from_millis(500)); // let explorer release the .lnk
        let _ = fs::remove_dir_all(&dir);
    }

    /// What does the shell actually deliver to a shortcut-launched process?
    /// Launches the release binary via `explorer.exe <lnk>` and asks it to
    /// dump `STARTUPINFO.wShowWindow` and its cwd (the `.lnk` has a working
    /// directory, so this also reveals whether explorer honors it).  If the
    /// arguments get mangled and the binary runs as a plain launcher, the
    /// stray worker is killed before its `--wait` elapses.
    #[test]
    #[ignore = "launches processes on the real desktop"]
    fn shortcut_delivers_show_cmd_and_workdir() {
        let exe = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/release/zed-reload.exe");
        assert!(exe.exists(), "build --release first: {}", exe.display());
        let out = std::env::temp_dir().join("zed-reload-startup-cmd.txt");
        let _ = fs::remove_file(&out);
        let lnk = std::env::temp_dir().join("zed-reload-delivery-test.lnk");

        write_shortcut(
            &exe,
            &format!("--dump-startup \"{}\"", out.display()),
            // Deliberately distinct from explorer's own cwd (C:\Windows...)
            &env!("CARGO_MANIFEST_DIR").replace('/', "\\"),
            &lnk,
        )
        .unwrap();
        spawn(
            &format!("explorer.exe \"{}\"", lnk.display()),
            CREATE_NEW_PROCESS_GROUP,
        )
        .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while !out.exists() {
            if std::time::Instant::now() > deadline {
                // The launch got mangled into a plain launcher invocation;
                // kill it before its worker acts on Zed.
                for pid in super::super::win32::find_pids("zed-reload.exe") {
                    let _ = kill(pid);
                }
                panic!("dump file missing; killed stray zed-reload processes");
            }
            sleep(Duration::from_millis(250));
        }
        let content = fs::read_to_string(&out).unwrap();
        let _ = fs::remove_file(&out);
        let _ = fs::remove_file(&lnk);
        // The shortcut carries "Run: Minimized" (SW_SHOWMINIMIZED) and a
        // working directory.  Explorer does NOT deliver the show-cmd through
        // `explorer.exe "file.lnk"` on this system (it arrives as
        // SW_SHOWDEFAULT=10), so the injection flow must not depend on a
        // minimized start — but the working directory must be delivered,
        // since the relaunch depends on it.
        assert!(
            content.contains(&format!(
                "cwd={}",
                env!("CARGO_MANIFEST_DIR").replace('/', "\\")
            )),
            "expected shortcut workdir delivered, got: {content}"
        );
        println!("delivered: {content}");
    }
}

// ---------------------------------------------------------------------------
// notification balloon
// ---------------------------------------------------------------------------

/// Show a Windows notification balloon anchored to the system tray.
///
/// Unlike a focus grab it never steals focus, and it works even when Zed
/// is already the foreground window (where the old focus-based heads-up
/// was a no-op).  The balloon is anchored to a hidden window + tray icon;
/// the OS removes both when this process exits, which happens after the
/// injection completes.
pub fn notify_balloon(title: &str, text: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::{
        NIF_ICON, NIF_INFO, NIF_MESSAGE, NIIF_INFO, NIM_ADD, NOTIFYICONDATAW, Shell_NotifyIconW,
    };

    unsafe {
        let hinstance = GetModuleHandleW(std::ptr::null());
        let class_name = to_wide("zed-reload-balloon");
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(DefWindowProcW);
        wc.hInstance = hinstance;
        wc.lpszClassName = class_name.as_ptr();
        if RegisterClassExW(&wc) == 0 {
            return Err(format!("RegisterClassExW failed, GLE={}", GetLastError()));
        }
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            std::ptr::null(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null_mut(),
        );
        if hwnd.is_null() {
            return Err(format!("CreateWindowExW failed, GLE={}", GetLastError()));
        }

        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_ICON | NIF_INFO | NIF_MESSAGE;
        nid.uCallbackMessage = WM_USER + 1;
        nid.hIcon = LoadIconW(std::ptr::null_mut(), IDI_APPLICATION);
        nid.dwInfoFlags = NIIF_INFO;

        // szInfoTitle holds 63 chars, szInfo 255 (excluding the NUL).
        let title_wide: Vec<u16> = title
            .encode_utf16()
            .take(63)
            .chain(std::iter::once(0))
            .collect();
        let text_wide: Vec<u16> = text
            .encode_utf16()
            .take(255)
            .chain(std::iter::once(0))
            .collect();
        nid.szInfoTitle[..title_wide.len()].copy_from_slice(&title_wide);
        nid.szInfo[..text_wide.len()].copy_from_slice(&text_wide);

        if Shell_NotifyIconW(NIM_ADD, &nid) == 0 {
            return Err(format!("Shell_NotifyIconW failed, GLE={}", GetLastError()));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// process control
// ---------------------------------------------------------------------------

/// Post `WM_CLOSE` to a window handle (graceful close request).
pub fn post_quit(hwnd: HWND) {
    unsafe {
        let _ = PostMessageW(hwnd, WM_CLOSE, 0, 0);
    }
}

/// The window currently in the foreground (may be null).
pub fn get_foreground() -> HWND {
    unsafe { GetForegroundWindow() }
}

/// Force-terminate a process by PID.
pub fn kill(pid: u32) {
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, FALSE, pid);
        if !h.is_null() {
            let _ = TerminateProcess(h, 1);
            let _ = CloseHandle(h);
        }
    }
}

/// Spawn a process from `cmdline` with the given creation flags.
/// Returns the new process ID.
pub fn spawn(cmdline: &str, flags: u32) -> Result<u32, String> {
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
            flags,
            std::ptr::null(),
            std::ptr::null(),
            &si,
            &mut pi,
        );
        if ok == FALSE {
            let err = GetLastError();
            return Err(format!("CreateProcessW failed, GLE={err}"));
        }
        let _ = CloseHandle(pi.hProcess);
        let _ = CloseHandle(pi.hThread);
        Ok(pi.dwProcessId)
    }
}

/// True if `hwnd` is minimized (iconic).
pub fn is_minimized(hwnd: HWND) -> bool {
    unsafe { IsIconic(hwnd) == TRUE }
}

/// Minimize `hwnd` (no-op if already minimized).
pub fn minimize(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_MINIMIZE);
    }
}

// ---------------------------------------------------------------------------
// foreground window
// ---------------------------------------------------------------------------

/// Aggressively bring `hwnd` to the foreground.
///
/// Tries (in order, up to 15 rounds):
/// 1. `SwitchToThisWindow`
/// 2. `AttachThreadInput` + `SetForegroundWindow` + `BringWindowToTop`
/// 3. Phantom `VK_F15` input (satisfies foreground lock) + `SetForegroundWindow`
///
/// Returns `true` if the window ended up in the foreground.
pub fn force_foreground(log: &Log, hwnd: HWND) -> bool {
    for attempt in 1..=15u32 {
        unsafe {
            if GetForegroundWindow() == hwnd {
                log.info(&format!(
                    "foreground=true after {} attempt(s)",
                    attempt - 1,
                ));
                return true;
            }
            if IsIconic(hwnd) == TRUE {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }

            // Strategy 1
            SwitchToThisWindow(hwnd, TRUE);
            sleep(Duration::from_millis(200));
            if GetForegroundWindow() == hwnd {
                log.info(&format!(
                    "foreground=true via SwitchToThisWindow (attempt {attempt})",
                ));
                return true;
            }

            // Strategy 2 — attach to foreground thread
            let fg = GetForegroundWindow();
            let mut _fg_pid: u32 = 0;
            let fg_thread = GetWindowThreadProcessId(fg, &mut _fg_pid);
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
                log.info(&format!(
                    "foreground=true via AttachThreadInput (attempt {attempt})",
                ));
                return true;
            }

            // Strategy 3 — phantom F15 key
            send_combo(&[VK_F15]);
            let _ = SetForegroundWindow(hwnd);
            sleep(Duration::from_millis(250));
            if GetForegroundWindow() == hwnd {
                log.info(&format!(
                    "foreground=true via F15+SetForegroundWindow (attempt {attempt})",
                ));
                return true;
            }
        }
    }
    log.info("foreground=false after 15 attempts");
    false
}
