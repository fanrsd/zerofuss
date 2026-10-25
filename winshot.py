"""Tangkap satu jendela ke PNG, untuk memeriksa tampilan tanpa melihat layar.

    python winshot.py Zerofuss ui.png
    python winshot.py Zerofuss ui.png 1200x730 --key s      # tekan tombol dulu
    python winshot.py Zerofuss ui.png 1200x730 --click 1452,73
    python winshot.py Zerofuss fs.png keep            # jangan ubah jendela
    python winshot.py Zerofuss fs.png keep --move 0.5,0.004   # geser kursor saja
"""

import ctypes
import sys

from PIL import ImageGrab

u32 = ctypes.windll.user32
u32.SetProcessDPIAware()


class RECT(ctypes.Structure):
    _fields_ = [("left", ctypes.c_long), ("top", ctypes.c_long),
                ("right", ctypes.c_long), ("bottom", ctypes.c_long)]


def main():
    title = sys.argv[1] if len(sys.argv) > 1 else "Zerofuss"
    out = sys.argv[2] if len(sys.argv) > 2 else "ui.png"

    h = u32.FindWindowW(None, title)
    if not h:
        sys.exit(f"jendela '{title}' tidak ditemukan")
    # Jendela bisa terletak sebagian di luar layar; geser dulu supaya
    # tangkapannya utuh. Ukuran boleh dipaksa lewat argumen ke-3, mis. 1500x980.
    arg = sys.argv[3] if len(sys.argv) > 3 else "1500x980"
    if arg != "keep":
        w, ht = (int(v) for v in arg.split("x"))
        u32.SetWindowPos(h, 0, 0, 0, w, ht, 0x0004)  # SWP_NOZORDER
    u32.SetForegroundWindow(h)
    ctypes.windll.kernel32.Sleep(600)
    r0 = RECT()
    u32.GetWindowRect(h, ctypes.byref(r0))

    if "--click" in sys.argv:
        # Klik nyata, bukan kirim tombol: dokumen webview belum tentu punya
        # fokus keyboard sebelum ada yang mengkliknya.
        # Nilai < 1 dibaca sebagai fraksi jendela, supaya tidak perlu menebak
        # koordinat saat DPI atau ukuran jendela berubah.
        w0, h0 = r0.right - r0.left, r0.bottom - r0.top
        cx, cy = (float(v) for v in sys.argv[sys.argv.index("--click") + 1].split(","))
        cx = round(cx * w0) if cx < 1 else round(cx)
        cy = round(cy * h0) if cy < 1 else round(cy)
        u32.SetCursorPos(r0.left + cx, r0.top + cy)
        ctypes.windll.kernel32.Sleep(120)
        u32.mouse_event(0x0002, 0, 0, 0, 0)  # LEFTDOWN
        u32.mouse_event(0x0004, 0, 0, 0, 0)  # LEFTUP
        ctypes.windll.kernel32.Sleep(700)

    if "--move" in sys.argv:
        w0, h0 = r0.right - r0.left, r0.bottom - r0.top
        mx, my = (float(v) for v in sys.argv[sys.argv.index("--move") + 1].split(","))
        u32.SetCursorPos(r0.left + (round(mx * w0) if mx < 1 else round(mx)),
                         r0.top + (round(my * h0) if my < 1 else round(my)))
        ctypes.windll.kernel32.Sleep(600)

    if "--scroll" in sys.argv:
        sx, sy, notches = (int(v) for v in sys.argv[sys.argv.index("--scroll") + 1].split(","))
        u32.SetCursorPos(r0.left + sx, r0.top + sy)
        ctypes.windll.kernel32.Sleep(120)
        for _ in range(abs(notches)):
            u32.mouse_event(0x0800, 0, 0, 120 if notches > 0 else -120, 0)  # WHEEL
            ctypes.windll.kernel32.Sleep(40)
        ctypes.windll.kernel32.Sleep(400)

    if "--key" in sys.argv:
        vk = ord(sys.argv[sys.argv.index("--key") + 1].upper())
        u32.keybd_event(vk, 0, 0, 0)
        u32.keybd_event(vk, 0, 2, 0)  # KEYEVENTF_KEYUP
        ctypes.windll.kernel32.Sleep(700)

    r = RECT()
    u32.GetWindowRect(h, ctypes.byref(r))
    # Koordinat virtual screen bisa mulai negatif kalau ada monitor di kiri/atas.
    vx, vy = u32.GetSystemMetrics(76), u32.GetSystemMetrics(77)
    import time
    t = time.time()
    img = ImageGrab.grab(all_screens=True)
    img.crop((r.left - vx, r.top - vy, r.right - vx, r.bottom - vy)).save(out)
    print(f"{out} {r.right - r.left}x{r.bottom - r.top} epoch={t:.4f}")


if __name__ == "__main__":
    main()
