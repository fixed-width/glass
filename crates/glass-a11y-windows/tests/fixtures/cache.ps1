param([string]$ReadyFile, [int]$Count = 24, [int]$Depth = 4, [switch]$WinForms)
$ErrorActionPreference = 'Stop'
if ($WinForms) {
    Add-Type -AssemblyName System.Windows.Forms
    $form = New-Object Windows.Forms.Form
    $form.Text = 'Glass UIA cache fixture'
    $form.Width = 600
    $form.Height = 800
    $form.SuspendLayout()
    for ($i = 0; $i -lt $Count; $i++) {
        if ($i % 2 -eq 0) {
            $control = New-Object Windows.Forms.CheckBox
            $control.Checked = $true
        } else {
            $control = New-Object Windows.Forms.TextBox
            $control.Text = "Value $i"
        }
        $control.AccessibleName = "Control $i"
        $control.SetBounds(10, 10 + $i * 25, 200, 24)
        $form.Controls.Add($control)
    }
    $form.ResumeLayout()
    $form.Add_Shown({ [IO.File]::WriteAllText($ReadyFile, $form.Handle.ToInt64().ToString()) })
    [Windows.Forms.Application]::Run($form)
    exit
}
Add-Type -AssemblyName PresentationFramework
$window = New-Object Windows.Window
$window.Title = 'Glass UIA cache fixture'
$window.Width = 600
$window.Height = 800
$panel = New-Object Windows.Controls.StackPanel
$window.Content = $panel
function Add-Control($control, $name) {
    [Windows.Automation.AutomationProperties]::SetName($control, $name)
    [void]$panel.Children.Add($control)
    return $control
}
$note = Add-Control (New-Object Windows.Controls.TextBox) 'Note'
$note.Text = 'hello'
[Windows.Automation.AutomationProperties]::SetHelpText($note, 'Extra help')
$readonly = Add-Control (New-Object Windows.Controls.TextBox) 'Read only'
$readonly.Text = 'fixed'
$readonly.IsReadOnly = $true
$check = Add-Control (New-Object Windows.Controls.CheckBox) 'Checked'
$check.IsChecked = $true
$toggle = Add-Control (New-Object Windows.Controls.Primitives.ToggleButton) 'Toggle'
$toggle.IsChecked = $true
$slider = Add-Control (New-Object Windows.Controls.Slider) 'Slider'
$slider.Maximum = 100
$slider.Value = 42
$progress = Add-Control (New-Object Windows.Controls.ProgressBar) 'Progress'
$progress.Value = 63
$disabled = Add-Control (New-Object Windows.Controls.Button) 'Disabled'
$disabled.IsEnabled = $false
$list = Add-Control (New-Object Windows.Controls.ListBox) 'Choices'
$item = New-Object Windows.Controls.ListBoxItem
$item.Content = 'Selected item'
[void]$list.Items.Add($item)
$list.SelectedIndex = 0
$tree = Add-Control (New-Object Windows.Controls.TreeView) 'Nested'
$parent = $tree
for ($i = 0; $i -lt $Depth; $i++) {
    $item = New-Object Windows.Controls.TreeViewItem
    $item.Header = "Level $i"
    $item.IsExpanded = $true
    [void]$parent.Items.Add($item)
    $parent = $item
}
for ($i = 0; $i -lt $Count; $i++) {
    $button = Add-Control (New-Object Windows.Controls.Button) "Button $i"
    $button.Content = "Button $i"
}
$window.Add_ContentRendered({
    $helper = New-Object Windows.Interop.WindowInteropHelper($window)
    [IO.File]::WriteAllText($ReadyFile, $helper.Handle.ToInt64().ToString())
})
[void]$window.ShowDialog()
