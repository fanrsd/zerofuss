# Zerofuss P0 — snapshot state goggles di USB.
# Jalankan DUA kali: (a) goggles nyala saja, (b) goggles + drone/air unit nyala & linked.
# Bandingkan hasilnya: interface / endpoint bisa berubah saat goggles broadcasting.
# Syarat: Settings > About > 'OTG Wired Connection to Computer' = ON.

Write-Host "== USB interface DJI (VID_2CA3) ==" -ForegroundColor Cyan
$dji = Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -like '*VID_2CA3*' }
if (-not $dji) {
  Write-Host "  tidak ada. Cek kabel USB-C data, goggles nyala, OTG Wired Connection = ON." -ForegroundColor Yellow
  return
}
$dji | Sort-Object InstanceId | Format-Table -Auto Status, Class, FriendlyName, InstanceId

Write-Host "== Link RNDIS ==" -ForegroundColor Cyan
$nic = Get-NetAdapter | Where-Object InterfaceDescription -like '*Remote NDIS*'
if (-not $nic) { Write-Host "  adapter RNDIS tidak ada." -ForegroundColor Yellow }
else {
  $nic | Format-Table -Auto Name, Status, LinkSpeed, ifIndex
  Get-NetIPAddress -InterfaceIndex $nic.ifIndex | Format-Table -Auto IPAddress, AddressFamily, PrefixLength, PrefixOrigin
  # tetangga non-multicast = tanda goggles benar-benar bicara di link ini
  $peers = Get-NetNeighbor -InterfaceIndex $nic.ifIndex -ErrorAction SilentlyContinue |
    Where-Object { $_.State -ne 'Permanent' }
  if ($peers) { Write-Host "  PEER TERDETEKSI:" -ForegroundColor Green; $peers | Format-Table -Auto IPAddress, LinkLayerAddress, State }
  else { Write-Host "  tidak ada peer. Goggles diam di jalur IP (DHCP mati, tidak menjawab)." -ForegroundColor Yellow }
}

Write-Host "== Ringkasan ==" -ForegroundColor Cyan
$bulk = @($dji | Where-Object FriendlyName -like '*BULK*')
"  BULK interface       : {0} (driver terpasang: {1})" -f $bulk.Count, @($bulk | Where-Object Status -eq 'OK').Count
"  IP dari goggles      : {0}" -f $(if (Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue | Where-Object IPAddress -like '192.168.42.*') { 'ya' } else { 'tidak (APIPA/link-local saja)' })
