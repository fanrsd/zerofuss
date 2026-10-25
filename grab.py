"""Zerofuss receiver: live view DJI Goggles 3 ke PC lewat kabel USB, tanpa HP.

  python grab.py --secs 10 -o gc.h264
  python grab.py | ffplay -i -            # tonton langsung

Protokol (hasil bongkar sendiri, tanpa dokumentasi DJI):
  transport : UDP 192.168.60.1:12346 <-> 192.168.60.2:9003 (RNDIS kabel USB)
  header    : 8 byte, sama untuk semua paket kedua arah
              [0:2] = 0x8000 | panjang   [2:4] = session id (dipilih klien)
              [4:6] = seq/timestamp      [6]   = tipe
              [7]   = checksum; XOR 8 byte header selalu 0
  tipe      : 00 = open (48 B, dari klien)   01 = ack goggles (9 B)
              02 = video                     04 = subscribe/keepalive (34 B)
  urutan    : klien kirim open -> goggles balas ack -> video mengalir;
              klien kirim subscribe tiap ~31 ms, timestamp naik 6496/detik
  video     : payload setelah 20 byte awal; H.264 Annex-B High 1920x1080.
              Stream tanpa IDR (intra-refresh), jadi decoder perlu beberapa
              detik sebelum gambar penuh terbentuk.
"""

import argparse
import os
import socket
import queue
import struct
import sys
import threading
import time

GOGGLES = ("192.168.60.2", 9003)
LOCAL = "192.168.60.1"
PORT = 12346
HDR = 20
SPS = b"\x00\x00\x00\x01\x67"  # penanda awal parameter set, dipakai untuk sinkron
AUD = b"\x00\x00\x00\x01\x09"  # access unit delimiter = awal frame
RATE = 6496.0  # unit timestamp per detik, terukur dari sesi asli

# ekor paket open: blok konfigurasi tetap (mtu 1472 dll) hasil rekaman sesi asli
OPEN_TAIL = bytes.fromhex(
    "64006400c0051400000a0064006400c00514000064001400"
    "640" "0c00514000064000101040a02"
)


def head(length, session, seq, kind):
    """8 byte header; byte 7 dipilih agar XOR header = 0."""
    h = bytearray(8)
    h[0:2] = (0x8000 | length).to_bytes(2, "little")
    h[2:4] = session
    h[4:6] = seq.to_bytes(2, "little")
    h[6] = kind
    x = 0
    for b in h:
        x ^= b
    h[7] = x
    return bytes(h)


def build_open(ts, session):
    return head(48, session, 0, 0x00) + ts.to_bytes(2, "little") + OPEN_TAIL


def build_subscribe(ts, session):
    return head(34, session, 0, 0x04) + ts.to_bytes(2, "little") * 2 + bytes(22)


def grab_socket(timeout=8.0):
    """Port 12346 bisa masih dipegang app lain yang baru ditutup - tunggu lepas."""
    end = time.time() + timeout
    while time.time() < end:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            s.bind((LOCAL, PORT))
            return s
        except OSError:
            s.close()
            time.sleep(0.05)
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-o", "--out", help="tulis H.264 ke file (default: stdout)")
    ap.add_argument("--secs", type=float, default=0, help="berhenti setelah N detik (0=terus)")
    ap.add_argument("--session", help="session id hex 2 byte (default: acak)")
    ap.add_argument("--raw", action="store_true", help="simpan paket utuh (u16 panjang + paket) untuk analisis")
    ap.add_argument("--rate", type=float, default=RATE)
    ap.add_argument("--no-open", action="store_true", help="lewati handshake (ambil alih sesi berjalan)")
    ap.add_argument("--stamps", help="tulis 'frame waktu-epoch' tiap frame, untuk ukur latensi")
    args = ap.parse_args()

    session = bytes.fromhex(args.session) if args.session else os.urandom(2)

    s = grab_socket()
    if s is None:
        sys.exit(f"gagal mengikat port {PORT} - ada app lain yang memakainya?")
    # Buffer terima dijaga kecil: kalau player tersendat, buffer besar hanya
    # menumpuk video basi dan latensi tidak pernah pulih. 256 KB = ~230 ms slack.
    s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1 << 18)
    print(f"session {session.hex()}, port {PORT} siap", file=sys.stderr)

    # ACK: subscribe membawa seq paket terakhir yang kita terima; goggles hanya
    # mengirim sejauh jendela di depan nilai itu. Sebelum paket pertama datang
    # belum ada yang bisa di-ACK, jadi dipancing dengan estimasi jam goggles
    # (jam mereka mulai dari ~0 tiap sesi dan naik RATE/detik).
    ack = [None]
    t_start = time.time()

    def ack_now():
        if ack[0] is not None:
            return ack[0]
        return int((time.time() - t_start) * args.rate) & 0xFFFF

    if not args.no_open:
        for _ in range(3):
            s.sendto(build_open(ack_now(), session), GOGGLES)
            time.sleep(0.02)

    stop = threading.Event()

    def keepalive():
        while not stop.is_set():
            try:
                s.sendto(build_subscribe(ack_now(), session), GOGGLES)
            except OSError:
                pass
            stop.wait(0.03)

    threading.Thread(target=keepalive, daemon=True).start()

    # Penulisan dipisah ke thread sendiri. Kalau player tersendat, pipe penuh
    # dan write memblokir; kalau itu terjadi di loop utama, ACK ikut berhenti,
    # goggles berhenti mengirim, dan stream beku permanen. Jadi loop jaringan
    # tidak boleh pernah memblokir: antrean penuh -> frame dibuang.
    sink = open(args.out, "wb") if args.out else sys.stdout.buffer
    q = queue.Queue(maxsize=32)  # ~46 KB = ~40 ms; batas atas latensi antrean

    def writer():
        while True:
            chunk = q.get()
            if chunk is None:
                break
            try:
                sink.write(chunk)
                sink.flush()  # tanpa ini stdout menahan 8 KB (~7 ms) per buffer
            except OSError:
                break

    wr = threading.Thread(target=writer, daemon=True)
    wr.start()

    stamps = open(args.stamps, "w") if args.stamps else None
    au = 0
    total = pkts = lost = dropped = 0
    prev_idx = None
    synced = args.raw  # mulai menulis di SPS pertama agar player bisa sinkron
    s.settimeout(1.5)
    t0 = last_log = time.time()
    try:
        while True:
            if args.secs and time.time() - t0 >= args.secs:
                break
            try:
                d, _ = s.recvfrom(65536)
            except socket.timeout:
                continue
            except ConnectionResetError:
                # Windows: keepalive ke port goggles yang sedang tidak mendengar
                # memicu ICMP port-unreachable, dan recv berikutnya melempar ini.
                # Terjadi tiap kali Share Liveview dimatikan - bukan alasan keluar.
                continue
            ack[0] = int.from_bytes(d[4:6], "little")  # seq yang di-ACK berikutnya
            if len(d) <= HDR or d[6] != 0x02:  # bukan paket video
                continue
            idx = d[16]  # indeks paket berurutan; dipakai mendeteksi paket hilang
            if prev_idx is not None:
                gap = (idx - prev_idx) & 0xFF
                if gap > 1:
                    lost += gap - 1
            prev_idx = idx
            if not synced:
                if SPS not in d[HDR:]:
                    continue
                synced = True
            if stamps is not None and AUD in d[HDR:]:
                # AUD menandai awal frame: catat kapan frame itu tiba di PC.
                au += 1
                stamps.write(f"{au} {time.time():.6f}\n")
            try:
                q.put_nowait(struct.pack("<H", len(d)) + d if args.raw else d[HDR:])
            except queue.Full:
                dropped += 1
            total += len(d) - HDR
            pkts += 1
            t = time.time()
            if t - last_log >= 1.0:
                print(f"\r{total/1024:.0f} KiB / {pkts} pkt / {total/1024/max(t-t0,0.001):.0f} KiB/s",
                      end="", file=sys.stderr)
                last_log = t
    except KeyboardInterrupt:
        pass
    finally:
        stop.set()
        if stamps:
            stamps.close()
        q.put(None)
        wr.join(timeout=2)
        if args.out:
            sink.close()
        print(f"\nselesai: {total} B / {pkts} pkt / hilang {lost} "
              f"({100*lost/max(pkts+lost,1):.1f}%) / dibuang {dropped}", file=sys.stderr)
        if pkts == 0:
            print("tidak ada data - Share Liveview di goggles nyala? kabel USB data?", file=sys.stderr)


if __name__ == "__main__":
    main()
