Uji jalur USB bulk terhadap hardware sungguhan.

  cd uji-usb && cargo run

Menjalankan langkah yang sama persis dengan app/src-tauri/src/usb.rs:
enumerasi -> open -> claim interface 3 -> kirim RMVT -> baca 0x84 -> pancing
ulang saat sunyi.

Yang diharapkan:

  FPV Goggles V1/V2 (PID 001f)  potongan data mengalir, byte awal 00 00 01 ...
  Goggles 3 / N3    (PID 0020)  claim OK, RMVT GAGAL, 0 byte

Kalau claim gagal dengan kind=Unsupported/NotFound: WinUSB belum di-bind ke
interface 3. Pasang lewat Zadig - lihat Panduan bagian 7 di aplikasi.

Kalau claim gagal dengan kind=Busy/PermissionDenied: drivernya SUDAH ada,
interface-nya cuma sedang dipakai program lain (DigiView, tab browser, atau
Zerofuss sendiri).


------------------------------------------------------------------------------
HASIL UKUR: Goggles 3 TIDAK bisa memakai jalur ini
------------------------------------------------------------------------------

Diuji pada Goggles3-753XM647025QMU (PID 0020) dengan WinUSB ter-bind ke MI_03,
LiveView Sharing MENYALA, dan video sedang mengalir lewat RNDIS di saat yang
sama:

  claim interface 3         OK
  endpoint                  OUT 0x03 max 512 | IN 0x84 max 512   <- bentuk
                            descriptor-nya IDENTIK dengan V1/V2
  baca pasif 3 s            0 byte
  tulis RMVT timeout 500ms  Err(Cancelled)
  tulis RMVT timeout 3 s    Err(Cancelled)
  clear_halt OUT dan IN     Ok(()) - tidak mengubah apa pun
  tulis RMVT sesudah halt   Err(Cancelled)
  baca 4 s sesudahnya       0 byte

Jadi IF3 di Goggles 3 bukan cuma menolak perintah RMVT - endpoint OUT-nya
tidak menerima tulisan APA PUN (timeout, bukan stall), dan endpoint IN-nya
tidak pernah mengeluarkan satu byte pun. Interface itu ada di descriptor tapi
dorman.

Ini mereproduksi temuan brendan779/N3Live recon/FINDINGS.md secara independen:
"IF3, IF5, IF6, IF7 are dormant. Their bulk-OUT endpoints time out on write
and their bulk-IN endpoints are silent." dan "The legacy voc-poc start packet
(0x524d5654 "RMVT") does not start video on the N3."

Kesimpulan yang dipakai kode: PID 0020 SELALU dirutekan ke jalur RNDIS, dan
kesamaan bentuk descriptor tidak boleh dipakai sebagai dasar deteksi - hanya
PID. Lihat komentar `PID_RNDIS` di app/src-tauri/src/usb.rs.

Membalikkan driver IF3 (tidak wajib - Zerofuss tidak memakainya di Goggles 3):
Device Manager -> klik kanan device -> Uninstall device -> centang "Attempt to
remove the driver" -> cabut-colok kabel.
