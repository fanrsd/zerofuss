"""Ukur latensi glass-to-glass Zerofuss dalam satu tangkapan.

  python latshot.py                 # baca jendela 'Zerofuss'
  python latshot.py 0.333 0.468     # geser titik jam kalau kamera dipindah

Jalankan latency-clock.ps1 lebih dulu. Hasil: banding.png berisi jam asli
(atas) dan jam yang tergambar di dalam jendela (bawah). Selisihnya = latensi
kamera -> goggles -> USB -> decoder -> layar.
"""

import ctypes
import sys
import time

from PIL import Image, ImageGrab

u32 = ctypes.windll.user32

# Titik jam di dalam frame drone, sebagai fraksi lebar/tinggi gambar. Dipakai
# fraksi supaya pembacaan tetap benar walau jendela player diubah ukurannya.
# Kotak jam asli di layar; harus sama dengan -X/-Y latency-clock.ps1 dan
# tidak boleh bertumpuk dengan jendela player, karena player dibawa ke depan.
CLOCK = (1400, 60, 1900, 240)


class RECT(ctypes.Structure):
    _fields_ = [("left", ctypes.c_long), ("top", ctypes.c_long),
                ("right", ctypes.c_long), ("bottom", ctypes.c_long)]


class POINT(ctypes.Structure):
    _fields_ = [("x", ctypes.c_long), ("y", ctypes.c_long)]


def client_box(title):
    """Kotak area gambar (tanpa title bar/border) dalam koordinat layar."""
    h = u32.FindWindowW(None, title)
    if not h:
        sys.exit(f"jendela '{title}' tidak ditemukan - Zerofuss sudah jalan?")
    u32.SetForegroundWindow(h)
    u32.BringWindowToTop(h)
    time.sleep(0.4)
    r = RECT()
    u32.GetClientRect(h, ctypes.byref(r))
    p = POINT(0, 0)
    u32.ClientToScreen(h, ctypes.byref(p))
    return p.x, p.y, r.right, r.bottom


def main():
    x, y, w, h = client_box("Zerofuss")

    img = ImageGrab.grab(all_screens=True)
    vx, vy = u32.GetSystemMetrics(76), u32.GetSystemMetrics(77)
    off = lambda a, b: (a - vx, b - vy)

    # Seluruh jendela ditempel di bawah jam asli. Posisi jam di dalam
    # tampilan drone bergeser tiap kali kamera bergerak, jadi jangan potong
    # per-koordinat - tempel utuh dan baca langsung.
    asli = img.crop(off(CLOCK[0], CLOCK[1]) + off(CLOCK[2], CLOCK[3]))
    asli = asli.resize((w, round(asli.height * w / asli.width)), Image.LANCZOS)
    jendela = img.crop(off(x, y) + off(x + w, y + h))

    out = Image.new("RGB", (w, asli.height + jendela.height))
    out.paste(asli, (0, 0))
    out.paste(jendela, (0, asli.height))
    out.save("banding.png")
    print(f"client {w}x{h} di ({x},{y}) -> banding.png")


if __name__ == "__main__":
    main()
