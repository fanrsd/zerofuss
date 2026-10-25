#!/bin/sh
# Bangun Zerofuss.app + .dmg untuk macOS.
#
#   sh ambil-ffmpeg-mac.sh     <- sekali saja, ambil ffmpeg
#   sh bangun-mac.sh           <- ini
#
# Argumen:
#
#   (tanpa argumen)  dmg Apple Silicon + dmg Intel        <- yang dirilis
#   --arm            hanya dmg Apple Silicon
#   --intel          hanya dmg Intel
#   --universal      tambah satu bundle universal-apple-darwin
#
# Bisa digabung: `sh bangun-mac.sh --arm --universal`.
#
# Kenapa yang dirilis per-arsitektur, bukan universal:
#
#   - Bundle universal berisi DUA salinan biner (Zerofuss dan ffmpeg), jadi
#     unduhannya kira-kira dua kali ukuran yang sebenarnya dipakai satu Mac.
#     Per-arsitektur memotongnya separuh, dan tidak ada Mac yang menjalankan
#     kedua potongan itu sekaligus.
#   - Pengelola paket seperti Homebrew cask memilih per-arsitektur sendiri,
#     dan itu butuh dua berkas terpisah - universal tidak muat di sana tanpa
#     membuang keuntungannya.
#
# Universal tetap disediakan di belakang flag karena berguna untuk satu hal:
# menyalin .app ke Mac orang lain yang arsitekturnya tidak kamu tahu.
#
# Perkakas frontend-nya bun, bukan npm: app/ memakai bun.lock dan
# @tauri-apps/cli terpasang sebagai devDependency dengan skrip "tauri", jadi
# perintah resminya `bun run tauri build`.
#
# Tanda tangan dan notarisasi TIDAK diatur di skrip ini. Semuanya lewat
# variabel lingkungan yang memang sudah dibaca Tauri CLI sendiri - kalau
# variabelnya kosong, build jalan tanpa tanda tangan dan tetap selesai.
# Nama variabelnya diperiksa terhadap dua sumber, bukan ingatan:
#
#   https://v2.tauri.app/distribute/sign/macos/   (dicek 2026-08-29, HTTP 200)
#     APPLE_CERTIFICATE           .p12 dalam base64
#     APPLE_CERTIFICATE_PASSWORD  sandi .p12 tersebut
#     APPLE_SIGNING_IDENTITY      nama identitas dari
#                                 `security find-identity -v -p codesigning`
#     APPLE_ID, APPLE_PASSWORD    kredensial notarisasi lewat Apple ID
#
#   crates/tauri-bundler/src/bundle/macos/sign.rs di repo tauri-apps/tauri
#   (dicek 2026-08-29, HTTP 200) - di sana APPLE_ID, APPLE_PASSWORD dan
#   APPLE_TEAM_ID dibaca sebagai satu paket bertiga, jadi APPLE_TEAM_ID wajib
#   ikut walau halaman dokumentasinya tidak menyebutnya. Alternatifnya
#   APPLE_API_KEY + APPLE_API_ISSUER + APPLE_API_KEY_PATH (App Store Connect
#   API); skrip ini hanya melaporkan mana yang terisi, tidak mengubahnya.
#
# Satu hal yang BUKAN lewat variabel lingkungan: bundle-nya dibangun dengan
# hardenedRuntime menyala dan entitlement app/src-tauri/Entitlements.plist
# yang memberi com.apple.security.device.usb. Tanpa entitlement itu build yang
# ditandatangani dan dinotarisasi tidak bisa membuka interface USB goggles -
# aplikasinya terbuka, USB-nya tidak. Itu jalan satu-satunya buat Goggles 3 /
# N3 di macOS (host RNDIS di user space) dan buat V1/V2. Diatur di sisi Tauri,
# bukan di sini.

set -eu

AKAR=$(cd "$(dirname "$0")" && pwd)
cd "$AKAR"

mati() {
    printf '\nGAGAL: %s\n' "$1" >&2
    exit 1
}

[ "$(uname -s)" = "Darwin" ] || mati "skrip ini hanya untuk macOS (uname -s = $(uname -s))"

# 0. Argumen. Tanpa argumen = arm + intel, karena itu pasangan yang dirilis.
ARM=0
INTEL=0
UNIVERSAL=0
for a in "$@"; do
    case "$a" in
        --arm)       ARM=1 ;;
        --intel)     INTEL=1 ;;
        --universal) UNIVERSAL=1 ;;
        -h|--help)
            printf 'pemakaian: sh bangun-mac.sh [--arm] [--intel] [--universal]\n'
            printf 'tanpa argumen: --arm --intel\n'
            exit 0
            ;;
        *) mati "argumen tidak dikenal: $a (yang ada: --arm, --intel, --universal)" ;;
    esac
done
if [ "$ARM$INTEL$UNIVERSAL" = "000" ]; then
    ARM=1
    INTEL=1
fi

# Satu daftar triple yang dipakai untuk semuanya: pemeriksaan target rustup,
# perintah build, dan ringkasan di akhir. universal-apple-darwin ikut sebagai
# anggota daftar karena `--target universal-apple-darwin` bentuknya sama saja
# bagi Tauri, cuma isinya yang digabung lipo.
DAFTAR=""
if [ "$ARM" = 1 ];       then DAFTAR="$DAFTAR aarch64-apple-darwin"; fi
if [ "$INTEL" = 1 ];     then DAFTAR="$DAFTAR x86_64-apple-darwin"; fi
if [ "$UNIVERSAL" = 1 ]; then DAFTAR="$DAFTAR universal-apple-darwin"; fi

# Versi untuk nama artefak yang dicetak di ringkasan. Dibaca dengan sed, bukan
# jq: jq bukan bawaan macOS, dan satu baris "version" di tauri.conf.json tidak
# membutuhkan parser JSON utuh. `sed -n 1p` menjaga hasilnya tetap satu baris
# kalau suatu saat ada kunci "version" bersarang.
VERSI=$(sed -n 's/^  "version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    app/src-tauri/tauri.conf.json | sed -n 1p)
[ -n "$VERSI" ] || mati "tidak bisa membaca \"version\" dari app/src-tauri/tauri.conf.json"

# Nama dmg yang seharusnya dihasilkan Tauri per target. Label arsitekturnya
# bukan nama triple: aarch64 dan x64, itu yang dipakai bundler dan itu yang
# dipakai di nama rilis. Universal tidak dipaku di mana pun, jadi namanya tidak
# diklaim di sini - cukup daftar isi direktori.
nama_dmg() {
    case "$1" in
        aarch64-apple-darwin)   printf 'Zerofuss_%s_aarch64.dmg' "$VERSI" ;;
        x86_64-apple-darwin)    printf 'Zerofuss_%s_x64.dmg' "$VERSI" ;;
        universal-apple-darwin) printf '' ;;
    esac
}

printf 'versi    : %s\n' "$VERSI"

# 1. ffmpeg yang akan dibungkus ke Contents/Resources/ffmpeg.
[ -f bin-mac/ffmpeg ] || mati "bin-mac/ffmpeg tidak ada. Jalankan dulu: sh ambil-ffmpeg-mac.sh"
[ -x bin-mac/ffmpeg ] || mati "bin-mac/ffmpeg ada tapi tidak executable. Perbaiki: chmod +x bin-mac/ffmpeg"
printf 'ffmpeg   : bin-mac/ffmpeg'
if command -v lipo >/dev/null 2>&1; then
    printf ' (%s)' "$(lipo -archs bin-mac/ffmpeg 2>/dev/null || echo 'arsitektur tidak terbaca')"
fi
printf '\n'

# 2. Command Line Tools - tanpa ini tidak ada clang, tidak ada lipo, tidak ada
#    codesign, dan penautan Rust gagal di tengah build yang panjang.
xcode-select -p >/dev/null 2>&1 || mati "Xcode Command Line Tools belum terpasang. Jalankan: xcode-select --install"

# 3. Target Rust yang benar-benar dipakai build kali ini. universal-apple-darwin
#    bukan target tunggal - Tauri membangun aarch64 dan x86_64 lalu
#    menggabungnya, jadi flag itu menuntut dua-duanya terpasang.
command -v rustup >/dev/null 2>&1 || mati "rustup tidak ada. Pasang Rust dari https://rustup.rs"
TARGET_TERPASANG=$(rustup target list --installed)
PERLU=""
for t in $DAFTAR; do
    case "$t" in
        universal-apple-darwin) PERLU="$PERLU aarch64-apple-darwin x86_64-apple-darwin" ;;
        *)                      PERLU="$PERLU $t" ;;
    esac
done
for t in $PERLU; do
    printf '%s\n' "$TARGET_TERPASANG" | grep -qx "$t" \
        || mati "target Rust $t belum terpasang. Jalankan: rustup target add $t"
done
printf 'target   :%s\n' "$DAFTAR"

# 4. bun dan dependensi frontend.
command -v bun >/dev/null 2>&1 || mati "bun tidak ada. Pasang dari https://bun.sh lalu ulangi"
if [ ! -d app/node_modules ]; then
    printf 'app/node_modules belum ada - menjalankan bun install\n'
    ( cd app && bun install ) || mati "bun install gagal"
fi

# 5. Laporkan mode tanda tangan, dan tandatangani ffmpeg lebih dulu kalau
#    memang menandatangani. Sengaja sebelum build supaya tidak ada kejutan di
#    akhir proses yang lamanya belasan menit - dan sekali saja, walau
#    bundle-nya nanti dua atau tiga.
BERTANDA=0
if [ -n "${APPLE_SIGNING_IDENTITY:-}" ] || [ -n "${APPLE_CERTIFICATE:-}" ]; then
    BERTANDA=1
    # if, bukan `[ ... ] && printf`: sebagai perintah utuh daftar && yang
    # gagal di sisi kiri ikut menggagalkan perintahnya, dan set -e langsung
    # mematikan skrip di sini.
    printf 'tanda    : ditandatangani'
    if [ -n "${APPLE_SIGNING_IDENTITY:-}" ]; then printf ' (APPLE_SIGNING_IDENTITY)'; fi
    if [ -n "${APPLE_CERTIFICATE:-}" ]; then printf ' (APPLE_CERTIFICATE)'; fi
    printf '\n'
    if { [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_PASSWORD:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ]; } \
        || { [ -n "${APPLE_API_KEY:-}" ] && [ -n "${APPLE_API_ISSUER:-}" ]; }; then
        printf 'notaris  : kredensial lengkap, notarisasi akan dijalankan Tauri\n'
    else
        printf 'notaris  : TIDAK dijalankan - butuh APPLE_ID + APPLE_PASSWORD +\n'
        printf '           APPLE_TEAM_ID, atau APPLE_API_KEY + APPLE_API_ISSUER +\n'
        printf '           APPLE_API_KEY_PATH\n'
    fi
    # bin-mac/ffmpeg jadi Mach-O bersarang di Contents/Resources. Notarisasi
    # menolak executable bersarang yang tidak ditandatangani, dan bundle-nya
    # dibangun dengan hardenedRuntime, jadi --options runtime harus ikut.
    # Tauri tidak menandatangani berkas resource satu per satu, jadi ini
    # dikerjakan di sini, sebelum berkasnya disalin ke dalam bundle.
    if [ -n "${APPLE_SIGNING_IDENTITY:-}" ]; then
        codesign --force --timestamp --options runtime \
            --sign "$APPLE_SIGNING_IDENTITY" bin-mac/ffmpeg \
            || mati "gagal menandatangani bin-mac/ffmpeg - notarisasi akan menolak bundle dengan executable bersarang tanpa tanda tangan"
        printf 'ffmpeg   : ditandatangani ulang dengan --options runtime\n'
    else
        printf 'ffmpeg   : TIDAK ditandatangani ulang - APPLE_SIGNING_IDENTITY\n'
        printf '           kosong, isi variabel itu kalau bundle-nya mau\n'
        printf '           dinotarisasi\n'
    fi
else
    printf 'tanda    : TANPA tanda tangan (APPLE_SIGNING_IDENTITY dan\n'
    printf '           APPLE_CERTIFICATE dua-duanya kosong)\n'
fi

# 6. Build, satu target per putaran. Berhenti di kegagalan pertama: dua dmg
#    yang cuma separuh jadi lebih menyesatkan daripada tidak ada dmg.
for t in $DAFTAR; do
    printf '\n== bun run tauri build --target %s ==\n\n' "$t"
    ( cd app && bun run tauri build --target "$t" ) \
        || mati "tauri build --target $t gagal - baca pesan terakhir di atas, itu penyebabnya"
done

# 7. Ringkasan. Dicetak sesudah semua build supaya jalur hasilnya berkumpul di
#    satu tempat, bukan terselip belasan menit di atas.
printf '\nselesai.\n'
KURANG=0
for t in $DAFTAR; do
    HASIL="app/src-tauri/target/$t/release/bundle"
    HARAP=$(nama_dmg "$t")
    printf '\n  %s\n' "$t"
    printf '    .app : %s/macos/Zerofuss.app\n' "$HASIL"
    printf '    .dmg : %s/dmg/\n' "$HASIL"
    if [ -n "$HARAP" ]; then
        printf '           seharusnya %s\n' "$HARAP"
    fi
    if [ -d "$HASIL/dmg" ]; then
        for d in "$HASIL"/dmg/*.dmg; do
            if [ -f "$d" ]; then printf '           %s\n' "$d"; fi
        done
    fi

    # Periksa ffmpeg benar-benar masuk ke bundle. Kalau pemetaan resources di
    # tauri.macos.conf.json meleset, aplikasinya tetap terbangun tapi tidak
    # akan pernah bisa memutar video - lebih baik ketahuan sekarang.
    FF_BUNDLE="$HASIL/macos/Zerofuss.app/Contents/Resources/ffmpeg"
    if [ -x "$FF_BUNDLE" ]; then
        printf '    ffmpeg : Contents/Resources/ffmpeg ada dan executable\n'
    else
        printf '    ffmpeg : TIDAK ADA atau tidak executable\n'
        KURANG=1
    fi
done

if [ "$KURANG" = 1 ]; then
    printf '\nPERINGATAN: ada bundle tanpa Contents/Resources/ffmpeg yang\n'
    printf 'executable. Aplikasinya akan terbuka tapi video tidak akan jalan.\n'
fi

if [ "$BERTANDA" = 0 ]; then
    printf '\nBuild ini tanpa tanda tangan. Gatekeeper akan menolak membukanya\n'
    printf 'dengan pesan "aplikasi rusak" sampai atribut karantina dibuang.\n'
    printf 'Sesudah menyalin .app ke /Applications, jalankan:\n\n'
    printf '  xattr -dr com.apple.quarantine /Applications/Zerofuss.app\n'
fi
