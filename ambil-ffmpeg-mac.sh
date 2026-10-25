#!/bin/sh
# Ambil ffmpeg statik untuk macOS dan gabung jadi SATU binary universal di
# bin-mac/ffmpeg. Dijalankan DI MAC, sekali, sebelum bangun-mac.sh.
#
#   sh ambil-ffmpeg-mac.sh
#
# Kenapa ffmpeg harus ikut masuk ke dalam .app dan bukan cukup
# `brew install ffmpeg`: aplikasi GUI yang dibuka dari Finder/Dock TIDAK
# mewarisi PATH dari shell user. Proses GUI diluncurkan oleh launchd, dan
# lingkungannya cuma /usr/bin:/bin:/usr/sbin:/sbin - /opt/homebrew/bin tidak
# ada di sana. Jadi ffmpeg yang dipasang Homebrew hanya kelihatan kalau
# aplikasinya dijalankan dari Terminal. Satu-satunya cara yang selalu jalan
# adalah menaruh binary-nya di dalam bundle, di
# Zerofuss.app/Contents/Resources/ffmpeg. Nama berkasnya wajib `ffmpeg`
# polos tanpa akhiran arsitektur - find_ffmpeg di src-tauri mencari nama itu.
#
# Sumber build: build server FFmpeg milik Martin Riedl. Dipilih karena tiga
# alasan yang bisa diperiksa: (1) ada build arm64 DAN amd64 untuk versi rilis
# yang sama, jadi lipo bisa menyatukannya; (2) skrip build-nya publik di
# git.martin-riedl.de/ffmpeg/build-script, dan itu yang membuat kewajiban GPL
# v3 pasal 6 di THIRD-PARTY-NOTICES.txt jadi benar, bukan janji kosong;
# (3) konfigurasinya --enable-version3 --enable-gpl --enable-libx264, sama
# kelas lisensinya dengan ffmpeg.exe yang dipakai di Windows.
#
# URL di bawah SUDAH diperiksa hidup (curl -sIL, 2026-08-29, dari jaringan
# pengembang):
#
#   https://ffmpeg.martin-riedl.de/                                    200
#   .../download/macos/arm64/1787073674_9.0.1/ffmpeg.zip               200
#   .../download/macos/arm64/1787073674_9.0.1/ffmpeg.zip.sha256        200
#   .../download/macos/amd64/1787081194_9.0.1/ffmpeg.zip               200
#   .../download/macos/amd64/1787081194_9.0.1/ffmpeg.zip.sha256        200
#   .../download/macos/arm64/1787073674_9.0.1/versions.txt             200
#   https://git.martin-riedl.de/ffmpeg/build-script                    200
#
# Catatan hasil ukur: endpoint /redirect/latest/... di server itu menjawab 404
# untuk HEAD tapi 200/206 untuk GET. Karena itu skrip ini TIDAK memakai
# /redirect/latest - jalur unduhan dipaku ke ID build yang tetap, supaya hasil
# bisa diulang dan sha256-nya bisa dipaku juga.

set -eu

FFMPEG_VERSI="9.0.1"

# ID build tetap (stempel waktu + versi) - isinya tidak pernah berubah,
# jadi sha256 di bawah tetap sah. sha256 diambil dari berkas .sha256 milik
# server itu sendiri pada 2026-08-29.
BASIS="https://ffmpeg.martin-riedl.de/download/macos"
BUILD_ARM64="1787073674_${FFMPEG_VERSI}"
BUILD_X86_64="1787081194_${FFMPEG_VERSI}"
SHA_ARM64="8287a1b2229e05eb41859f073e18e6c52c60a778f2f5e6881070fe51b79407fe"
SHA_X86_64="5bdead62ff504ab9b447cc72b212c4fb481e3f7de5877d427a51bee8136dda40"

AKAR=$(cd "$(dirname "$0")" && pwd)
TUJUAN="$AKAR/bin-mac/ffmpeg"
# Dibangun ke berkas sementara dulu. Kalau ada satu langkah saja yang gagal,
# set -e menghentikan skrip dan bin-mac/ffmpeg yang lama tetap utuh - tidak
# pernah ada keadaan setengah tertimpa.
BARU="$AKAR/bin-mac/ffmpeg.baru"

mati() {
    printf '\nGAGAL: %s\n' "$1" >&2
    exit 1
}

[ "$(uname -s)" = "Darwin" ] || mati "skrip ini hanya untuk macOS (uname -s = $(uname -s)). Di Windows pasang ffmpeg sendiri (lihat README.md)."
command -v curl >/dev/null 2>&1 || mati "curl tidak ditemukan"
command -v unzip >/dev/null 2>&1 || mati "unzip tidak ditemukan"
command -v shasum >/dev/null 2>&1 || mati "shasum tidak ditemukan"

SEMENTARA=$(mktemp -d "${TMPDIR:-/tmp}/zerofuss-ffmpeg.XXXXXX")
bersih() { rm -rf "$SEMENTARA" "$BARU"; }
trap bersih EXIT INT TERM

# $1 nama arsitektur lokal, $2 nama arsitektur di server, $3 ID build, $4 sha256
ambil() {
    arsip="$SEMENTARA/$1.zip"
    url="$BASIS/$2/$3/ffmpeg.zip"
    printf 'unduh %-7s %s\n' "$1" "$url"
    if ! curl -fL --retry 3 --connect-timeout 20 --progress-bar -o "$arsip" "$url"; then
        printf '  tidak bisa diunduh (%s)\n' "$1" >&2
        return 1
    fi
    nyata=$(shasum -a 256 "$arsip" | cut -d' ' -f1)
    if [ "$nyata" != "$4" ]; then
        printf '  sha256 tidak cocok (%s)\n    harap : %s\n    dapat : %s\n' "$1" "$4" "$nyata" >&2
        return 1
    fi
    printf '  sha256 cocok\n'
    mkdir -p "$SEMENTARA/$1"
    unzip -o -q "$arsip" ffmpeg -d "$SEMENTARA/$1" || {
        printf '  arsip tidak memuat berkas ffmpeg (%s)\n' "$1" >&2
        return 1
    }
    [ -f "$SEMENTARA/$1/ffmpeg" ] || return 1
    return 0
}

printf 'ffmpeg %s untuk macOS -> bin-mac/ffmpeg\n\n' "$FFMPEG_VERSI"

ada_arm64=0
ada_x86_64=0
ambil arm64 arm64 "$BUILD_ARM64" "$SHA_ARM64" && ada_arm64=1 || true
ambil x86_64 amd64 "$BUILD_X86_64" "$SHA_X86_64" && ada_x86_64=1 || true

mkdir -p "$AKAR/bin-mac"
rm -f "$BARU"

if [ "$ada_arm64" = 1 ] && [ "$ada_x86_64" = 1 ]; then
    command -v lipo >/dev/null 2>&1 || mati "lipo tidak ada - pasang Command Line Tools: xcode-select --install"
    lipo -create -output "$BARU" "$SEMENTARA/arm64/ffmpeg" "$SEMENTARA/x86_64/ffmpeg"
    printf '\nuniversal: arm64 + x86_64\n'
elif [ "$ada_arm64" = 1 ]; then
    cp "$SEMENTARA/arm64/ffmpeg" "$BARU"
    printf '\nPERINGATAN: hanya arm64 yang berhasil diambil. Hasilnya BUKAN\n'
    printf 'universal - .app-nya tidak akan bisa memutar video di Mac Intel.\n'
elif [ "$ada_x86_64" = 1 ]; then
    cp "$SEMENTARA/x86_64/ffmpeg" "$BARU"
    printf '\nPERINGATAN: hanya x86_64 yang berhasil diambil. Hasilnya BUKAN\n'
    printf 'universal - di Apple Silicon ffmpeg akan jalan lewat Rosetta 2,\n'
    printf 'dan kalau Rosetta belum dipasang video tidak akan muncul sama sekali.\n'
else
    mati "tidak ada satu pun arsitektur yang berhasil diambil. Periksa koneksi, lalu cek daftar build terbaru di https://ffmpeg.martin-riedl.de/ - kalau ID build di skrip ini sudah dihapus dari server, perbarui BUILD_ARM64/BUILD_X86_64 beserta sha256-nya."
fi

chmod +x "$BARU"

# Di Apple Silicon binary TANPA tanda tangan langsung dibunuh kernel
# (Killed: 9) - tanda tangan ad-hoc pun cukup, tapi harus ada. lipo -create
# membuang tanda tangan kedua potongan, jadi hasil gabungan WAJIB
# ditandatangani ulang. Untuk kasus satu arsitektur tanda tangan aslinya masih
# utuh dan tidak kita sentuh.
if command -v codesign >/dev/null 2>&1; then
    if ! codesign --verify "$BARU" >/dev/null 2>&1; then
        codesign --force --sign - --timestamp=none "$BARU" >/dev/null 2>&1 \
            || mati "codesign ad-hoc gagal - tanpa tanda tangan, ffmpeg akan dibunuh kernel di Apple Silicon"
        codesign --verify "$BARU" >/dev/null 2>&1 \
            || mati "codesign selesai tapi verifikasi tetap gagal"
        printf 'tanda tangan: ad-hoc dipasang\n'
    else
        printf 'tanda tangan: sudah sah, tidak diubah\n'
    fi
else
    printf 'PERINGATAN: codesign tidak ada, tanda tangan tidak diperiksa.\n'
fi

# Uji cepat: binary-nya benar-benar mau jalan di mesin ini.
"$BARU" -hide_banner -version >/dev/null 2>&1 \
    || mati "$BARU tidak bisa dijalankan di mesin ini"

mv "$BARU" "$TUJUAN"
trap - EXIT INT TERM
rm -rf "$SEMENTARA"

printf '\nselesai: %s\n' "$TUJUAN"
# `cmd && cmd` sebagai perintah utuh: kalau yang kiri gagal, seluruh daftar
# gagal dan set -e mematikan skrip. Karena itu dipakai if, bukan &&.
if command -v lipo >/dev/null 2>&1; then
    lipo -info "$TUJUAN"
fi
shasum -a 256 "$TUJUAN"
printf '%s byte\n' "$(wc -c < "$TUJUAN" | tr -d ' ')"
printf '\nlanjut: sh bangun-mac.sh\n'
