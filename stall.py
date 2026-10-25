"""Tiru stall yang terjadi saat jendela digeser/di-resize: bekukan proses
player sejenak, lalu lepaskan. Dipakai untuk menguji apakah player bisa
menyusul kembali atau tundaannya jadi permanen.

  python stall.py ffplay 600      # bekukan ffplay 600 ms
"""

import ctypes
import sys
import time

k32 = ctypes.windll.kernel32
ntdll = ctypes.windll.ntdll
PROCESS_ALL = 0x1F0FFF


def pids(name):
    import subprocess
    out = subprocess.run(["powershell", "-NoProfile", "-Command",
                          f"(Get-Process {name} -ErrorAction SilentlyContinue).Id"],
                         capture_output=True, text=True).stdout
    return [int(x) for x in out.split()]


def main():
    name = sys.argv[1] if len(sys.argv) > 1 else "ffplay"
    ms = int(sys.argv[2]) if len(sys.argv) > 2 else 600
    found = pids(name)
    if not found:
        sys.exit(f"proses {name} tidak ditemukan")
    for pid in found:
        h = k32.OpenProcess(PROCESS_ALL, False, pid)
        ntdll.NtSuspendProcess(h)
        print(f"{name} pid {pid} dibekukan {ms} ms")
        time.sleep(ms / 1000)
        ntdll.NtResumeProcess(h)
        k32.CloseHandle(h)
        print("dilepas")


if __name__ == "__main__":
    main()
