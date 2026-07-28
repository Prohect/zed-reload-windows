<#
.SYNOPSIS
  zed-reload - restart Zed and inject a message into the Agent Panel.

.DESCRIPTION
  Worker script; normally launched detached via the `zed-reload` bash
  wrapper (MSYS2) or zed-reload.cmd so it survives Zed's own death.

  Modes:
    restart - close Zed, relaunch it (session restore reopens the workspace),
              focus the Agent Panel, paste the message, send it. (default)
    send    - inject into the currently running Zed, no restart.
    watch   - poll until Zed's window vanishes (or hangs when
              -UnresponsiveSeconds > 0), then revive + inject; exits after
              one revive or when -WatchTimeout elapses.
    check   - print resolved configuration and exit (diagnostics).

  Zed defaults this relies on (verified against zed-industries/zed source):
    ctrl+shift+/  = agent::ToggleFocus. Panel unfocused -> focuses it,
      hidden -> shows+focuses, focused -> hides it. Right after a fresh
      start the panel is never focused, so one press deterministically
      lands focus in the message editor. Override with -PanelFocusKeys.
    enter = send message, unless settings.json sets
      "use_modifier_to_send": true (then ctrl+enter sends and enter is a
      newline). Auto-detected from %APPDATA%\Zed\settings.json; override
      with -SendEnter / -SendCtrlEnter.
    restore_on_startup defaults to last_session, so a bare relaunch
      reopens the previous workspace incl. the Agent Panel.

  Caveats:
    - Keystrokes go through WScript.Shell SendKeys: needs an unlocked
      interactive desktop; do not type elsewhere while it runs.
    - The clipboard is used for pasting and restored afterwards (text only).
    - If Zed shows a modal prompt on quit (e.g. unsaved files), graceful
      close times out after -GraceSeconds and Zed is force-killed.
    - Mode=send presses the panel toggle blindly: if the Agent Panel
      already has focus at that instant it would be hidden and the paste
      would land in an editor. restart/watch are deterministic - prefer
      them for unattended operation.
#>
[CmdletBinding()]
param(
  [string]$Message = "continue",
  [string]$MessageFile,
  [ValidateSet("restart","send","watch","check")] [string]$Mode = "restart",
  [int]$PreDelay = 6,
  [int]$Settle = 10,
  [int]$WindowTimeout = 90,
  [int]$GraceSeconds = 20,
  [int]$WatchTimeout = 3600,
  [int]$UnresponsiveSeconds = 0,
  [string]$Project,
  [string]$ZedPath,
  [string]$WindowTitleMatch,
  [string]$PanelFocusKeys = "^+/",
  [switch]$SendEnter,
  [switch]$SendCtrlEnter,
  [string]$LogFile = (Join-Path $PSScriptRoot "zed-reload.log")
)

function Write-Log([string]$line) {
  $entry = "[{0}] [{1}] [pid {2}] {3}" -f (Get-Date -Format "yyyy-MM-dd HH:mm:ss"), $Mode, $PID, $line
  try { Add-Content -Path $LogFile -Value $entry -Encoding UTF8 } catch {}
  Write-Host $entry  # visible progress for foreground .cmd runs; goes nowhere when hidden
}

function Get-ZedExe {
  if ($ZedPath -and (Test-Path $ZedPath)) { return $ZedPath }
  $candidates = @(
    (Join-Path $env:LOCALAPPDATA "Programs\Zed Nightly\Zed.exe"),
    (Join-Path $env:LOCALAPPDATA "Programs\Zed\Zed.exe")
  )
  foreach ($c in $candidates) { if (Test-Path $c) { return $c } }
  $cmd = Get-Command Zed.exe -ErrorAction SilentlyContinue
  if ($cmd) { return $cmd.Source }
  return $null
}

function Get-ZedWindows {
  @(Get-Process -Name Zed -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 })
}

function Get-SendSequence {
  if ($SendEnter) { return "{ENTER}" }
  if ($SendCtrlEnter) { return "^{ENTER}" }
  $settings = Join-Path $env:APPDATA "Zed\settings.json"
  if (Test-Path $settings) {
    $raw = Get-Content -Raw -Encoding UTF8 $settings
    $noComments = ($raw -split "`r?`n" | Where-Object { $_ -notmatch '^\s*//' }) -join "`n"
    if ($noComments -match '"use_modifier_to_send"\s*:\s*true') { return "^{ENTER}" }
  }
  return "{ENTER}"
}

function Stop-Zed {
  $wins = Get-ZedWindows
  if ($wins.Count -eq 0) { Write-Log "no Zed window to close" }
  foreach ($p in $wins) {
    Write-Log "CloseMainWindow pid=$($p.Id) title='$($p.MainWindowTitle)'"
    try { [void]$p.CloseMainWindow() } catch { Write-Log "CloseMainWindow failed: $_" }
  }
  $deadline = (Get-Date).AddSeconds($GraceSeconds)
  while ((Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 500
    if (-not (Get-Process -Name Zed -ErrorAction SilentlyContinue)) {
      Write-Log "Zed exited gracefully"
      return
    }
  }
  Write-Log "graceful close timed out after ${GraceSeconds}s (modal prompt?) - force killing"
  Get-Process -Name Zed -ErrorAction SilentlyContinue | Stop-Process -Force
  Start-Sleep -Seconds 2
}

function Start-Zed {
  $script:ZedExe = Get-ZedExe
  if (-not $script:ZedExe) { Write-Log "ERROR: Zed.exe not found"; exit 4 }
  if ($Project) {
    Write-Log "starting '$script:ZedExe' with project '$Project'"
    Start-Process -FilePath $script:ZedExe -ArgumentList ('"{0}"' -f $Project)
  } else {
    Write-Log "starting '$script:ZedExe' (bare, session restore)"
    Start-Process -FilePath $script:ZedExe
  }
}

function Wait-ZedWindow {
  $deadline = (Get-Date).AddSeconds($WindowTimeout)
  while ((Get-Date) -lt $deadline) {
    $w = Get-ZedWindows
    if ($WindowTitleMatch) { $w = @($w | Where-Object { $_.MainWindowTitle -match $WindowTitleMatch }) }
    if ($w.Count -gt 0) { return $w[0] }
    Start-Sleep -Milliseconds 500
  }
  return $null
}

function Send-AgentMessage([string]$text) {
  $w = Wait-ZedWindow
  if (-not $w) { Write-Log "ERROR: no Zed window within ${WindowTimeout}s"; return $false }
  Write-Log "window found pid=$($w.Id) title='$($w.MainWindowTitle)'; settling ${Settle}s"
  Start-Sleep -Seconds $Settle

  try {
    Add-Type -Namespace ZCUtil -Name Win -MemberDefinition @"
[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern bool SetForegroundWindow(System.IntPtr hWnd);
[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern System.IntPtr GetForegroundWindow();
[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern bool ShowWindow(System.IntPtr hWnd, int nCmdShow);
[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern bool IsIconic(System.IntPtr hWnd);
[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern bool BringWindowToTop(System.IntPtr hWnd);
[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern void SwitchToThisWindow(System.IntPtr hWnd, bool fAltTab);
[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(System.IntPtr hWnd, out uint lpdwProcessId);
[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool fAttach);
[System.Runtime.InteropServices.DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
"@
  } catch {}

  $wshell = New-Object -ComObject WScript.Shell
  $hwnd = $w.MainWindowHandle
  # A background process may not steal the foreground (foreground lock).
  # Layered workarounds: SwitchToThisWindow (taskbar path), AttachThreadInput
  # to the foreground thread (AutoHotkey-style), AppActivate after an injected
  # phantom keystroke. SW_RESTORE only when minimized (it un-maximizes!).
  $fg = $false
  for ($i = 0; $i -lt 15 -and -not $fg; $i++) {
    try {
      if ([ZCUtil.Win]::GetForegroundWindow() -eq $hwnd) { $fg = $true; break }
      if ([ZCUtil.Win]::IsIconic($hwnd)) { [void][ZCUtil.Win]::ShowWindow($hwnd, 9) }
      [ZCUtil.Win]::SwitchToThisWindow($hwnd, $true)
      Start-Sleep -Milliseconds 200
      if ([ZCUtil.Win]::GetForegroundWindow() -eq $hwnd) { $fg = $true; break }
      $fgHwnd = [ZCUtil.Win]::GetForegroundWindow()
      $fgPid = 0
      $fgThread = [ZCUtil.Win]::GetWindowThreadProcessId($fgHwnd, [ref]$fgPid)
      $myThread = [ZCUtil.Win]::GetCurrentThreadId()
      if ($fgThread -ne 0 -and $fgThread -ne $myThread) {
        [void][ZCUtil.Win]::AttachThreadInput($myThread, $fgThread, $true)
        [void][ZCUtil.Win]::SetForegroundWindow($hwnd)
        [void][ZCUtil.Win]::BringWindowToTop($hwnd)
        [void][ZCUtil.Win]::AttachThreadInput($myThread, $fgThread, $false)
      } else {
        [void][ZCUtil.Win]::SetForegroundWindow($hwnd)
      }
      Start-Sleep -Milliseconds 200
      if ([ZCUtil.Win]::GetForegroundWindow() -eq $hwnd) { $fg = $true; break }
      $wshell.SendKeys('{F15}')   # phantom key: counts as input, apps ignore it
      [void]$wshell.AppActivate($w.Id)
      Start-Sleep -Milliseconds 250
      $fg = ([ZCUtil.Win]::GetForegroundWindow() -eq $hwnd)
    } catch { Write-Log "foreground attempt $($i+1) error: $_" }
  }
  Write-Log "foreground=$fg after $i attempt(s)"
  if (-not $fg) { Write-Log "ERROR: could not focus Zed window"; return $false }
  Start-Sleep -Milliseconds 700

  $sendSeq = Get-SendSequence
  $oldClip = $null
  try { $oldClip = Get-Clipboard -Raw } catch {}
  try {
    Set-Clipboard -Value $text
    $wshell.SendKeys($PanelFocusKeys)   # agent::ToggleFocus -> message editor
    Start-Sleep -Milliseconds 1600
    $wshell.SendKeys("^v")              # paste
    Start-Sleep -Milliseconds 900
    $wshell.SendKeys($sendSeq)          # send (enter or ctrl+enter)
    Write-Log "injected $($text.Length) chars, sendKey='$sendSeq'"
  } finally {
    if ($null -ne $oldClip) {
      Start-Sleep -Milliseconds 400
      try { Set-Clipboard -Value $oldClip } catch {}
    }
  }
  return $true
}

# ---- main ----
$msg = $Message
if ($MessageFile) {
  try {
    $msg = [System.IO.File]::ReadAllText($MessageFile, [System.Text.Encoding]::UTF8)
    Remove-Item -Force -ErrorAction SilentlyContinue $MessageFile
  } catch {
    Write-Log "ERROR reading MessageFile '$MessageFile': $_"
    exit 2
  }
}

if ($Mode -eq "check") {
  $exe = Get-ZedExe
  Write-Output "zed-reload check"
  Write-Output "  script     : $PSCommandPath"
  Write-Output "  zed exe    : $(if ($exe) { $exe } else { 'NOT FOUND' })"
  $wins = Get-ZedWindows
  Write-Output "  zed windows: $($wins.Count)"
  foreach ($p in $wins) { Write-Output "    pid=$($p.Id) responding=$($p.Responding) title='$($p.MainWindowTitle)'" }
  Write-Output "  send key   : $(Get-SendSequence) (auto-detected)"
  Write-Output "  panel keys : $PanelFocusKeys"
  Write-Output "  log file   : $LogFile"
  try { New-Object -ComObject WScript.Shell | Out-Null; Write-Output "  wscript    : ok" } catch { Write-Output "  wscript    : FAILED" }
  exit $(if ($exe) { 0 } else { 4 })
}

Write-Log "=== start: msgLen=$($msg.Length) preDelay=${PreDelay}s settle=${Settle}s grace=${GraceSeconds}s ==="

$ok = $false
switch ($Mode) {
  "send" {
    if ($PreDelay -gt 0) { Start-Sleep -Seconds $PreDelay }
    $ok = Send-AgentMessage $msg
  }
  "restart" {
    if ($PreDelay -gt 0) { Start-Sleep -Seconds $PreDelay }
    Stop-Zed
    Start-Zed
    $ok = Send-AgentMessage $msg
  }
  "watch" {
    $deadline = (Get-Date).AddSeconds($WatchTimeout)
    $misses = 0
    $badSince = $null
    Write-Log "watching: timeout=${WatchTimeout}s unresponsive=$(if ($UnresponsiveSeconds -gt 0) { "${UnresponsiveSeconds}s" } else { 'off' })"
    while ((Get-Date) -lt $deadline -and -not $ok) {
      $wins = Get-ZedWindows
      if ($wins.Count -eq 0) {
        $misses++
        Write-Log "no Zed window (check $misses/2)"
        if ($misses -ge 2) {
          Write-Log "Zed gone - reviving"
          Stop-Zed   # clean up windowless leftovers
          Start-Zed
          $ok = Send-AgentMessage $msg
        }
      } else {
        $misses = 0
        if ($UnresponsiveSeconds -gt 0) {
          $hung = @($wins | Where-Object { -not $_.Responding })
          if ($hung.Count -gt 0) {
            if (-not $badSince) { $badSince = Get-Date; Write-Log "unresponsive: pid=$($hung[0].Id) title='$($hung[0].MainWindowTitle)'" }
            elseif (((Get-Date) - $badSince).TotalSeconds -ge $UnresponsiveSeconds) {
              Write-Log "still unresponsive after ${UnresponsiveSeconds}s - restarting"
              Stop-Zed
              Start-Zed
              $ok = Send-AgentMessage $msg
            }
          } else { $badSince = $null }
        }
      }
      if (-not $ok) { Start-Sleep -Seconds 5 }
    }
    if (-not $ok) { Write-Log "watch timeout, Zed stayed healthy" ; $ok = $true }
  }
}
Write-Log "=== done: ok=$ok ==="
exit $(if ($ok) { 0 } else { 3 })
