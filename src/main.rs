//! zed-reload — restart Zed and inject a message into the Agent Panel (Windows).
//!
//! # Architecture
//!
//! Two-process split so the worker survives Zed's death:
//!
//! **Launcher** (the process the user invokes):
//!   1. Writes the message to a temp file.
//!   2. Resolves the running Zed's exe path — the closest Zed *ancestor* —
//!      so the worker restarts the same variant (Release, Preview, Nightly,
//!      dev).  Several running Zeds without an ancestor is ambiguous: the
//!      launcher aborts and asks for `--zed-path`.
//!   3. Reads the user's Zed config — editor mode (Vim/Helix/Normal), send
//!      key, custom bindings — from `settings.json` / `keymap.json`, see
//!      "Zed internals relied on" below.
//!   4. Re-spawns itself detached (`DETACHED_PROCESS | CREATE_NO_WINDOW |
//!      CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB`) with
//!      `--worker`, `--message-file`, `--zed-path` and the resolved keys
//!      (`--mode`, `--toggle-keys`, `--paste-keys`, `--send-enter` /
//!      `--send-ctrl-enter`).
//!   5. Prints the worker PID + log path and exits immediately.
//!
//! **Worker** (the detached process):
//!   1. Reads the message from the temp file (deletes it after).
//!   2. Waits `--wait` seconds.
//!   3. Stops only the target Zed process (`--zed-pid`; default: the
//!      invoking ancestor — other Zed processes, same variant included,
//!      keep running), then starts the resolved exe via `explorer.exe`
//!      (desktop parent — keeps the terminal-shell auto-detection on the
//!      user's shell; direct spawns regress it to PowerShell).  If the old
//!      session was not focused the worker minimizes the new window as
//!      soon as it appears, so the launch cannot hold focus.
//!   4. Single-focus injection:
//!      a. **Windows notification** — a tray balloon warns the user
//!         ("stop typing, something is about to happen"); unlike a focus
//!         grab it never steals focus and works even when Zed is front.
//!      b. **Heads-up delay** (`--heads-up` seconds) — reaction time.
//!      c. **Single focus** — brings Zed to the foreground (restoring it
//!         from minimized), then injects the message (toggle binding,
//!         quick-paste, send).  When the old session was not focused the
//!         window is minimized again once the injection is done, so the
//!         relaunch never holds focus.
//!
//! # Zed internals relied on
//!
//! * `agent::ToggleFocus` — default `Ctrl+Shift+/`; the launcher reads the
//!   user's `keymap.json` for a custom binding and hands it to the worker.
//!   On a fresh launch the panel is not focused, so the first press lands
//!   focus in the message editor.
//! * `Enter` sends the message, unless the user's `settings.json` has
//!   `"use_modifier_to_send": true` — then `Ctrl+Enter` is required.
//!   Auto-detected; override with `--send-enter` / `--send-ctrl-enter`.
//! * The Agent Panel message editor is a regular editor.  In Vim/Helix mode
//!   it stays in Normal mode — both modes have a quick-paste key that reads
//!   the Windows clipboard via `editor::Paste`: Helix `shift-r` (the fork's
//!   helix keymap), Vim `shift-insert` (the Windows editor default, which
//!   Vim does not shadow — `ctrl-v` is `vim::ToggleVisualBlock` there).
//!   The worker pastes directly in Normal mode with these keys; custom
//!   `editor::Paste` bindings from the user's keymap.json win over the
//!   mode defaults.
//! * `settings.json` (`vim_mode`, `helix_mode`,
//!   `agent.use_modifier_to_send`) and `keymap.json` (custom
//!   `agent::ToggleFocus` / `editor::Paste` bindings) are read from
//!   `%APPDATA%\Zed` by the launcher, which passes the resolved values to
//!   the worker explicitly; override the directory with `--config-dir`.
//! * The worker reopens the *calling* project path (the launcher's cwd),
//!   not whatever session-restore puts in front.  With several projects
//!   open, session restore may bring a different window to the front than
//!   the one the agent invoked zed-reload from, so recovery would land in
//!   the wrong session.  The injection therefore targets the window whose
//!   title contains the calling project's folder name (Zed titles windows
//!   with the project folder name) — recovery is strictly the agent that
//!   invoked zed-reload; no match means no injection.
//!
//! # Minimized start
//!
//! The relaunched Zed window cannot be *created* minimized from outside:
//! explorer delivers `SW_SHOWDEFAULT` regardless of the shortcut's
//! "Run: Minimized" (verified by the delivery test), Zed rejects unknown
//! CLI flags (e.g. `--minimized`), and gpui ignores
//! `STARTUPINFO.wShowWindow` (verified: a direct spawn with
//! `SW_SHOWMINIMIZED` still opened a normal window).  The worker therefore
//! minimizes the window the moment it appears — a brief flash to the front
//! is unavoidable — unless the old session was focused (the user is already
//! in Zed; nothing to protect).  The injection focus restores it via
//! `SW_RESTORE`; once the message is in, the window is minimized again
//! (old session not focused), so the whole flow never holds focus.
//!
//! # Why `explorer.exe`?
//!
//! Spawning Zed directly (even from the detached, console-less worker)
//! leaves it under a console-process ancestry, which tricks its
//! terminal-shell auto-detection into falling back to PowerShell instead of
//! the user's preferred shell — verified empirically.  Launching through
//! `explorer.exe` gives it the standard desktop-shell parent so it
//! auto-detects the shell correctly.

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
    after_help = "The log is written to zed-reload.log next to the executable.",
)]
struct Args {
    /// Print diagnostics and exit (no side effects).
    #[arg(long)]
    check: bool,

    // ── internal (used by the launcher → worker handoff) ──────────

    /// Internal: this process is the detached worker.
    #[arg(long, hide = true)]
    worker: bool,

    /// Internal: write GetStartupInfoW().wShowWindow to a file and exit.
    #[arg(long, hide = true)]
    dump_startup: Option<String>,

    /// Internal: path to the temporary message file.
    #[arg(long, hide = true)]
    message_file: Option<String>,

    // ── timing ────────────────────────────────────────────────────

    /// Seconds to wait before acting.
    #[arg(long, default_value = "26")]
    wait: u64,

    /// Seconds to wait after the Zed window appears (session-restore, etc.).
    #[arg(long, default_value = "10")]
    settle: u64,

    /// Graceful-close budget (seconds) before force-kill.
    #[arg(long, default_value = "42")]
    grace: u64,

    /// Max seconds to wait for the Zed window.
    #[arg(long, default_value = "16")]
    window_timeout: u64,

    /// Seconds between the heads-up focus and the injection focus.  Gives the
    /// user time to stop any keyboard/mouse activity that could interfere.
    #[arg(long, default_value = "2")]
    heads_up: u64,

    // ── misc ──────────────────────────────────────────────────────

    /// Project path to reopen (default: the current working directory, so
    /// the restarted session is the one that invoked zed-reload).
    #[arg(long)]
    project: Option<String>,

    /// Only target a window whose title contains this substring.
    #[arg(long)]
    window_title: Option<String>,

    /// Explicit path to Zed.exe (what to start; default: the target
    /// process's own exe).
    #[arg(long)]
    zed_path: Option<String>,

    /// Only stop this Zed process (default: the closest Zed ancestor).
    /// Other Zed processes — same variant included — keep running.
    #[arg(long)]
    zed_pid: Option<u32>,

    /// Force-send with Enter (override auto-detect).
    #[arg(long, group = "send_key")]
    send_enter: bool,

    /// Force-send with Ctrl+Enter (override auto-detect).
    #[arg(long, group = "send_key")]
    send_ctrl_enter: bool,

    /// Zed config directory (default: %APPDATA%\Zed).  settings.json and
    /// keymap.json are read from here.
    #[arg(long)]
    config_dir: Option<String>,

    /// Editor mode override: normal, vim, or helix (default: inferred from
    /// settings.json's vim_mode / helix_mode).
    #[arg(long, value_enum)]
    mode: Option<zed::EditorMode>,

    /// agent::ToggleFocus keybinding override, e.g. "ctrl-shift-," (default:
    /// inferred from keymap.json).
    #[arg(long)]
    toggle_keys: Option<String>,

    /// Paste keybinding override, e.g. "shift-r" (default: inferred from
    /// keymap.json, else the mode default).
    #[arg(long)]
    paste_keys: Option<String>,

    /// The message to inject (all remaining arguments joined with spaces).
    /// Defaults to "continue".
    #[arg(num_args = 0.., allow_hyphen_values = true)]
    message: Vec<String>,
}

impl Args {
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
// injection plan (launcher-side config resolution)
// ===================================================================

/// The fully-resolved injection plan: mode, send key, and the keybindings
/// the worker must send.  Inferred from the user's Zed config (settings.json
/// + keymap.json), with per-flag overrides on top.
struct InjectPlan {
    mode: zed::EditorMode,
    ctrl_enter: bool,
    toggle_binding: String,
    paste_binding: String,
}

fn plan_inject(args: &Args) -> InjectPlan {
    let cfg = zed::load_config(&args.config_dir);
    let mode = args.mode.unwrap_or(cfg.mode);
    InjectPlan {
        mode,
        ctrl_enter: args.send_key().unwrap_or(cfg.ctrl_enter_to_send),
        toggle_binding: args.toggle_keys.clone().unwrap_or(cfg.toggle_binding),
        paste_binding: args
            .paste_keys
            .clone()
            .or(cfg.paste_binding)
            .unwrap_or_else(|| mode.default_paste_binding()),
    }
}

/// Resolve the worker's injection keys from its (launcher-supplied) args.
/// A standalone worker falls back to Zed defaults.
fn worker_keys(args: &Args) -> work::InjectKeys {
    let mode = args.mode.unwrap_or(zed::EditorMode::Normal);
    let toggle_binding = args.toggle_keys.clone().unwrap_or_else(|| "ctrl-shift-/".into());
    let paste_binding = args
        .paste_keys
        .clone()
        .unwrap_or_else(|| mode.default_paste_binding());
    work::InjectKeys {
        ctrl_enter: args.send_key().unwrap_or(false),
        toggle: zed::binding_to_vk(&toggle_binding, "ctrl-shift-/"),
        paste: zed::binding_to_vk(&paste_binding, &mode.default_paste_binding()),
    }
}

// ===================================================================
// main
// ===================================================================

fn main() {
    let args = Args::parse();

    // `--check` is read-only – no side effects.
    if args.check {
        exit(run_check(&args));
    }

    // Worker is the detached process that does the actual work.
    if args.worker {
        exit(run_worker(&args));
    }

    // Diagnostic: report the show-state and cwd this process was launched
    // with (used to verify what the shell actually delivers to a process
    // launched through a shortcut).
    if let Some(f) = &args.dump_startup {
        let _ = fs::write(
            f,
            format!(
                "show={} cwd={}",
                win32::startup_show_cmd(),
                env::current_dir().map(|d| d.display().to_string()).unwrap_or_default(),
            ),
        );
        exit(0);
    }

    // Launcher: write message → spawn detached worker → return.
    run_launcher(&args);
}

// ===================================================================
// launcher
// ===================================================================

fn run_launcher(args: &Args) {
    let message = args.message();

    // Restart the *same* Zed instance that invoked us: the closest Zed
    // ancestor is the target — only that process is stopped (other Zeds,
    // same variant included, keep running) and its own exe is what gets
    // started again (Release, Preview, Nightly, or a dev build).  Must be
    // resolved here in the launcher — by the time the worker acts, the
    // target may already be dead.  Without a Zed ancestor, several running
    // Zeds are ambiguous -> hard error (unless --zed-path/--zed-pid was
    // given).  No running Zed at all -> the worker's on-disk search starts
    // one; nothing is stopped.
    let target = zed::resolve_target(&args.zed_path, args.zed_pid).unwrap_or_else(|e| {
        eprintln!("zed-reload: {e}");
        exit(2);
    });

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

    // Build the worker command-line.  Paths are quoted for Windows parsing.
    let mut cmdline = format!(
        "\"{}\" --worker --message-file \"{}\" \
         --wait {} --settle {} --grace {} --window-timeout {} --heads-up {}",
        exe.display(),
        msg_file.display(),
        args.wait,
        args.settle,
        args.grace,
        args.window_timeout,
        args.heads_up,
    );
    // Resolve the user's Zed config (editor mode, send key, custom
    // bindings) here in the launcher and pass the result explicitly to the
    // worker — the worker must not re-read config files, and the effective
    // values are visible in `--check`.
    let plan = plan_inject(args);
    cmdline.push_str(&format!(
        " --mode {} --toggle-keys \"{}\" --paste-keys \"{}\"",
        plan.mode, plan.toggle_binding, plan.paste_binding,
    ));
    cmdline.push_str(if plan.ctrl_enter { " --send-ctrl-enter" } else { " --send-enter" });
    // The restarted Zed must land in *this* session's project.  Zed's
    // default `restore_on_startup = last_session` may bring a different
    // window to the front when several projects are open, so the
    // reload-recovery-continue flow would resume the wrong session.
    // Pass the calling project ($pwd) explicitly; --project overrides.
    let project = match &args.project {
        Some(p) => Some(p.clone()),
        None => env::current_dir()
            .map(|d| d.to_string_lossy().into_owned())
            .ok(),
    };
    if let Some(p) = &project {
        cmdline.push_str(&format!(" --project \"{p}\""));
    }
    if let Some(t) = &args.window_title {
        cmdline.push_str(&format!(" --window-title \"{t}\""));
    }
    if let Some(t) = &target {
        cmdline.push_str(&format!(" --zed-path \"{}\"", t.exe.display()));
        if let Some(pid) = t.pid {
            cmdline.push_str(&format!(" --zed-pid {pid}"));
        }
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
                "zed-reload: detached worker pid={pid}  (wait={}s, settle={}s, heads-up={}s)",
                args.wait,
                args.settle,
                args.heads_up,
            );
            println!("zed-reload: mode      -> {}", plan.mode);
            println!(
                "zed-reload: send key  -> {}",
                if plan.ctrl_enter { "ctrl+enter" } else { "enter" },
            );
            println!("zed-reload: toggle    -> {}", plan.toggle_binding);
            println!("zed-reload: paste     -> {}", plan.paste_binding);
            match &project {
                Some(p) => println!("zed-reload: project -> {p}"),
                None => println!("zed-reload: project -> (session restore)"),
            }
            match &target {
                Some(t) => {
                    println!("zed-reload: zed exe  -> {}", t.exe.display());
                    match t.pid {
                        Some(pid) => println!("zed-reload: target   -> pid {pid}"),
                        None => println!("zed-reload: target   -> (none — start only)"),
                    }
                }
                None => println!("zed-reload: zed exe  -> (on-disk search)"),
            }
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

    let log = Log::new("restart");

    log.info(&format!(
        "=== start: msgLen={} wait={}s settle={}s grace={}s heads-up={}s ===",
        message.chars().count(),
        args.wait,
        args.settle,
        args.grace,
        args.heads_up,
    ));

    // The launcher resolved the user's config into explicit keys (see
    // `plan_inject`); a standalone worker falls back to Zed defaults.
    let keys = worker_keys(args);
    log.info(&format!(
        "mode={} toggle={} paste={} send={}",
        args.mode.unwrap_or(zed::EditorMode::Normal),
        args.toggle_keys.clone().unwrap_or_else(|| "ctrl-shift-/".into()),
        args.paste_keys.clone().unwrap_or_else(|| {
            args.mode
                .unwrap_or(zed::EditorMode::Normal)
                .default_paste_binding()
        }),
        if keys.ctrl_enter { "ctrl+enter" } else { "enter" },
    ));

    // Resolve the exe up front (fail fast): the worker stops only the
    // target process (`--zed-pid`) and starts this exe.
    let Some(exe) = zed::find_exe(&args.zed_path) else {
        log.error("Zed.exe not found");
        return 2;
    };

    sleep(Duration::from_secs(args.wait));

    let was_focused = zed::stop(&log, args.grace, args.zed_pid);

    // The new process is identified by its creation time: anything started
    // after this instant cannot be a survivor of the old session(s).
    let launch_after = win32::now_filetime();

    let ok = match zed::start(&log, &exe, &args.project) {
        Ok(lnk) => {
            let ok = work::inject(
                &log,
                &message,
                args.window_timeout,
                args.settle,
                args.heads_up,
                &args.window_title,
                &args.project,
                &exe,
                launch_after,
                was_focused,
                &keys,
            );
            // The launch is confirmed (or failed) by now; explorer has
            // parsed the shortcut long ago, so it can be removed.
            if let Some(p) = lnk {
                let _ = fs::remove_file(p);
            }
            ok
        }
        Err(e) => {
            log.error(&format!("{e}"));
            false
        }
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
    match zed::resolve_target(&args.zed_path, args.zed_pid) {
        Ok(Some(t)) => {
            println!("  zed running: {}", t.exe.display());
            match t.pid {
                Some(pid) => println!("  zed target : pid {pid}"),
                None => println!("  zed target : (stop nothing)"),
            }
        }
        Ok(None) => println!("  zed running: (none)"),
        Err(e) => println!("  zed running: AMBIGUOUS — {e}"),
    }

    let wins = zed::windows();
    println!("  zed windows: {}", wins.len());
    for w in &wins {
        println!(
            "    pid={} hung={} title='{}'",
            w.pid, w.hung, w.title,
        );
    }

    let cfg = zed::load_config(&args.config_dir);
    let plan = plan_inject(args);

    println!("  config dir : {}", cfg.dir.display());
    println!("  edit mode  : {}", plan.mode);
    println!(
        "  send key   : {} (effective)",
        if plan.ctrl_enter { "ctrl+enter" } else { "enter" },
    );
    println!(
        "  toggle key : {} ({:?}) (effective)",
        plan.toggle_binding,
        zed::parse_binding(&plan.toggle_binding),
    );
    println!(
        "  paste key  : {} ({:?}) (effective)",
        plan.paste_binding,
        zed::parse_binding(&plan.paste_binding),
    );

    let log = Log::new("check");
    println!("  log file   : {}", log.path().display());

    if exe_path.is_some() { 0 } else { 4 }
}

// ===================================================================
// tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_INSERT, VK_LCONTROL, VK_LSHIFT};

    #[test]
    fn plan_inject_from_fixture_config() {
        let dir = std::env::temp_dir().join("zed-reload-plan-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("settings.json"),
            "{ \"helix_mode\": true, \"agent\": { \"use_modifier_to_send\": true } }",
        )
        .unwrap();
        fs::write(
            dir.join("keymap.json"),
            "[ { \"context\": \"Workspace\", \"bindings\": { \"ctrl-shift-,\": \"agent::ToggleFocus\" } } ]",
        )
        .unwrap();
        let args = Args::parse_from(["zed-reload", "--config-dir", dir.to_str().unwrap()]);
        let plan = plan_inject(&args);
        assert_eq!(plan.mode, zed::EditorMode::Helix);
        assert!(plan.ctrl_enter);
        assert_eq!(plan.toggle_binding, "ctrl-shift-,");
        assert_eq!(plan.paste_binding, "shift-r");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_inject_defaults_and_overrides() {
        // No config at all: everything falls back to Zed defaults.
        let args = Args::parse_from([
            "zed-reload",
            "--config-dir",
            std::env::temp_dir().join("zed-reload-no-such-dir").to_str().unwrap(),
        ]);
        let plan = plan_inject(&args);
        assert_eq!(plan.mode, zed::EditorMode::Normal);
        assert!(!plan.ctrl_enter);
        assert_eq!(plan.toggle_binding, "ctrl-shift-/");
        assert_eq!(plan.paste_binding, "ctrl-v");

        // Overrides win over the (absent) config.
        let args = Args::parse_from([
            "zed-reload",
            "--mode",
            "vim",
            "--toggle-keys",
            "ctrl-alt-/",
            "--paste-keys",
            "shift-insert",
            "--send-enter",
        ]);
        let plan = plan_inject(&args);
        assert_eq!(plan.mode, zed::EditorMode::Vim);
        assert!(!plan.ctrl_enter);
        assert_eq!(plan.toggle_binding, "ctrl-alt-/");
        assert_eq!(plan.paste_binding, "shift-insert");
    }

    #[test]
    fn worker_keys_paste_in_normal_mode() {
        // Vim: quick-paste is shift-insert, pressed in Normal mode.
        let args = Args::parse_from(["zed-reload", "--mode", "vim", "--send-ctrl-enter"]);
        let keys = worker_keys(&args);
        assert!(keys.ctrl_enter);
        assert_eq!(keys.paste, vec![VK_LSHIFT, VK_INSERT]);

        // Helix: quick-paste is shift-r.
        let args = Args::parse_from(["zed-reload", "--mode", "helix"]);
        assert_eq!(worker_keys(&args).paste, vec![VK_LSHIFT, b'R' as u16]);

        // Normal mode: ctrl-v.
        let args = Args::parse_from(["zed-reload"]);
        assert_eq!(worker_keys(&args).paste, vec![VK_LCONTROL, b'V' as u16]);
    }
}
