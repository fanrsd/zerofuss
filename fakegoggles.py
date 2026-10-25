"""Emulator DJI Goggles 3 untuk sisi video: bicara protokol yang sama seperti
goggles asli, tapi memutar berkas H.264 dari disk.

Dipakai untuk menguji Zerofuss tanpa perangkat - dan itu bukan kompromi:
protokolnya identik sampai ke tata letak header, jadi kalau penerima jalan di
sini, yang berubah saat perangkat dicolok hanyalah alamat IP.

    python fakegoggles.py --make                         # bangkitkan klip uji
    python fakegoggles.py testdata/mid.h264
    python fakegoggles.py testdata/mid.h264 --loss 2     # buang 2% paket

Sisi penerima harus diarahkan ke 127.0.0.1 (di Zerofuss: Pengaturan > Jalur
goggles), atau jalankan grab.py dengan alamat yang sama.
"""

import argparse
import os
import random
import re
import socket
import subprocess
import sys
import time

CLIP = "testdata/mid.h264"
FFMPEG = os.environ.get("ZEROFUSS_FFMPEG", "ffmpeg")

AUD = b"\x00\x00\x00\x01\x09"  # access unit delimiter = awal frame
MTU_PAYLOAD = 1452  # 1472 - 20 byte header, sama seperti sesi asli


def make_clip(path: str = CLIP, seconds: int = 23) -> str:
    """Bangkitkan klip yang meniru dua sifat menyusahkan dari stream goggles:
    intra-refresh dan TIDAK ADA IDR sama sekali. Nomor frame dibakar ke gambar
    supaya latensi bisa diukur dari tangkapan layar.
    """
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    tmp = path + ".full"
    font = "C\\:/Windows/Fonts/consola.ttf" if os.name == "nt" else "/System/Library/Fonts/Menlo.ttc"
    subprocess.run([FFMPEG, "-hide_banner", "-loglevel", "error", "-y",
                    "-f", "lavfi", "-i", f"testsrc2=size=1920x1080:rate=30:duration={seconds}",
                    "-vf", f"drawtext=fontfile={font}:text='%{{n}}':fontsize=120:fontcolor=white"
                           ":box=1:boxcolor=black@0.85:boxborderw=14:x=60:y=60",
                    "-c:v", "libx264", "-profile:v", "high", "-pix_fmt", "yuv420p", "-b:v", "8M",
                    "-bf", "0", "-aud", "1",
                    "-x264-params", "intra-refresh=1:keyint=infinite",
                    "-f", "h264", tmp], check=True)

    d = open(tmp, "rb").read()
    nals = [(m.start(), m.end(), d[m.end()] & 0x1F) for m in re.finditer(rb"\x00\x00\x01", d)]
    ends = [n[0] for n in nals[1:]] + [len(d)]
    sps = next(d[nals[i][0]:ends[i]] for i in range(len(nals)) if nals[i][2] == 7)
    pps = next(d[nals[i][0]:ends[i]] for i in range(len(nals)) if nals[i][2] == 8)
    auds = [i for i in range(len(nals)) if nals[i][2] == 9]
    # Dipotong di AUD ke-60: IDR di awal terbuang, jadi persis seperti
    # menyambung ke tengah siaran goggles yang sudah berjalan.
    mid = b"\x00\x00\x00\x01" + sps[3:] + b"\x00\x00\x00\x01" + pps[3:] + d[nals[auds[60]][0]:]
    open(path, "wb").write(mid)
    os.remove(tmp)
    print(f"{path}: {len(mid)/1e6:.1f} MB, {len(auds)-60} frame, nol IDR", file=sys.stderr)
    return path


def split_frames(data: bytes) -> list[bytes]:
    """Pecah Annex-B jadi access unit di batas AUD."""
    starts = []
    i = data.find(AUD)
    while i >= 0:
        starts.append(i)
        i = data.find(AUD, i + 1)
    if not starts:
        return [data]
    # Apa pun sebelum AUD pertama (SPS/PPS) disatukan ke frame pertama.
    starts[0] = 0
    return [data[a:b] for a, b in zip(starts, starts[1:] + [len(data)])]


def head(length: int, session: bytes, seq: int, kind: int) -> bytes:
    """Header 8 byte; byte ke-7 tambalan supaya XOR seluruh header = 0."""
    h = bytearray(8)
    h[0:2] = (0x8000 | length).to_bytes(2, "little")
    h[2:4] = session
    h[4:6] = (seq & 0xFFFF).to_bytes(2, "little")
    h[6] = kind
    x = 0
    for b in h:
        x ^= b
    h[7] = x
    return bytes(h)


def video_packet(session: bytes, seq: int, idx: int, frame: int, payload: bytes) -> bytes:
    p = bytearray(head(len(payload) + 20, session, seq, 0x02))
    p += bytes(12)
    p[10:12] = (seq & 0xFFFF).to_bytes(2, "little")
    p[16] = idx & 0xFF  # indeks paket berurutan: dipakai penerima menghitung yang hilang
    p[17] = frame & 0x7F
    return bytes(p) + payload


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file", nargs="?", default=CLIP,
                    help="berkas H.264 Annex-B (idealnya tanpa IDR, seperti goggles)")
    ap.add_argument("--make", action="store_true", help="bangkitkan klip uji lalu keluar")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=9003)
    ap.add_argument("--fps", type=float, default=30.0)
    ap.add_argument("--loss", type=float, default=0.0, help="persen paket dibuang, untuk uji statistik")
    ap.add_argument("--tick", help="catat 'indeks-frame epoch' tiap frame, untuk ukur latensi")
    args = ap.parse_args()

    if args.make:
        make_clip(args.file)
        return
    if not os.path.exists(args.file):
        make_clip(args.file)

    tick = open(args.tick, "w", buffering=1) if args.tick else None
    frames = split_frames(open(args.file, "rb").read())
    print(f"{len(frames)} frame dimuat dari {args.file}", file=sys.stderr)

    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind((args.host, args.port))
    s.settimeout(0.2)
    print(f"menunggu klien di {args.host}:{args.port}", file=sys.stderr)

    peer = None
    session = b"\x5d\xa8"
    seq = idx = 0
    fi = 0
    sent = dropped = 0
    last_ka = 0.0
    next_frame = 0.0
    period = 1.0 / args.fps

    while True:
        # Terima open/subscribe. Goggles asli hanya mengirim selama klien
        # terus-menerus memperbarui ACK; sifat itu ditiru di sini.
        try:
            d, addr = s.recvfrom(2048)
            if len(d) >= 8 and d[6] in (0x00, 0x04):
                if peer != addr:
                    print(f"klien tersambung: {addr}", file=sys.stderr)
                    peer = addr
                    next_frame = time.time()
                last_ka = time.time()
        except socket.timeout:
            pass
        except ConnectionResetError:
            # Klien menutup portnya; Windows melaporkannya di recv berikutnya.
            peer = None

        now = time.time()
        if peer is None:
            continue
        if now - last_ka > 1.0:
            print("klien diam >1 s - berhenti mengirim", file=sys.stderr)
            peer = None
            continue
        if now < next_frame:
            continue
        next_frame += period

        idx_in_clip = fi % len(frames)
        frame = frames[idx_in_clip]
        fi += 1
        if tick:
            tick.write(f"{idx_in_clip} {time.time():.4f}\n")
        for off in range(0, len(frame), MTU_PAYLOAD):
            pkt = video_packet(session, seq, idx, fi, frame[off : off + MTU_PAYLOAD])
            seq += 8  # jam goggles naik 8 per paket, seperti sesi asli
            idx += 1
            if args.loss and random.random() * 100 < args.loss:
                dropped += 1
                continue
            try:
                s.sendto(pkt, peer)
                sent += 1
            except OSError:
                pass

        if fi % (int(args.fps) * 5) == 0:
            print(f"\rframe {fi} / {sent} paket terkirim / {dropped} dibuang", end="", file=sys.stderr)


if __name__ == "__main__":
    main()
