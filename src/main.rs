//! zed-reload — restart Zed and inject a message into the Agent Panel (Windows).
//!
//! # Architecture
//!
//! Two-process split so the worker survives Zed's death:
//!
//! **Launcher** (the process the user invokes):
//!   1. Writes the message to a temp file.
//!   2. Re-spawns itself detached (`DETACHED_PROCESS | CREATE_NO_WINDOW |
//!      CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB`) with `--worker`
//!      and `--message-file`.
//!   3. Prints the worker PID + log path and exits immediately.
//!
//! **Worker** (the detached process):
//!   1. Reads the message from the temp file (deletes it after).
//!   2. Waits `--wait` seconds.
//!   3. Depending on mode:
//!      - `--send`: inject into the running Zed.
//!      - `--restart` (default): stop Zed, start it via `explorer.exe`,
//!        wait for the window, inject.
//!      - `--watch`: loop, reviving Zed if it dies or hangs.
//!
//! # Zed internals relied on
//!
//! * `Ctrl+Shift+/` = `agent::ToggleFocus`.  On a fresh launch the panel is
//!   not focused, so the first press lands focus in the message editor.
//! * `Enter` sends the message, unless the user's `settings.json` has
//!   `"use_modifier_to_send": true` — then `Ctrl+Enter` is required.
//!   Auto-detected; override with `--send-enter` / `--send-ctrl-enter`.
//! * `restore_on_startup` defaults to `last_session`, so a bare relaunch
//!   reopens the previous workspace including the Agent Panel.
//!
//! # Why `explorer.exe`?
//!
//! Spawning Zed directly leaves it under a console-process ancestry, which
//! tricks its terminal-shell auto-detection into falling back to PowerShell.
//! Launching through `explorer.exe` gives it the standard desktop-shell parent
//! so it auto-detects the user's preferred shell correctly.

mod log;
mod win32;
mod zed;
mod work;

use std::env;
use std::fs;
use std::process::exit;
use std::thread::sleep;
use std::time::Duration;

use clap::Parser;

use crate::log::Log;

// ---------------------------------------------------------------------------
// CLI definition (clap derive)
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "zed-reload",
    version,
    about = "Restart Zed and inject a message into the Agent Panel",
    after_help = "Without a mode flag, --restart is assumed.\n\
                  The log is written to zed-reload.log next to the executable.",
)]
struct Args {
    /// Restart Zed, then inject the message [default].
    #[arg(long, group = "mode")]
    restart: bool,

    /// Inject into the running Zed (no restart).
    #[arg(long, group = "mode")]
    send: bool,

    /// Watch for Zed death / hang, then revive and inject.
    #[arg(long, group = "mode")]
    watch: bool,

    /// Print diagnostics and exit (no side effects).
    #[arg(long, group = "mode")]
    check: bool,

    // ── internal (used by the launcher → worker handoff) ──────────

    /// Internal: this process is the detached worker.
    #[arg(long, hide = true)]
    worker: bool,

    /// Internal: path to the temporary message file.
    #[arg(long, hide = true)]
    message_file: Option<String>,

    // ── timing ────────────────────────────────────────────────────

    /// Seconds to wait before acting.
    #[arg(long, default_value = "6")]
    wait: u64,

    /// Seconds to wait after the Zed window appears.
    #[arg(long, default_value = "10")]
    settle: u64,

    /// Graceful-close budget (seconds) before force-kill.
    #[arg(long, default_value = "20")]
    grace: u64,

    /// Max seconds to wait for the Zed window.
    #[arg(long, default_value = "90")]
    window_timeout: u64,

    /// Watch mode: give up after this many seconds.
    #[arg(long, default_value = "3600")]
    watch_timeout: u64,

    /// Watch mode: revive if a window hangs this many seconds (0 = off).
    #[arg(long, default_value = "0")]
    unresponsive: u64,

    // ── misc ──────────────────────────────────────────────────────

    /// Open this project path instead of relying on session restore.
    #[arg(long)]
    project: Option<String>,

    /// Only target a window whose title contains this substring.
    #[arg(long)]
    window_title: Option<String>,

    /// Explicit path to Zed.exe.
    #[arg(long)]
    zed_path: Option<String>,

    /// Force-send with Enter (override auto-detect).
    #[arg(long, group = "send_key")]
    send_enter: bool,

    /// Force-send with Ctrl+Enter (override auto-detect).
    #[arg(long, group = "send_key")]
    send_ctrl_enter: bool,

    /// The message to inject (all remaining arguments joined with spaces).
    /// Defaults to "continue".
    #[arg(num_args = 0.., allow_hyphen_values = true)]
    message: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Restart,
    Send,
    Watch,
    Check,
}

impl Args {
    fn mode(&self) -> Mode {
        if self.check {
            Mode::Check
        } else if self.send {
            Mode::Send
        } else if self.watch {
            Mode::Watch
        } else {
            Mode::Restart
        }
    }

    fn send_key(&self) -> Option<bool> {
        if self.send_enter {
            Some(false)
        } else if self.send_ctrl_enter {
            Some(true)
        } else {
            None
        }
    }

    fn message(&self) -> String {
        if self.message.is_empty() {
            "continue".into()
        } else {
            self.message.join(" ")
        }
    }
}

// ===================================================================
// main
// ===================================================================

fn main() {
    let args = Args::parse();

    // `--check` is read-only – no side effects.
    if args.mode() == Mode::Check {
        exit(run_check(&args));
    }

    // Worker is the detached process that does the actual work.
    if args.worker {
        exit(run_worker(&args));
    }

    // Launcher: write message → spawn detached worker → return.
    run_launcher(&args);
}

// ===================================================================
// launcher
// ===================================================================

fn run_launcher(args: &Args) {
    let message = args.message();

    // Write to a unique temp file so the message survives re-parsing.
    let msg_file = env::temp_dir().join(format!(
        "zed-reload-{}-{}.msg",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    ));
    if let Err(e) = fs::write(&msg_file, &message) {
        eprintln!("zed-reload: cannot write {}: {e}", msg_file.display());
        exit(2);
    }

    let exe = env::current_exe().unwrap_or_else(|e| {
        eprintln!("zed-reload: current_exe: {e}");
        exit(2);
    });

    let mode_arg = match args.mode() {
        Mode::Restart => "--restart",
        Mode::Send => "--send",
        Mode::Watch => "--watch",
        Mode::Check => unreachable!(),
    };

    // Build the worker command-line.  Paths are quoted for Windows parsing.
    let mut cmdline = format!(
        "\"{}\" --worker {} --message-file \"{}\" \
         --wait {} --settle {} --grace {} --window-timeout {} \
         --watch-timeout {} --unresponsive {}",
        exe.display(),
        mode_arg,
        msg_file.display(),
        args.wait,
        args.settle,
        args.grace,
        args.window_timeout,
        args.watch_timeout,
        args.unresponsive,
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
    if args.send_enter {
        cmdline.push_str(" --send-enter");
    }
    if args.send_ctrl_enter {
        cmdline.push_str(" --send-ctrl-enter");
    }

    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
    };

    let flags = DETACHED_PROCESS
        | CREATE_NO_WINDOW
        | CREATE_NEW_PROCESS_GROUP
        | CREATE_BREAKAWAY_FROM_JOB;

    let spawned = win32::spawn(&cmdline, flags).or_else(|_| {
        // The parent job may forbid breakaway; retry without it.
        win32::spawn(
            &cmdline,
            DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP,
        )
    });

    match spawned {
        Ok(pid) => {
            let log = Log::new("launcher");
            println!(
                "zed-reload: launched detached worker pid={pid} \
                 (mode={:?}, wait={}s, settle={}s)",
                args.mode(),
                args.wait,
                args.settle,
            );
            println!("zed-reload: log -> {}", log.path().display());
        }
        Err(e) => {
            eprintln!("zed-reload: LAUNCH FAILED: {e}");
            exit(1);
        }
    }
}

// ===================================================================
// worker
// ===================================================================

fn run_worker(args: &Args) -> i32 {
    // Prefer the temp file; fall back to positional args.
    let message = if let Some(f) = &args.message_file {
        match fs::read_to_string(f) {
            Ok(m) => {
                let _ = fs::remove_file(f);
                m
            }
            Err(e) => {
                let log = Log::new("worker");
                log.error(&format!("reading message file '{f}': {e}"));
                return 2;
            }
        }
    } else {
        args.message()
    };

    let tag = match args.mode() {
        Mode::Restart => "restart",
        Mode::Send => "send",
        Mode::Watch => "watch",
        Mode::Check => "check",
    };
    let log = Log::new(tag);

    log.info(&format!(
        "=== start: msgLen={} wait={}s settle={}s grace={}s ===",
        message.chars().count(),
        args.wait,
        args.settle,
        args.grace,
    ));

    let ok = match args.mode() {
        Mode::Send => {
            sleep(Duration::from_secs(args.wait));
            work::inject(
                &log,
                &message,
                args.window_timeout,
                args.settle,
                &args.window_title,
                args.send_key(),
            )
        }
        Mode::Restart => {
            sleep(Duration::from_secs(args.wait));
            zed::stop(&log, args.grace);
            match zed::start(&log, &args.zed_path, &args.project) {
                Ok(()) => work::inject(
                    &log,
                    &message,
                    args.window_timeout,
                    args.settle,
                    &args.window_title,
                    args.send_key(),
                ),
                Err(e) => {
                    log.error(&format!("{e}"));
                    false
                }
            }
        }
        Mode::Watch => work::watch(
            &log,
            &message,
            args.watch_timeout,
            args.unresponsive,
            args.grace,
            args.window_timeout,
            args.settle,
            &args.window_title,
            &args.zed_path,
            &args.project,
            args.send_key(),
        ),
        Mode::Check => true, // unreachable — handled before worker dispatch
    };

    log.info(&format!("=== done: ok={ok} ==="));
    if ok { 0 } else { 3 }
}

// ===================================================================
// check (diagnostics)
// ===================================================================

fn run_check(args: &Args) -> i32 {
    let exe_path = zed::find_exe(&args.zed_path);

    println!("zed-reload check");
    println!(
        "  self       : {}",
        env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    );
    match &exe_path {
        Some(p) => println!("  zed exe    : {}", p.display()),
        None => println!("  zed exe    : NOT FOUND"),
    }

    let wins = zed::windows();
    println!("  zed windows: {}", wins.len());
    for w in &wins {
        println!(
            "    pid={} hung={} title='{}'",
            w.pid, w.hung, w.title,
        );
    }

    println!(
        "  send key   : {} (auto-detected)",
        if zed::detect_ctrl_enter() { "ctrl+enter" } else { "enter" },
    );

    let log = Log::new("check");
    println!("  log file   : {}", log.path().display());

    if exe_path.is_some() { 0 } else { 4 }
}
