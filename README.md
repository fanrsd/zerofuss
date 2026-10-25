# Zerofuss

Live view goggles DJI di jendela desktop, lewat kabel USB. Windows dan macOS.

Dua keluarga goggles, tiga jalur - dan jalurnya dipilih dari apa yang tercolok,
bukan dari setelan:

| Perangkat | Windows / Linux | macOS |
|---|---|---|
| Goggles 3 / N3 | adapter RNDIS milik OS + UDP, satu langkah admin untuk alamat statis | USB langsung: host RNDIS di user space, tanpa driver, tanpa adapter, tanpa admin |
| FPV Goggles V1 / V2 | USB bulk, sekali pasang WinUSB lewat Zadig | USB bulk, tanpa pemasangan apa pun |

Rust memegang transport dan decode; webview hanya menggambar. Video tidak lewat
IPC Tauri melainkan satu respons HTTP chunked di loopback - 3 MB per frame
terlalu mahal untuk JSON.

## ffmpeg

Decoder-nya ffmpeg, dan ia TIDAK ikut di repositori ini. Aplikasi mencarinya
berurutan di: variabel lingkungan `ZEROFUSS_FFMPEG` (path lengkap ke
berkasnya), di sebelah executable aplikasi, folder resource aplikasi, lalu
`PATH`.

- **Windows**: `winget install Gyan.FFmpeg`, atau unduh build dari
  [gyan.dev](https://www.gyan.dev/ffmpeg/builds/) dan taruh `ffmpeg.exe` di
  `PATH` atau di sebelah `zerofuss.exe`.
- **macOS**: `brew install ffmpeg`, atau `sh ambil-ffmpeg-mac.sh` yang
  mengunduhnya ke `bin-mac/ffmpeg` untuk dibungkus ke dalam `.app`.

## Jalankan dan bangun

Perlu [Rust](https://rustup.rs), [bun](https://bun.sh), dan ffmpeg di atas.

```
cd app
bun install
bun run tauri dev      # jalan dari sumber
bun run tauri build    # installer Windows (NSIS + MSI)
```

**macOS** (perlu Xcode Command Line Tools, rustup, bun)

```
rustup target add aarch64-apple-darwin x86_64-apple-darwin
sh ambil-ffmpeg-mac.sh     # unduh ffmpeg universal ke bin-mac/ffmpeg
sh bangun-mac.sh           # dmg arm64 + Intel; --arm atau --intel untuk satu saja
```

Selengkapnya di [`MACOS.txt`](MACOS.txt), langkah rilis di [`RILIS.txt`](RILIS.txt).

## Menguji jalur Goggles 3 di macOS

Jalur ini sudah terbukti di hardware: MacBook Air (Apple Silicon) + Goggles 3,
gambar muncul di jendela lewat `bun run tauri dev`. Perintah di bawah yang
memberi buktinya tahap demi tahap - klaim interface, INITIALIZE, MAC perangkat,
filter paket, ARP, paket open DJI, lalu hitungan paket video selama 5 detik:

```
Zerofuss.app/Contents/MacOS/zerofuss --uji-rndis
```

Yang penting bukan "gagal", tapi tahap keberapa: tujuh tahap itu punya tujuh
tindakan yang sama sekali berbeda.

## Tes

```
cargo test --manifest-path app/src-tauri/Cargo.toml
```

Seluruh protokol - RNDIS, ARP/IPv4/UDP, framing DJI, parser log ffmpeg - teruji
tanpa hardware, termasuk sambungan tiga lapisnya (RNDIS_PACKET -> Ethernet ->
UDP -> paket video). Yang butuh hardware hanya `--uji-rndis` dan `uji-usb/`.

## Lisensi

Kode Zerofuss dirilis di bawah [lisensi MIT](LICENSE) - bebas dipakai, diubah,
dan dibagikan.

**ffmpeg bukan bagian dari kode MIT ini.** Zerofuss menjalankannya sebagai
proses terpisah (lewat pipe stdin/stdout) dan tidak menyertakannya di
repositori. Build ffmpeg yang disarankan memakai GPLv3 (`--enable-gpl
--enable-version3 --enable-libx264`); kalau kamu membungkusnya ke installer
dan membagikannya, kewajiban GPLv3 untuk ffmpeg ikut berlaku untukmu.
Komponen pihak ketiga lain dan lisensinya ada di
[`THIRD-PARTY-NOTICES.txt`](THIRD-PARTY-NOTICES.txt), dibangkitkan dari
`cargo metadata` lewat `python notices.py`.

## Tentang DJI

Zerofuss adalah karya mandiri. Tidak berafiliasi dengan, tidak didukung oleh,
dan tidak disponsori oleh SZ DJI Technology Co., Ltd. "DJI", "DJI Goggles",
dan "DJI Fly" adalah merek dagang pemiliknya masing-masing, dipakai di sini
semata untuk menyebut perangkat yang kompatibel.

Zerofuss bekerja dengan mengamati lalu lintas jaringan perangkatnya sendiri
untuk mencapai interoperabilitas. Ia tidak memuat, menyalin, mengubah, atau
mendistribusikan kode, firmware, maupun berkas milik DJI, dan tidak mengubah
apa pun di dalam goggles selain meminta goggles mulai mengirim gambar.
