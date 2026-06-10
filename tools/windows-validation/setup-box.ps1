<#
.SYNOPSIS
  One-time prep for the glass Windows validation box, driven remotely from another
  machine: auto-login (so a monitorless reboot returns to a composited session),
  no sleep / no auto-lock, Remote Desktop, and the OpenSSH server (for edit+build).

.DESCRIPTION
  Run elevated. Pass -User/-Password to enable auto-login. After this, install a
  console-mirroring viewer (Sunshine, or VNC) for *running* the capture/input tests,
  and a signed IddCx virtual display driver for the headless test — see REMOTE.md.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File .\setup-box.ps1 -User glass -Password 'hunter2'
#>
param(
    [string]$User,
    [string]$Password
)

$ErrorActionPreference = 'Stop'

function Assert-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $p = New-Object Security.Principal.WindowsPrincipal($id)
    if (-not $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Run this script from an elevated (Administrator) PowerShell.'
    }
}

Assert-Admin

Write-Host '== Disabling sleep / monitor timeout (AC) ==' -ForegroundColor Cyan
powercfg /change standby-timeout-ac 0
powercfg /change monitor-timeout-ac 0
powercfg /change hibernate-timeout-ac 0

Write-Host '== Preventing auto-lock during tests (reversible) ==' -ForegroundColor Cyan
# No idle machine-lock (the capture/input session must stay the active input desktop).
New-Item 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System' -Force | Out-Null
Set-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System' -Name InactivityTimeoutSecs -Value 0 -Type DWord
# No screensaver / no password-on-resume.
Set-ItemProperty 'HKCU:\Control Panel\Desktop' -Name ScreenSaveActive -Value '0'
Set-ItemProperty 'HKCU:\Control Panel\Desktop' -Name ScreenSaverIsSecure -Value '0' -ErrorAction SilentlyContinue
powercfg -setacvalueindex SCHEME_CURRENT SUB_NONE CONSOLELOCK 0
powercfg -setactive SCHEME_CURRENT
Write-Host '   (manual Win+L still works; only idle auto-lock is disabled)'

Write-Host '== Enabling Remote Desktop (dev only — see REMOTE.md caveat) ==' -ForegroundColor Cyan
Set-ItemProperty 'HKLM:\System\CurrentControlSet\Control\Terminal Server' -Name fDenyTSConnections -Value 0
Enable-NetFirewallRule -DisplayGroup 'Remote Desktop' -ErrorAction SilentlyContinue

Write-Host '== Enabling OpenSSH server (edit + build over SSH) ==' -ForegroundColor Cyan
$cap = Get-WindowsCapability -Online -Name 'OpenSSH.Server*' | Select-Object -First 1
if ($cap -and $cap.State -ne 'Installed') {
    Add-WindowsCapability -Online -Name $cap.Name | Out-Null
}
Set-Service -Name sshd -StartupType Automatic
Start-Service sshd
if (-not (Get-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -DisplayName 'OpenSSH Server (sshd)' `
        -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 | Out-Null
}
# Nicer interactive shell over SSH (optional).
New-Item 'HKLM:\SOFTWARE\OpenSSH' -Force | Out-Null
Set-ItemProperty 'HKLM:\SOFTWARE\OpenSSH' -Name DefaultShell `
    -Value "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -ErrorAction SilentlyContinue

if ($User -and $Password) {
    Write-Host '== Enabling auto-login ==' -ForegroundColor Cyan
    Write-Warning 'Auto-login stores the password in the registry in CLEARTEXT. Use a throwaway local account, or prefer Sysinternals Autologon (which obfuscates it).'
    $key = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $key -Name AutoAdminLogon -Value '1'
    Set-ItemProperty $key -Name DefaultUserName -Value $User
    Set-ItemProperty $key -Name DefaultPassword -Value $Password
    Set-ItemProperty $key -Name DefaultDomainName -Value $env:COMPUTERNAME
    Write-Host "auto-login set for $env:COMPUTERNAME\$User" -ForegroundColor Green
} else {
    Write-Host '== Skipping auto-login (no -User/-Password) ==' -ForegroundColor Yellow
    Write-Host '   Re-run with -User <name> -Password <pw>, or use Sysinternals Autologon.'
}

Write-Host ''
Write-Host ("reach this box at:  {0}" -f $env:COMPUTERNAME) -ForegroundColor Cyan
Get-NetIPAddress -AddressFamily IPv4 |
    Where-Object { $_.IPAddress -notlike '169.254.*' -and $_.IPAddress -ne '127.0.0.1' } |
    ForEach-Object { Write-Host ("  ssh {0}@{1}" -f $env:USERNAME, $_.IPAddress) }

Write-Host ''
Write-Host 'Next (see REMOTE.md):' -ForegroundColor Cyan
Write-Host '  1. Install a console-mirroring viewer to RUN the capture/input tests:'
Write-Host '       Sunshine (host) + Moonlight (Linux client)  https://github.com/LizardByte/Sunshine'
Write-Host '     (Plain RDP spawns a separate session and breaks capture/input on disconnect.)'
Write-Host '  2. Install a signed IddCx virtual display driver for the headless test:'
Write-Host '       https://github.com/nomi-san/parsec-vdd            (MIT, signed)'
Write-Host '  3. Install the Rust MSVC toolchain + "Desktop development with C++".'
Write-Host '  4. From Linux, run the on-box suite:  ./scripts/test-windows.sh --tests onbox'
Write-Host '  5. Reboot to verify auto-login returns to a usable session unattended.'
