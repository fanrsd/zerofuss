# Jam milidetik raksasa untuk mengukur latensi glass-to-glass.
# Arahkan kamera drone ke layar ini, jalankan Zerofuss, lalu tangkap desktop:
# selisih jam asli dengan jam di dalam jendela Zerofuss = latensi total.
#
#   powershell -ExecutionPolicy Bypass -File latency-clock.ps1 -X 1930 -Y 700

param([int]$X = 0, [int]$Y = 0, [int]$W = 760, [int]$H = 280)

Add-Type -AssemblyName System.Windows.Forms, System.Drawing

$f = New-Object System.Windows.Forms.Form
$f.Text = 'JAM'
$f.FormBorderStyle = 'None'
$f.BackColor = [System.Drawing.Color]::Black
$f.TopMost = $true
$f.StartPosition = 'Manual'
$f.Location = New-Object System.Drawing.Point($X, $Y)
$f.Size = New-Object System.Drawing.Size($W, $H)

$l = New-Object System.Windows.Forms.Label
$l.Dock = 'Fill'
$l.ForeColor = [System.Drawing.Color]::White
$l.BackColor = [System.Drawing.Color]::Black
$l.TextAlign = 'MiddleCenter'
# Ukuran font mengikuti lebar jendela supaya 6 karakter 'ss.fff' selalu muat.
$pt = [int]($W / 5)
$l.Font = New-Object System.Drawing.Font('Consolas', $pt, [System.Drawing.FontStyle]::Bold)
$f.Controls.Add($l)
$f.Show()

# Loop manual, bukan Forms.Timer: timer di dalam skrip tidak pernah menembak.
while ($f.Visible) {
    $l.Text = (Get-Date).ToString('ss.fff')
    [System.Windows.Forms.Application]::DoEvents()
    Start-Sleep -Milliseconds 8
}
