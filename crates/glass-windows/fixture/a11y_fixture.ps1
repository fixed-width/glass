# WinForms accessibility fixture for the on-box Windows tests.
#
# WinForms deliberately: those controls reach UI Automation through the legacy MSAA bridge,
# which raises structure and name changes but never IsEnabled. That is the provider whose
# behaviour `onbox_a_wait_for_enabled_falls_back_to_the_forced_reread` pins. A WPF fixture is a
# *native* UIA provider and does announce the transition, probed the same way — the opposite case.
#
# Interpreted on purpose — no build step, matching the Linux fixtures' .py and unlike the macOS
# .swift, which needs swiftc. Run: powershell.exe -NoProfile -ExecutionPolicy Bypass -File <this>
#
# Timeline from launch: the "Save" button starts disabled and becomes enabled at -EnableAfterSec
# (default 4s), late enough that a wait started after launch is already running when it flips.
param([int]$EnableAfterSec = 4)

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$form = New-Object System.Windows.Forms.Form
$form.Text = "glass a11y fixture"
$form.Size = New-Object System.Drawing.Size(400, 200)

$save = New-Object System.Windows.Forms.Button
$save.Text = "Save"
$save.Name = "Save"
$save.Enabled = $false
$save.Location = New-Object System.Drawing.Point(20, 20)
$form.Controls.Add($save)

$note = New-Object System.Windows.Forms.TextBox
$note.Text = "hello"
$note.Name = "Note"
$note.AccessibleName = "Note"
$note.Location = New-Object System.Drawing.Point(20, 60)
$form.Controls.Add($note)

$script:tick = 0
$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 1000
$timer.Add_Tick({
    $script:tick++
    if ($script:tick -eq $EnableAfterSec) {
      $save.Enabled = $true
    }
  })
$timer.Start()

[System.Windows.Forms.Application]::Run($form)
