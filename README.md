# zed-reload

Reload Zed and inject a message into the Agent Panel — so an AI agent
thread survives the restart and continues **without human help**. Built for
MCP-server development loops (Zed spawns MCP servers at startup; a reload is
required to pick up a rebuilt server or changed config, and the agent should
resume the benchmark/update task automatically), Zed/extension/settings
updates, and crash/hang recovery.

Windows only. Primary implementation: a ~200KB Rust binary, zero
dependencies beyond `windows-sys`. Legacy bash/PowerShell/cmd
implementations live in `scripts/` (same behavior, handy where Rust isn't
available).

## Usage

```bash
zed-reload "continue"                  # reload Zed, then send message (default msg: continue)
zed-reload --send "ping"               # inject into the running Zed, no reload
zed-reload --watch "continue"          # wait for Zed to die, then revive + send
zed-reload --watch --unresponsive 60   # also revive if the window hangs for 60s
zed-reload --check                     # diagnostics (no side effects)
zed-reload --wait 25 "msg"             # delay before acting (lets the caller finish first)
zed-reload --help                      # full option list
```

Every run logs to `zed-reload.log` next to the exe — check it first when
something didn't happen.

## Agent self-resurrection pattern

The agent arms its own host's restart; the launcher returns immediately,
then a detached worker outlives Zed, reloads it, and injects the message
into the same thread — the next agent instance reads it as a user message:

```bash
zed-reload --wait 25 "[zed-reload] Zed was reloaded. Read <file> for context and continue <task> from step <N>."
```

Rules: finish all file writes before arming; make the revival message
self-contained; use `--wait >= 20` so the final chat message flushes.

## MCP development loop

```bash
# rebuild your MCP server, then:
zed-reload --wait 25 "MCP server rebuilt. Zed reloaded it — re-run the benchmark: <cmd>, compare with <baseline>, report."
```

## How it works

1. Launcher writes the message to a temp file and re-spawns itself detached
   (`DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB`), so
   the worker survives the terminal and Zed's own death.
2. `WM_CLOSE` every Zed window; force-kill after `--grace` (20s) if a modal
   prompt blocks the quit.
3. Bare relaunch — Zed's `restore_on_startup` defaults to `last_session`,
   so the workspace incl. the Agent Panel reopens (`--project PATH` to open
   a specific folder instead).
4. Wait for the main window, settle, force it to the foreground
   (`SwitchToThisWindow` + `AttachThreadInput` — the same foreground-lock
   fight Zed's own `activate()` does with an ALT `SendInput`).
5. `ctrl+shift+/` (`agent::ToggleFocus`) focuses the message editor —
   deterministic because the panel is never focused right after a start.
6. Message is pasted via the clipboard (restored afterwards, text only) and
   sent. Send key is auto-detected from Zed's `settings.json`: `ctrl+enter`
   when `"use_modifier_to_send": true`, plain `enter` otherwise
   (`--send-enter` / `--send-ctrl-enter` to override).

Zed.exe resolution: `%LOCALAPPDATA%\Programs\Zed Nightly\Zed.exe`, then
`%LOCALAPPDATA%\Programs\Zed\Zed.exe`, then `%PATH%` (`--zed-path` to
override). Requires the default `ctrl+shift+/` → `agent::ToggleFocus`
keybinding.

## Caveats

- Keystrokes go to the foreground window: needs an **unlocked desktop**;
  don't type while it fires. If the Zed window can't be brought to the
  foreground, the run aborts instead of typing into the wrong app.
- `--send` presses the panel toggle blindly: if the panel already has focus
  it gets hidden and the paste lands in an open editor. `--restart` (default)
  and `--watch` are deterministic — use those unattended.
- Unsaved editors can block the graceful quit → force-kill after `--grace`.
- Clipboard is clobbered during paste, then restored (text only).

## Build & install

```bash
cargo build --release
# put target/release/zed-reload.exe anywhere on %PATH%
```

To migrate: copy `zed-reload.exe` to any directory on `%PATH%`. Nothing in
the binary is machine- or user-specific.

## Why not Zed's own mechanisms?

Zed has no supported "reload without focus change" or "run action on
startup" facility: gpui/workspace contain internal `focus`/`visible`
plumbing, but neither the CLI nor the IPC payload exposes it, and the CLI
can't dispatch actions to a running instance. Keystroke injection against a
freshly reloaded window is the deterministic workaround.
