param([string]$Title = "Glass Revision Main", [switch]$SingleWindow)

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

function New-RevisionWindow([string]$Name, [int]$X, [string]$Marker) {
    $form = New-Object System.Windows.Forms.Form
    $form.Text = $Name
    $form.StartPosition = "Manual"
    $form.Location = New-Object System.Drawing.Point($X, 100)
    $form.ClientSize = New-Object System.Drawing.Size(450, 420)

    $markerLabel = New-Object System.Windows.Forms.Label
    $markerLabel.Text = $Marker
    $markerLabel.AutoSize = $true
    $markerLabel.Location = New-Object System.Drawing.Point(15, 12)
    $form.Controls.Add($markerLabel)

    $row = 40
    foreach ($field in @("Revision target", "Revision decoy")) {
        $fieldBox = New-Object System.Windows.Forms.TextBox
        $fieldBox.AccessibleName = $field
        $fieldBox.Text = "initial"
        $fieldBox.Location = New-Object System.Drawing.Point(15, $row)
        $fieldBox.Width = 200
        $form.Controls.Add($fieldBox)
        $row += 30
    }
    foreach ($index in 1..30) {
        $label = New-Object System.Windows.Forms.Label
        $label.Text = "Fixture row $index"
        $label.AutoSize = $true
        $xPos = 15 + ([int][Math]::Floor(($index - 1) / 15) * 200)
        $yPos = 125 + ((($index - 1) % 15) * 18)
        $label.Location = New-Object System.Drawing.Point($xPos, $yPos)
        $form.Controls.Add($label)
    }
    return $form
}

function New-MainWindow {
    $form = New-RevisionWindow $Title 70 "Main window content"
    $replace = New-Object System.Windows.Forms.Button
    $replace.Text = "Replace window"
    $replace.Location = New-Object System.Drawing.Point(240, 40)
    $replace.Width = 140
    $replace.Add_Click({
        $old = $script:main
        $script:main = New-MainWindow
        $script:main.Bounds = $old.Bounds
        $script:main.Show()
        $old.Close()
    })
    $form.Controls.Add($replace)
    return $form
}

$script:main = New-MainWindow
if (-not $SingleWindow) {
    $sibling = New-RevisionWindow "$Title Sibling" 560 "Sibling window content"
    $sibling.Show()
}
$script:main.Show()
$script:main.Activate()
[System.Windows.Forms.Application]::Run()
