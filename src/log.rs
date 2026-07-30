//! Minimal file + stdout logger.
//!
//! Writes timestamped entries to `zed-reload.log` next to the exe and prints to
//! stdout (stdout may not exist in the detached worker; write errors are
//! ignored).

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

pub struct Log {
    path: PathBuf,
    tag: String,
    pid: u32,
}

impl Log {
    pub fn new(tag: &str) -> Self {
        let dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            path: dir.join("zed-reload.log"),
            tag: tag.to_string(),
            pid: std::process::id(),
        }
    }

    pub fn info(&self, msg: &str)     { self.emit("INFO", msg); }
    pub fn error(&self, msg: &str)    { self.emit("ERROR", msg); }

    pub fn path(&self) -> &PathBuf { &self.path }

    fn emit(&self, level: &str, msg: &str) {
        let line = format!(
            "[{}] [{}] [{level}] [pid {}] {msg}",
            self.now_local(),
            self.tag,
            self.pid,
        );
        // Best-effort: file may not be writable, stdout may be closed.
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "{line}");
        }
        let _ = writeln!(io::stdout(), "{line}");
        let _ = io::stdout().flush();
    }

    fn now_local(&self) -> String {
        unsafe {
            use windows_sys::Win32::Foundation::SYSTEMTIME;
            use windows_sys::Win32::System::SystemInformation::GetLocalTime;
            let mut st: SYSTEMTIME = std::mem::zeroed();
            GetLocalTime(&mut st);
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond,
            )
        }
    }
}
