# glass-windows on-box bridge. Invoked over SSH by scripts/test-windows.sh. Does ALL box-side work:
# git sync (to a known base + optional working-tree overlay), cargo build, and the scheduled-task
# bounce into the interactive desktop session (session 1) where WGC capture + SendInput work --
# unlike the SSH session (session 0). Prints "<target>: PASS|FAIL", an aggregate, and exits non-zero
# on any failure. ASCII-only (non-ASCII corrupts over the transfer). ErrorActionPreference=Continue
# because native tools (cargo/git/schtasks) write progress to stderr, which under Stop aborts us.
param(
  [Parameter(Mandatory = $true)][string]$RepoDir,
  [string]$Branch = "",
  [string]$Sha = "",
  [string]$DiffPath = "",
  [string]$UntarPath = "",
  [string]$Targets = "",
  [switch]$All,
  [string]$Tests = "",
  [int]$TimeoutSec = 300,
  [switch]$Release
)
$ErrorActionPreference = "Continue"
$failures = 0
$artifacts = Join-Path $RepoDir ".windows-artifacts"

function Fail($msg) { Write-Host "ERROR: $msg"; exit 1 }

Set-Location $RepoDir

# --- 1. sync to the requested base commit, then overlay any working-tree delta ---
if ($Sha -ne "") {
  Write-Host "== sync: fetch + reset to $Sha ($Branch) =="
  cmd /c "git fetch -q origin $Branch 2>&1" | Out-Null
  if ($LASTEXITCODE -ne 0) { Fail "git fetch failed" }
  cmd /c "git checkout -q -f $Branch 2>&1" | Out-Null
  if ($LASTEXITCODE -ne 0) { Fail "git checkout failed" }
  cmd /c "git reset -q --hard $Sha 2>&1" | Out-Null
  if ($LASTEXITCODE -ne 0) { Fail "git reset failed" }
  cmd /c "git clean -q -fd 2>&1" | Out-Null
  if ($LASTEXITCODE -ne 0) { Fail "git clean failed" }
}
if ($DiffPath -ne "" -and (Test-Path $DiffPath)) {
  Write-Host "== sync: apply working-tree diff =="
  cmd /c "git apply --whitespace=nowarn `"$DiffPath`" 2>&1"
  if ($LASTEXITCODE -ne 0) { Fail "git apply failed" }
}
if ($UntarPath -ne "" -and (Test-Path $UntarPath)) {
  Write-Host "== sync: extract untracked files =="
  cmd /c "tar -xf `"$UntarPath`" -C `"$RepoDir`" 2>&1"
  if ($LASTEXITCODE -ne 0) { Fail "untar failed" }
}
# Remove the scratch delta files shipped by test-windows.sh (they live outside the repo) after use.
if ($DiffPath -ne "") { Remove-Item -Force -ErrorAction SilentlyContinue $DiffPath }
if ($UntarPath -ne "") { Remove-Item -Force -ErrorAction SilentlyContinue $UntarPath }

# --- 2. resolve target list ---
$exDir = Join-Path $RepoDir "crates\glass-windows\examples"
# -Targets arrives as one comma-joined string: PowerShell's -File does not split a command-line value
# into a [string[]] param, so split it here ourselves.
$targetList = @($Targets -split ',' | Where-Object { $_ -ne "" })
if ($All -or ($targetList.Count -eq 0 -and $Tests -eq "")) {
  # onbox*.rs (not onbox_*.rs): also include the plain `onbox` example, which produces the WebP artifacts.
  $targetList = @(Get-ChildItem (Join-Path $exDir "onbox*.rs") | ForEach-Object { $_.BaseName })
}
if ($targetList.Count -eq 0 -and $Tests -eq "") { Fail "no onbox examples found and no -Tests specified" }
$profile = if ($Release) { "release" } else { "debug" }
$relArg = if ($Release) { "--release" } else { "" }

# Fresh artifacts dir each run.
if (Test-Path $artifacts) { Remove-Item $artifacts -Recurse -Force }
New-Item -ItemType Directory -Path $artifacts | Out-Null

# --- 3. helper: run one built exe in the interactive session, return its log text ---
function Invoke-Interactive($exe, $tag, $exeArgs = "") {
  $log = Join-Path $env:TEMP "glassval-$tag.log"
  if (Test-Path $log) { Remove-Item $log -Force }
  $cmd = "cmd.exe /c `"`"$exe`" $exeArgs > `"$log`" 2>&1`""
  schtasks /delete /tn glassval /f 2>$null | Out-Null
  # /tr $cmd (with $cmd's embedded quotes) is validated on PowerShell 5.1 on the box -- PowerShell
  # passes $cmd as one argv element; do NOT "fix" this quoting (it produced correct interactive runs).
  schtasks /create /tn glassval /tr $cmd /sc ONCE /st 00:00 /rl HIGHEST /it /f | Out-Null
  schtasks /run /tn glassval | Out-Null
  $deadline = (Get-Date).AddSeconds($TimeoutSec)
  do {
    Start-Sleep -Milliseconds 700
    $q = schtasks /query /tn glassval /fo LIST /v 2>$null
    $sl = ($q | Select-String "^Status:" | Select-Object -First 1)
    $running = $sl -and ($sl.ToString() -match "Running")
  } while ($running -and (Get-Date) -lt $deadline)
  # Loop exits when the task finished OR the deadline passed; still Running => we timed out.
  if ($running) {
    schtasks /end /tn glassval 2>$null | Out-Null
    schtasks /delete /tn glassval /f 2>$null | Out-Null
    return @{ text = ""; result = "timeout"; ran = $false }
  }
  Start-Sleep -Milliseconds 500
  $q = schtasks /query /tn glassval /fo LIST /v 2>$null   # fresh query so Last Result is final
  $lr = ($q | Select-String "Last Result:" | Select-Object -First 1)
  $lastResult = if ($lr) { ($lr.ToString() -replace '.*:\s*', '').Trim() } else { "unknown" }
  schtasks /delete /tn glassval /f 2>$null | Out-Null
  if (-not (Test-Path $log)) { return @{ text = ""; result = $lastResult; ran = $false } }
  return @{ text = (Get-Content $log -Raw); result = $lastResult; ran = $true }
}

# --- 4. verdict from a target's captured output ---
function Test-Verdict($r) {
  if (-not $r.ran) {
    $why = if ($r.result -eq "timeout") { "timed out" } else { "no log (interactive bounce failed?)" }
    return @{ ok = $false; why = $why }
  }
  if ($r.result -ne "0" -and $r.result -ne "unknown") { return @{ ok = $false; why = "exit $($r.result)" } }
  # Case-SENSITIVE (-cmatch): only the uppercase PASS/FAIL status tokens count, so prose like
  # "fast-fail preserved" does not trip the verdict (-match is case-insensitive by default).
  if ($r.text -cmatch "(?m)\bFAIL\b") { return @{ ok = $false; why = "FAIL token" } }
  if ($r.text -cnotmatch "(?m)\bPASS\b") { return @{ ok = $false; why = "no PASS token" } }
  return @{ ok = $true; why = "ok" }
}

# --- 5. run examples ---
foreach ($t in $targetList) {
  Write-Host "`n===== example: $t ====="
  cmd /c "cargo build -p glass-windows --example $t $relArg 2>&1" | Select-Object -Last 20
  if ($LASTEXITCODE -ne 0) { Write-Host "${t}: FAIL (build)"; $failures++; continue }
  $exe = Join-Path $RepoDir "target\$profile\examples\$t.exe"
  $r = Invoke-Interactive $exe $t
  Write-Host $r.text
  $v = Test-Verdict $r
  if ($v.ok) { Write-Host "${t}: PASS" } else { Write-Host "${t}: FAIL ($($v.why))"; $failures++ }
  Get-ChildItem (Join-Path $env:USERPROFILE "*.webp") -ErrorAction SilentlyContinue |
    Copy-Item -Destination $artifacts -Force -ErrorAction SilentlyContinue
}

# --- 6. run ignored tests (optional) ---
# cargo test would run them in OUR session (0); on-box tests need session 1. So build the test
# binaries with --no-run, find their executables from the JSON artifact stream, and bounce each into
# the interactive session via the same schtasks /it path the examples use.
if ($Tests -ne "") {
  Write-Host "`n===== tests: --ignored $Tests ====="
  $json = cmd /c "cargo test -p glass-windows --no-run --message-format=json $relArg 2>&1"
  if ($LASTEXITCODE -ne 0) {
    $json | Select-Object -Last 20 | Out-Host
    Write-Host "tests($Tests): FAIL (build)"; $failures++
  } else {
    $exes = @()
    foreach ($line in $json) {
      if ($line -notmatch '"reason"') { continue }
      try { $o = $line | ConvertFrom-Json } catch { continue }
      if ($o.reason -eq "compiler-artifact" -and $o.executable -and ($o.target.kind -contains "test")) {
        $exes += $o.executable
      }
    }
    if ($exes.Count -eq 0) {
      Write-Host "tests($Tests): FAIL (no integration test binary built)"; $failures++
    } else {
      $anyFail = $false
      foreach ($exe in $exes) {
        $tag = [System.IO.Path]::GetFileNameWithoutExtension($exe)
        $r = Invoke-Interactive $exe $tag "--ignored --test-threads=1 $Tests"
        Write-Host $r.text
        if (-not ($r.ran -and $r.result -eq "0")) { $anyFail = $true }
      }
      if ($anyFail) { Write-Host "tests($Tests): FAIL"; $failures++ } else { Write-Host "tests($Tests): PASS" }
    }
  }
}

# --- 7. aggregate verdict ---
$total = $targetList.Count + ($(if ($Tests -ne "") { 1 } else { 0 }))
$passed = $total - $failures
Write-Host "`n== aggregate: $passed PASS / $failures FAIL =="
if ($failures -gt 0) { exit 1 } else { exit 0 }
