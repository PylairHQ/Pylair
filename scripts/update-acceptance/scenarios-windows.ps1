# Update acceptance (decision 0011, PR 4) — Windows scenario runner.
#
# Scenarios a–e (currentUser NSIS) plus f (per-machine): the per-machine
# rollback must record rollback-pending-approval and NOT silently
# restore — elevation mid-crash-loop is impossible (spec §2).
#
#   a. v1 -> v2        confirm chain end to end
#   b. v1 -> v2-slow   45s of progress outlives the 10s heartbeat
#   c. v2 -> v3        readiness_failed rollback + restore + skip list
#   d. v2 -> v3-crash  crash_loop after maxBoots
#   e. tampered cache  verify_failed, no restore, app reports it
#   f. per-machine     approval recorded, v3pm stays installed
#
# Args: -Work <dir> -Htdocs <dir> [-Ident com.pylair.acceptupd]
#       [-Port 8791] [-Scenarios a,b,c,d,e,f] [-AppBin app-under-test]

param(
  [Parameter(Mandatory=$true)][string]$Work,
  [Parameter(Mandatory=$true)][string]$Htdocs,
  [string]$Ident = "com.pylair.acceptupd",
  [int]$Port = 8791,
  [string[]]$Scenarios = @("a","b","c","d","e","f"),
  [string]$AppBin = "app-under-test"
)

$ErrorActionPreference = "Continue"  # kills/late polls must not abort the leg
$script:Failed = 0
$DataDir  = Join-Path $env:APPDATA $Ident
$StateDir = Join-Path $DataDir "updater-state"
$Log      = Join-Path $DataDir "accept\log.jsonl"
$Evidence = Join-Path $Work "evidence"
$Timings  = Join-Path $Work "timings.log"
$AppDir   = Join-Path $Work "app"
$script:LaunchedPids = @()

New-Item -ItemType Directory -Force -Path $AppDir, $Evidence, (Join-Path $DataDir "accept"), (Join-Path $Htdocs "latest") | Out-Null

function Say($m) { Write-Host "== $(Get-Date -Format HH:mm:ss) $m" }
function Fail($m) { Add-Content (Join-Path $Work "FAILURES") "FAIL: $m"; Write-Host "FAIL: $m"; $script:Failed = 1 }
function NoteT($m) { Add-Content $Timings "$([DateTimeOffset]::Now.ToUnixTimeSeconds()) $m" }

# ---- log assertions --------------------------------------------------------

function LogLines($tag) {
  if (-not (Test-Path $Log)) { return @() }
  @(Select-String -Path $Log -Pattern ([regex]::Escape("`"tag`": `"$tag`"")) -SimpleMatch:$false |
      ForEach-Object { $_.Line })
}
function LogHas($tag, $substr) { @(LogLines $tag | Where-Object { $_.Contains($substr) }).Count -gt 0 }
function LogCount($tag, $substr) { @(LogLines $tag | Where-Object { $_.Contains($substr) }).Count }

function Wait-For([int]$Seconds, [string]$Desc, [scriptblock]$Probe) {
  $deadline = (Get-Date).AddSeconds($Seconds)
  while (-not (& $Probe)) {
    if ((Get-Date) -gt $deadline) { Fail "timeout (${Seconds}s): $Desc"; return $false }
    Start-Sleep -Milliseconds 500
  }
  return $true
}
function Wait-File([int]$S, [string]$Desc, [string]$Path) { Wait-For $S $Desc { Test-Path $Path } }
function Wait-Log([int]$S, [string]$Desc, [string]$Tag, [string]$Sub) { Wait-For $S $Desc { LogHas $Tag $Sub } }
function Assert-True([string]$Desc, [scriptblock]$Probe) { if (-not (& $Probe)) { Fail "assert: $Desc" } }

function StateSeq {
  # Transitions only: each boot's watcher re-logs the current state as
  # its first sample, so collapse consecutive repeats (v1 logs "pending"
  # before the install kills it; v2's first sample is also "pending").
  $seq = @((Select-String -Path $Log -Pattern '"tag": "update.state"' -ErrorAction SilentlyContinue) |
      ForEach-Object { if ($_.Line -match '"data": "([a-z_]+)"') { $Matches[1] } })
  $out = @()
  foreach ($s in $seq) { if ($out.Count -eq 0 -or $out[-1] -ne $s) { $out += $s } }
  $out -join " "
}

# ---- app control ------------------------------------------------------------

function Set-Offer($tag) {
  Copy-Item (Join-Path $Htdocs "releases\$tag\latest.json") (Join-Path $Htdocs "latest\latest.json") -Force
  Say "offer <- $tag"
}

function Installed-Exe($perMachine) {
  # NSIS per-user lands under %LOCALAPPDATA%, per-machine under
  # %ProgramFiles% — search like the acceptance job does rather than
  # assuming the install dir's exact shape.
  $roots = if ($perMachine) { @($env:ProgramFiles) } else { @($env:LOCALAPPDATA) }
  $c = Get-ChildItem $roots -Recurse -Filter "$AppBin.exe" -Depth 3 -ErrorAction SilentlyContinue |
       Select-Object -First 1
  if ($c) { $c.FullName } else { $null }
}

function Install-Tag($tag, [switch]$PerMachine) {
  $s = Get-Date
  $setup = Get-ChildItem (Join-Path $Htdocs "installable\$tag") -Filter "*-setup.exe" | Select-Object -First 1
  if (-not $setup) { Fail "no -setup.exe under installable\$tag"; return }
  $inst = Start-Process -FilePath $setup.FullName -ArgumentList "/S" -Wait -PassThru
  if ($inst.ExitCode -ne 0) { Fail "nsis install $tag exit $($inst.ExitCode)" }
  $ok = Wait-For 60 "installed exe for $tag" { Installed-Exe $PerMachine }
  if ($ok) { Say "installed $tag -> $(Installed-Exe $PerMachine)" }
  NoteT ("install {0} {1}s" -f $tag, [int]((Get-Date) - $s).TotalSeconds)
}

function Launch([switch]$PerMachine) {
  $exe = Installed-Exe $PerMachine
  if (-not $exe) { Fail "no installed exe to launch"; return }
  $p = Start-Process -FilePath $exe -PassThru
  $script:LaunchedPids += $p.Id
  Say "launched pid=$($p.Id) ($exe)"
}

function Stop-Apps {
  $pids = @($script:LaunchedPids)
  if (Test-Path $Log) {
    $pids += @(Select-String -Path $Log -Pattern '"ppid": (\d+)' |
      ForEach-Object { [int]$_.Matches[0].Groups[1].Value } | Sort-Object -Unique)
  }
  foreach ($procId in ($pids | Sort-Object -Unique)) {
    Stop-Process -Id $procId -Force -ErrorAction SilentlyContinue
  }
  # Path-scoped sweep — never a name match: any process whose executable
  # IS the installed app binary (either install root) is a survivor the
  # ppid records missed (watchdog-relaunched, installer-auto-launched).
  # A zombie once blocked an NSIS /S upgrade and poisoned a whole run.
  $exes = @((Installed-Exe $false), (Installed-Exe $true)) | Where-Object { $_ }
  if ($exes.Count) {
    Get-CimInstance Win32_Process |
      Where-Object { $ep = $_.ExecutablePath; $ep -and ($exes | Where-Object { $ep -eq $_ }) } |
      ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
  }
  # Stale watchdogs exit on their own (session mismatch), but a leftover
  # still polling an orphaned pending.json wastes a whole wait window.
  Get-CimInstance Win32_Process |
    Where-Object { $_.CommandLine -and $_.CommandLine.Contains("pylair-upwatch") } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
  Start-Sleep 2
  $script:LaunchedPids = @()
}

function Snapshot($name) {
  $d = Join-Path $Evidence $name
  New-Item -ItemType Directory -Force -Path $d | Out-Null
  Copy-Item (Join-Path $DataDir "*") $d -Recurse -Force -ErrorAction SilentlyContinue
}

function Reset-State {
  Stop-Apps
  # Clean-machine equivalent of CI: an installed app survives scenario
  # boundaries otherwise, and NSIS /S does NOT reliably downgrade-replace
  # (a leftover v3 kept the exe while v1's setup silently no-opped).
  foreach ($exe in @((Installed-Exe $false), (Installed-Exe $true)) | Where-Object { $_ }) {
    $uninst = Join-Path (Split-Path $exe) "uninstall.exe"
    if (Test-Path $uninst) {
      Say "uninstall $exe"
      Start-Process -FilePath $uninst -ArgumentList "/S" -Wait -ErrorAction SilentlyContinue | Out-Null
    } else {
      Remove-Item (Split-Path $exe) -Recurse -Force -ErrorAction SilentlyContinue
    }
  }
  Start-Sleep 2
  Remove-Item $DataDir -Recurse -Force -ErrorAction SilentlyContinue
  New-Item -ItemType Directory -Force -Path (Join-Path $DataDir "accept") | Out-Null
}

function Corrupt-File($f) {
  Set-Content "$f.tmp" "CORRUPTED-ACCEPTANCE"
  Move-Item "$f.tmp" $f -Force
}

function Boots-Count {
  $b = Join-Path $StateDir "boots"
  if (Test-Path $b) { @(Get-ChildItem $b).Count } else { 0 }
}

function Boots-Version-Count([string]$Ver) {
  # Product evidence: the host writes one boots/<nanos>-<pid>.json per
  # launch of the pending version (update/mod.rs) with "version" inside
  # — immune to the fixture's own log-line loss (the scenario-e flake:
  # v3 booted and proved it through update.state, yet its fixture "boot"
  # line never landed).
  $b = Join-Path $StateDir "boots"
  if (-not (Test-Path $b)) { return 0 }
  $pat = '"version":\s*"' + [regex]::Escape($Ver) + '"'
  @(Get-ChildItem $b -File -Filter *.json | Where-Object {
    (Get-Content $_.FullName -Raw -ErrorAction SilentlyContinue) -match $pat
  }).Count
}

# ---- scenarios --------------------------------------------------------------

function Scenario-A {
  Say "-- scenario a: v1 -> v2 confirms"
  $s = Get-Date
  Reset-State; Set-Offer v2.0.0; Install-Tag v1.0.0; Launch
  Wait-Log 420 "v1 records the v2 offer" "install" "2.0.0" | Out-Null
  Wait-File 420 "watchdog wrote update-ok" (Join-Path $StateDir "update-ok") | Out-Null
  Wait-Log 60 "on_update_confirmed fired for 2.0.0" "confirmed" "2.0.0" | Out-Null
  Assert-True "on_update_confirmed fired exactly once" { (LogCount "confirmed" "2.0.0") -eq 1 }
  Assert-True "pending cleared on confirm" { -not (Test-Path (Join-Path $StateDir "pending.json")) }
  Assert-True "update.state sequence none->pending->confirmed" { (StateSeq) -eq "none pending confirmed" }
  Wait-Log 60 "health().update surfaces ready" "health" '\"state\":\"ready\"' | Out-Null
  NoteT ("scenario a {0}s" -f [int]((Get-Date) - $s).TotalSeconds)
  Snapshot "a-confirm"
}

function Scenario-B {
  Say "-- scenario b: v1 -> v2-slow confirms despite heartbeat"
  $s = Get-Date
  Reset-State; Set-Offer v2.1.0; Install-Tag v1.0.0; Launch
  Wait-Log 420 "v1 records the v2-slow offer" "install" "2.1.0" | Out-Null
  $progress = Join-Path $StateDir "progress.json"
  Wait-File 120 "progress.json appears" $progress | Out-Null
  $m1 = (Get-Item $progress -ErrorAction SilentlyContinue).LastWriteTimeUtc
  Start-Sleep 8
  $m2 = (Get-Item $progress -ErrorAction SilentlyContinue).LastWriteTimeUtc
  Assert-True "progress.json advanced during slow ready ($m1 -> $m2)" { $m1 -ne $m2 }
  Wait-File 300 "v2-slow confirmed (update-ok)" (Join-Path $StateDir "update-ok") | Out-Null
  Assert-True "no rollback happened" { -not (Test-Path (Join-Path $StateDir "rollback-reason.txt")) }
  Wait-Log 60 "on_update_confirmed fired for 2.1.0" "confirmed" "2.1.0" | Out-Null
  NoteT ("scenario b {0}s" -f [int]((Get-Date) - $s).TotalSeconds)
  Snapshot "b-slow-confirm"
}

function Scenario-C {
  Say "-- scenario c: v2 -> v3 rolls back readiness_failed"
  $s = Get-Date
  Reset-State; Set-Offer v3.0.0; Install-Tag v2.0.0; Launch
  Wait-Log 420 "v2 records the v3 offer" "install" "3.0.0" | Out-Null
  $reason = Join-Path $StateDir "rollback-reason.txt"
  Wait-File 300 "rollback reason written" $reason | Out-Null
  Assert-True "reason is readiness_failed" { (Get-Content $reason -Raw) -match "readiness_failed" }
  Assert-True "v2 restored (outcome)" { (Get-Content $reason -Raw) -match "restored 2.0.0" }
  Assert-True "v3 on the skip list" { (Get-Content (Join-Path $StateDir "skipped.json") -Raw) -match "3.0.0" }
  # The relaunched v2 must be given real boot+record time: a bare
  # Wait-Log on the tag matches the INITIAL boot's line and returns
  # instantly — the count probe is the actual evidence.
  Wait-For 300 "restored v2 booted (second 2.0.0 boot record)" { (LogCount "boot" "2.0.0") -ge 2 } | Out-Null
  Wait-Log 300 "restored v2's fresh check is skipped -> null" "check" '"data": "null"' | Out-Null
  Assert-True "health().update surfaces rolled-back" { LogHas "health" "rolled-back" }
  NoteT ("scenario c {0}s" -f [int]((Get-Date) - $s).TotalSeconds)
  Snapshot "c-readiness-rollback"
}

function Scenario-D {
  Say "-- scenario d: v2 -> v3-crash rolls back crash_loop"
  $s = Get-Date
  Reset-State; Set-Offer v4.0.0; Install-Tag v2.0.0; Launch
  Wait-Log 420 "v2 records the v4 offer" "install" "4.0.0" | Out-Null
  Wait-File 120 "pending armed" (Join-Path $StateDir "pending.json") | Out-Null
  Wait-For 180 "boot #1 marker" { (Boots-Count) -ge 1 } | Out-Null
  Launch; Wait-For 180 "boot #2 marker" { (Boots-Count) -ge 2 } | Out-Null
  Launch; Wait-For 180 "boot #3 marker" { (Boots-Count) -ge 3 } | Out-Null
  $reason = Join-Path $StateDir "rollback-reason.txt"
  Wait-File 300 "rollback reason written" $reason | Out-Null
  Assert-True "reason is crash_loop" { (Get-Content $reason -Raw) -match "crash_loop" }
  Assert-True "v2 restored (outcome)" { (Get-Content $reason -Raw) -match "restored 2.0.0" }
  Wait-For 300 "restored v2 booted (second 2.0.0 boot record)" { (LogCount "boot" "2.0.0") -ge 2 } | Out-Null
  NoteT ("scenario d {0}s" -f [int]((Get-Date) - $s).TotalSeconds)
  Snapshot "d-crash-loop"
}

function Scenario-E {
  Say "-- scenario e: tampered cache -> verify_failed, no restore"
  $s = Get-Date
  Reset-State; Set-Offer v3.0.0; Install-Tag v2.0.0; Launch
  Wait-Log 420 "v2 records the v3 offer" "install" "3.0.0" | Out-Null
  Wait-File 120 "pending armed + cache written" (Join-Path $StateDir "pending.json") | Out-Null
  $artifact = Get-ChildItem (Join-Path $StateDir "cache") -File |
              Where-Object { $_.Name -notlike "upwatch-*" } | Select-Object -First 1
  if (-not $artifact) { Fail "no cached artifact under $StateDir\cache" }
  else {
    Corrupt-File $artifact.FullName
    Say "corrupted $($artifact.FullName)"
  }
  $reason = Join-Path $StateDir "rollback-reason.txt"
  Wait-File 300 "rollback reason written" $reason | Out-Null
  Assert-True "restore refused (verify_failed)" { (Get-Content $reason -Raw) -match "verify_failed" }
  # v3's boot is product evidence, not the fixture's own log line: the
  # host writes a boots/ marker for every launch of the pending version.
  Wait-For 300 "v3 booted (still installed) — boots/ marker" { (Boots-Version-Count "3.0.0") -ge 1 } | Out-Null
  Wait-Log 300 "app reports verify_failed via health().update" "health" "verify_failed" | Out-Null
  NoteT ("scenario e {0}s" -f [int]((Get-Date) - $s).TotalSeconds)
  Snapshot "e-tamper-abort"
}

function Scenario-F {
  Say "-- scenario f: per-machine -> rollback-pending-approval"
  $s = Get-Date
  Reset-State; Set-Offer v9.0.0; Install-Tag v8.0.0 -PerMachine; Launch -PerMachine
  Wait-Log 420 "v2pm records the v3pm offer" "install" "9.0.0" | Out-Null
  $approval = Join-Path $StateDir "rollback-pending-approval"
  Wait-File 300 "approval recorded" $approval | Out-Null
  Assert-True "approval payload mentions the reason" { (Get-Content $approval -Raw) -match "readiness_failed|crash_loop|no_sign_of_life|hard_cap" }
  Assert-True "skip entry written even without restore" { (Get-Content (Join-Path $StateDir "skipped.json") -Raw) -match "9.0.0" }
  # The installed binary must still be v3pm — no silent restore ran.
  Stop-Apps
  Launch -PerMachine
  Wait-Log 300 "installed app is still 9.0.0 (no silent restore)" "boot" "9.0.0" | Out-Null
  # The health tick lands on the driver's 2s interval — poll for it
  # like every other health assert rather than one-shot grepping.
  Wait-Log 60 "health().update surfaces rollback-pending-approval" "health" "rollback-pending-approval" | Out-Null
  NoteT ("scenario f {0}s" -f [int]((Get-Date) - $s).TotalSeconds)
  Snapshot "f-per-machine-approval"
}

# ---- driver -------------------------------------------------------------------

Say "acceptance windows runner: Work=$Work Port=$Port"
$server = Start-Process -FilePath "python" -ArgumentList "-m","http.server","$Port","-d",$Htdocs `
          -PassThru -WindowStyle Hidden -RedirectStandardOutput (Join-Path $Work "http.out") `
          -RedirectStandardError (Join-Path $Work "http.err")
Wait-For 30 "http server on :$Port" {
  try { (Invoke-WebRequest -UseBasicParsing "http://127.0.0.1:$Port/" -TimeoutSec 2).StatusCode -eq 200 } catch { $false }
} | Out-Null

$Scenarios = $Scenarios | ForEach-Object { $_ -split "," } | ForEach-Object { $_.Trim() }
foreach ($sc in $Scenarios) {
  switch ($sc.ToLower()) {
    "a" { Scenario-A }
    "b" { Scenario-B }
    "c" { Scenario-C }
    "d" { Scenario-D }
    "e" { Scenario-E }
    "f" { Scenario-F }
    default { Fail "unknown scenario $sc" }
  }
}

Stop-Apps
Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue
Say "timings:"; if (Test-Path $Timings) { Get-Content $Timings }
if ($script:Failed -ne 0) { Write-Host "-- FAILURES --"; Get-Content (Join-Path $Work "FAILURES"); exit 1 }
Say "all scenarios green"
exit 0
