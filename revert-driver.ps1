# Kembalikan driver goggles ke kondisi sebelum Zadig.
# Jalankan ini SEBELUM pakai DJI Assistant 2 / DJI Fly, atau kapan pun goggles perlu dipulihkan.
#
#   powershell -ExecutionPolicy Bypass -File revert-driver.ps1          # lihat rencana
#   powershell -ExecutionPolicy Bypass -File revert-driver.ps1 -Go      # jalankan (butuh admin)

param([switch]$Go)

$pkgs = @()
$cur = $null
foreach ($line in (pnputil /enum-drivers)) {
  if ($line -match '^Published Name:\s+(\S+)') { $cur = @{ inf = $matches[1] } }
  elseif ($line -match '^Original Name:\s+(.+)$' -and $cur) { $cur.orig = $matches[1].Trim() }
  elseif ($line -match '^Provider Name:\s+(.+)$' -and $cur) {
    if ($matches[1].Trim() -eq 'libwdi' -and $cur.orig -match 'bulk_interface') { $pkgs += $cur }
    $cur = $null
  }
}

$devs = Get-PnpDevice -PresentOnly |
  Where-Object { $_.InstanceId -like '*VID_2CA3*' -and $_.FriendlyName -like '*BULK*' }

Write-Host "device yang akan dilepas ($($devs.Count)):" -ForegroundColor Cyan
$devs | Format-Table -Auto Status, FriendlyName, InstanceId
Write-Host "paket driver libwdi yang akan dihapus ($($pkgs.Count)):" -ForegroundColor Cyan
$pkgs | ForEach-Object { "  $($_.inf)  <- $($_.orig)" }

if (-not $Go) {
  Write-Host "`ndry-run. Tambahkan -Go (sebagai admin) untuk benar-benar mengembalikan." -ForegroundColor Yellow
  return
}

if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
  ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
  Write-Host "butuh hak admin." -ForegroundColor Red
  return
}

foreach ($d in $devs) { pnputil /remove-device "$($d.InstanceId)" }
foreach ($p in $pkgs) { pnputil /delete-driver $p.inf /uninstall /force }
pnputil /scan-devices

Write-Host "`nselesai. Cabut-colok goggles, lalu jalankan probe.ps1." -ForegroundColor Green
Write-Host "BULK interface harus kembali berstatus Error (tanpa driver), seperti sebelum Zadig."
