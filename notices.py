"""Membangkitkan THIRD-PARTY-NOTICES.txt dari metadata cargo yang sungguhan.

    python notices.py

Dijalankan ulang setiap kali dependensi berubah. Daftar crate diambil dari
`cargo metadata`, bukan dari ingatan - kalau ada crate berlisensi copyleft
masuk ke pohon dependensi, berkas ini yang memperlihatkannya.
"""

import collections
import datetime
import json
import pathlib
import subprocess
import sys

AKAR = pathlib.Path(__file__).parent
KELUAR = AKAR / "THIRD-PARTY-NOTICES.txt"

TEKS_LISENSI = {
    "MIT": "https://opensource.org/license/mit",
    "Apache-2.0": "https://www.apache.org/licenses/LICENSE-2.0.txt",
    "MPL-2.0": "https://www.mozilla.org/MPL/2.0/",
    "BSD-3-Clause": "https://opensource.org/license/bsd-3-clause",
    "Zlib": "https://opensource.org/license/zlib",
    "Unicode-3.0": "https://www.unicode.org/license.txt",
    "Unlicense": "https://unlicense.org",
    "CC0-1.0": "https://creativecommons.org/publicdomain/zero/1.0/",
    "MIT-0": "https://opensource.org/license/mit-0",
    "0BSD": "https://opensource.org/license/0bsd",
}


# Platform yang benar-benar dikirim ke pengguna. Satu platform saja tidak
# cukup: kalau daftar crate hanya diambil untuk Windows, crate khusus macOS
# yang ditarik tauri dan nusb (objc2 dan kawan-kawannya, core-foundation)
# ikut didistribusikan di dalam .app tapi tidak pernah muncul di berkas ini.
PLATFORM = (
    "x86_64-pc-windows-msvc",
    "aarch64-apple-darwin",
)


def crates() -> list[tuple[str, str, str]]:
    """Gabungan crate dari semua platform di PLATFORM, tanpa duplikat.

    `cargo metadata` tidak mengompilasi apa pun - dia hanya membaca Cargo.toml
    dan Cargo.lock - jadi metadata target macOS tetap bisa diminta dari mesin
    Windows, tanpa perlu toolchain macOS terpasang.
    """
    out = set()
    for target in PLATFORM:
        # encoding="utf-8" eksplisit, bukan text=True saja: keluaran cargo
        # metadata selalu UTF-8, sedangkan text=True memakai encoding locale.
        # Di konsol cp1252 keluarannya gagal didekode di tengah jalan,
        # r.stdout jadi None, dan skripnya mati dengan galat yang menyesatkan.
        r = subprocess.run(
            ["cargo", "metadata", "--format-version", "1",
             "--filter-platform", target],
            cwd=AKAR / "app" / "src-tauri", capture_output=True,
            encoding="utf-8", errors="replace", shell=True,
        )
        if r.returncode != 0:
            sys.exit(f"cargo metadata ({target}) gagal:\n{r.stderr[-2000:]}")
        d = json.loads(r.stdout)
        pkgs = {p["id"]: p for p in d["packages"]}
        for node in d["resolve"]["nodes"]:
            p = pkgs.get(node["id"])
            if p and p["name"] != "zerofuss":
                out.add((p["name"], p["version"], p.get("license") or "TIDAK DINYATAKAN"))
    return sorted(out)


def bungkus(nama: list[str], lebar: int = 74) -> list[str]:
    """Membungkus daftar nama jadi baris-baris pendek supaya mudah dibaca."""
    baris, kini = [], ""
    for n in nama:
        calon = f"{kini}, {n}" if kini else n
        if len(calon) > lebar:
            baris.append(kini + ",")
            kini = n
        else:
            kini = calon
    if kini:
        baris.append(kini)
    return baris


def main() -> None:
    rows = crates()
    grup = collections.defaultdict(list)
    for n, v, l in rows:
        grup[l].append(f"{n} {v}")
    mpl = [f"{n} {v}" for n, v, l in rows if "MPL" in l]
    copyleft_kuat = [f"{n} {v} ({l})" for n, v, l in rows if "GPL" in l]

    b = []
    b.append("PEMBERITAHUAN KOMPONEN PIHAK KETIGA - Zerofuss")
    b.append("=" * 70)
    b.append("")
    b.append("Kode Zerofuss berlisensi MIT (lihat berkas LICENSE). Zerofuss memakai")
    b.append("perangkat lunak pihak ketiga di bawah ini; setiap komponen tetap")
    b.append("berada di bawah lisensinya sendiri.")
    b.append("")
    b.append("-" * 70)
    b.append("1. FFmpeg  -  GNU General Public License v3 atau lebih baru")
    b.append("-" * 70)
    b.append("")
    b.append("FFmpeg TIDAK termasuk dalam kode sumber Zerofuss dan tidak berada di")
    b.append("repositori ini. Zerofuss menjalankannya sebagai proses terpisah dan")
    b.append("berkomunikasi dengannya hanya lewat pipe stdin/stdout - tidak ada")
    b.append("penautan kode. Pemakai mengunduhnya sendiri (ZEROFUSS_FFMPEG atau")
    b.append("PATH); ambil-ffmpeg-mac.sh mengunduhnya untuk build macOS.")
    b.append("")
    b.append("Build yang disarankan memakai konfigurasi --enable-gpl")
    b.append("--enable-version3 --enable-libx264, sehingga berlaku GPL v3. Siapa")
    b.append("pun yang membungkus ffmpeg ke dalam installer atau .app dan")
    b.append("membagikannya wajib memenuhi kewajiban GPL v3 untuk binary itu.")
    b.append("")
    b.append("Hak cipta (C) 2000-2026 para pengembang FFmpeg.")
    b.append("  Beranda proyek : https://ffmpeg.org")
    b.append("  Kode sumber    : https://ffmpeg.org/download.html")
    b.append("  Teks GPL v3    : https://www.gnu.org/licenses/gpl-3.0.txt")
    b.append("")
    b.append("Sumber build yang dikenal baik:")
    b.append("")
    b.append("  Windows  gyan.dev, https://www.gyan.dev/ffmpeg/builds/")
    b.append("  macOS    build server FFmpeg milik Martin Riedl, dipakai oleh")
    b.append("           ambil-ffmpeg-mac.sh - https://ffmpeg.martin-riedl.de/")
    b.append("           skrip build lengkap:")
    b.append("           https://git.martin-riedl.de/ffmpeg/build-script")
    b.append("")
    b.append("-" * 70)
    b.append("2. Mesin tampilan sistem: WebView2 dan WKWebView")
    b.append("-" * 70)
    b.append("")
    b.append("Zerofuss memakai mesin tampilan milik sistem operasi, dan tidak")
    b.append("mendistribusikan ulang keduanya:")
    b.append("")
    b.append("  Windows  Microsoft Edge WebView2, disediakan oleh Windows")
    b.append("           https://developer.microsoft.com/microsoft-edge/webview2/")
    b.append("  macOS    WKWebView (WebKit), bagian dari macOS")
    b.append("           https://webkit.org - LGPL-2.1 dan BSD-2-Clause")
    b.append("")
    b.append("-" * 70)
    b.append(f"3. Tauri dan {len(rows)} crate Rust")
    b.append("-" * 70)
    b.append("")
    b.append("Ditautkan ke dalam zerofuss.exe (Windows) dan Zerofuss.app (macOS).")
    if copyleft_kuat:
        b.append("")
        b.append("PERINGATAN - copyleft kuat terdeteksi di pohon dependensi:")
        b.extend(f"  {x}" for x in copyleft_kuat)
        b.append("")
    else:
        b.append("Tidak ada dependensi berlisensi GPL/LGPL di pohon ini.")
    if mpl:
        b.append("")
        b.append("Berlisensi MPL-2.0 (copyleft tingkat berkas - sumber berkas MPL")
        b.append("wajib tersedia, dan tersedia di crates.io):")
        b.extend(f"  {x}" for x in mpl)
    b.append("")
    b.append("Sumber tiap crate: https://crates.io/crates/<nama>")
    b.append("")
    for l in sorted(grup):
        b.append(f"[{l}]  ({len(grup[l])} crate)")
        b.extend("  " + x for x in bungkus(grup[l]))
        b.append("")
    b.append("Teks lisensi:")
    for k, v in TEKS_LISENSI.items():
        b.append(f"  {k:<14}: {v}")
    b.append("")
    b.append(f"Dibangkitkan dari cargo metadata pada {datetime.date.today().isoformat()}.")
    b.append("Bangkitkan ulang dengan: python notices.py")
    b.append("")

    teks = "\n".join(b)
    KELUAR.write_text(teks, encoding="utf-8", newline="\r\n")
    print(f"{KELUAR.name}: {len(rows)} crate, {KELUAR.stat().st_size} byte")

    if copyleft_kuat:
        print("PERHATIAN: ada dependensi GPL/LGPL -", ", ".join(copyleft_kuat))


if __name__ == "__main__":
    main()
