//! Pipa video Zerofuss: goggles -> UDP -> ffmpeg (decode) -> frame NV12 -> HTTP lokal.
//!
//! Protokol goggles hasil bongkar sendiri lewat pengamatan trafik; DJI tidak
//! mendokumentasikannya. Ringkasnya:
//!
//!   PC 192.168.60.1:12346  <->  goggles 192.168.60.2:9003   (UDP, lewat RNDIS USB)
//!   header 8 byte; byte ke-7 dipilih supaya XOR seluruh header = 0
//!   tipe 0x00 = open, 0x02 = video, 0x04 = subscribe/keepalive
//!   paket video = 20 byte header + payload H.264 Annex-B
//!
//! Dua sifat stream yang menentukan seluruh desain di bawah:
//!
//!   1. Praktis tanpa keyframe, jadi decoder wajib dipaksa mengeluarkan gambar
//!      sebelum keyframe: `-flags2 +showall`. Terukur di Goggles 3: satu IDR
//!      dikirim saat sesi liveview PERTAMA dimulai, dan tidak ada lagi
//!      sesudahnya - tiga tangkapan berikutnya nol IDR, hanya SPS/PPS berulang
//!      (~3/detik) plus slice intra-refresh. Bahkan IDR yang satu itu tiba
//!      rusak (`bytestream -7`), jadi ffmpeg membuangnya: tanpa `+showall`
//!      keluarannya nol frame, terukur. Gambar penuh terbentuk ~4 detik setelah
//!      mulai; itu satu putaran intra-refresh, sekali saja.
//!   2. Goggles hanya mengirim sejauh jendela di depan seq yang kita ACK.
//!      ACK menumpang di paket subscribe. Kalau loop jaringan berhenti walau
//!      sesaat, ACK berhenti, goggles berhenti mengirim, dan stream beku
//!      permanen. Karena itu TIDAK ADA jalur di loop ini yang boleh memblokir:
//!      antrean penuh -> paket dibuang, bukan ditunggu.
//!
//!      Jendela itu KENDALI ALIRAN, bukan ACK reliabilitas - terukur, jangan
//!      dipakai untuk meminta kirim ulang. Menahan nilainya di seq lama supaya
//!      goggles mengirim ulang lubangnya: 11,5 Mbps -> 0,5 -> 0, nol paket
//!      terulang, dan stream TIDAK pulih walau jendelanya dilepas kembali.
//!
//!      Konsekuensinya bagus: karena goggles menahan diri sampai di-ACK,
//!      penerima yang tersendat menyebabkan JEDA, bukan kehilangan. Membekukan
//!      proses penerima 3x0,8 detik pada 12,8 Mbps membuat `Loss` tetap 0,0%.
//!      Itu sebabnya jalur kabel ini praktis tidak pernah kehilangan paket, dan
//!      sebabnya fitur "Retransmission" di aplikasi lain tidak menyisakan jejak
//!      apa pun di kabel: paket klien SquirrelReceiver dengan RTX ON dan RTX OFF
//!      sama-sama tipe 0x04, panjang 34, dua medan jendela identik, ekor nol -
//!      satu bentuk unik di kedua mode. Tidak ada yang bisa ditiru dari sana.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use parking_lot::{Condvar, Mutex};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

const HDR: usize = 20; // header paket video
const KIND_OPEN: u8 = 0x00;
const KIND_VIDEO: u8 = 0x02;
const KIND_SUB: u8 = 0x04;
// Penanda NAL yang dicari. Prefiksnya sengaja 3 byte (`00 00 01`), bukan 4:
// bentuk itu cocok untuk KEDUA panjang start code Annex-B, karena start code 4
// byte `00 00 00 01` memuatnya di offset 1. Ini bukan kelonggaran - jalur USB
// V1/V2 memang memakai start code 3 byte (voc-poc mencatat frame dimulai
// `00 00 01 09 10`, dan parser fpvout-go menangani dua-duanya), jadi needle 4
// byte akan MELEWATKAN seluruh SPS di sana: rekaman jadi tidak bisa dibuka
// pemutar, dan hitungan frame mati. Tidak ada risiko positif palsu: emulation
// prevention H.264 (`00 00 03`) menjamin `00 00 01` tidak pernah muncul di
// data terenkode.
const SPS: [u8; 4] = [0, 0, 1, 0x67]; // awal parameter set: titik sinkron
const RATE: f64 = 6496.0; // unit jam goggles per detik, terukur dari sesi asli

/// Ekor paket open: blok konfigurasi tetap (mtu 1472 dll) hasil rekaman sesi asli.
const OPEN_TAIL: [u8; 38] = [
    0x64, 0x00, 0x64, 0x00, 0xc0, 0x05, 0x14, 0x00, 0x00, 0x0a, 0x00, 0x64, 0x00, 0x64, 0x00, 0xc0,
    0x05, 0x14, 0x00, 0x00, 0x64, 0x00, 0x14, 0x00, 0x64, 0x00, 0xc0, 0x05, 0x14, 0x00, 0x00, 0x64,
    0x00, 0x01, 0x01, 0x04, 0x0a, 0x02,
];

/// Header 8 byte. Byte ke-7 adalah tambalan supaya XOR seluruh header nol -
/// aturan itu berlaku di 882/882 paket subscribe milik klien resmi.
fn head(len: u16, session: [u8; 2], seq: u16, kind: u8) -> [u8; 8] {
    let mut h = [0u8; 8];
    h[0..2].copy_from_slice(&(0x8000 | len).to_le_bytes());
    h[2..4].copy_from_slice(&session);
    h[4..6].copy_from_slice(&seq.to_le_bytes());
    h[6] = kind;
    h[7] = h.iter().fold(0u8, |a, b| a ^ b);
    h
}

fn build_open(ts: u16, session: [u8; 2]) -> Vec<u8> {
    let mut p = Vec::with_capacity(48);
    p.extend_from_slice(&head(48, session, 0, KIND_OPEN));
    p.extend_from_slice(&ts.to_le_bytes());
    p.extend_from_slice(&OPEN_TAIL);
    p
}

fn build_subscribe(ts: u16, session: [u8; 2]) -> Vec<u8> {
    let mut p = Vec::with_capacity(34);
    p.extend_from_slice(&head(34, session, 0, KIND_SUB));
    p.extend_from_slice(&ts.to_le_bytes());
    p.extend_from_slice(&ts.to_le_bytes());
    p.resize(34, 0);
    p
}

// ---------------------------------------------------------------- penghitung

#[derive(Default)]
pub struct Counters {
    pub bytes: AtomicU64,
    pub pkts: AtomicU64,
    pub lost: AtomicU64,
    pub dropped: AtomicU64,
    pub decoded: AtomicU64,
    pub width: AtomicU64,
    pub height: AtomicU64,
    pub last_rx_ms: AtomicU64,
    pub rec_bytes: AtomicU64,
    pub restarts: AtomicU64,
    /// Frame yang decoder selesaikan dengan galat. Naik lalu berhenti =
    /// transien (satu putaran intra-refresh). Terus naik = ada yang salah.
    pub corrupt: AtomicU64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ------------------------------------------------------------------ slot frame

pub struct Frame {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

/// Menampung satu frame terakhir saja. Kalau konsumen tertinggal, frame lama
/// dibuang - buffer video basi adalah persis penyebab latensi yang tidak pernah
/// pulih, jadi di sini tidak ada antrean sama sekali.
#[derive(Default)]
pub struct FrameSlot {
    slot: Mutex<Option<Frame>>,
    pool: Mutex<Vec<Vec<u8>>>, // buffer daur pakai; 3 MB/frame terlalu mahal untuk dialokasi ulang
    cv: Condvar,
}

impl FrameSlot {
    fn put(&self, w: u32, h: u32, data: Vec<u8>) {
        let old = self.slot.lock().replace(Frame { w, h, data });
        if let Some(f) = old {
            self.recycle(f.data); // frame yang tak terpakai: kembalikan buffernya
        }
        self.cv.notify_all();
    }

    pub fn take(&self, timeout: Duration) -> Option<Frame> {
        let mut g = self.slot.lock();
        if g.is_none() {
            self.cv.wait_for(&mut g, timeout);
        }
        g.take()
    }

    pub fn recycle(&self, buf: Vec<u8>) {
        let mut p = self.pool.lock();
        if p.len() < 4 {
            p.push(buf);
        }
    }

    fn buffer(&self, len: usize) -> Vec<u8> {
        match self.pool.lock().pop() {
            Some(b) if b.len() == len => b,
            Some(mut b) => {
                b.resize(len, 0);
                b
            }
            None => vec![0u8; len],
        }
    }
}

// -------------------------------------------------------------------- pipa

pub struct Pipeline {
    pub counters: Arc<Counters>,
    /// SPS+PPS terakhir yang terlihat. Rekaman dimulai dengan ini; tanpa itu
    /// berkas tidak bisa dibuka pemutar sampai SPS berikutnya lewat - terukur
    /// bisa 14 MB kemudian, jauh di luar jangkauan probe bawaan pemutar.
    params: Arc<Mutex<Vec<u8>>>,
    pub port: u16,
    /// Butuh alamat lokal di adapter OS? Dipakai `stats` untuk menilai apakah
    /// hilangnya alamat itu berarti kabel tercabut.
    ///
    /// HANYA benar di jalur RNDIS lewat adapter OS. Di jalur USB bulk maupun
    /// RNDIS user space alamat itu TIDAK pernah ada - kalau keduanya ikut
    /// diperiksa, pipa yang sehat dipanen sedetik setelah lahir, karena bind
    /// ke alamat yang memang tidak dipasang siapa pun selalu gagal.
    pub perlu_adapter: bool,
    pub log: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    rec: Arc<Mutex<Option<std::fs::File>>>,
}

/// Dari mana byte video datang. Dua keluarga goggles memakai jalur yang sama
/// sekali berbeda, tapi semua yang di hilir - decoder, rekaman, penghitung,
/// server frame - identik dan dipakai bersama.
#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    /// Goggles 3 / N3 lewat adapter RNDIS yang disediakan OS (Windows, Linux):
    /// UDP biasa di atas alamat statis 192.168.60.1.
    Rndis { goggles: String, goggles_port: u16, local: String },
    /// Goggles 3 / N3 lewat host RNDIS di user space (macOS): interface USB
    /// diklaim sendiri, dan ARP/IPv4/UDP dibentuk di dalam proses. Tidak ada
    /// adapter, alamat statis, maupun hak admin yang terlibat.
    RndisUsb { pid: u16, goggles: String, goggles_port: u16, local: String },
    /// FPV Goggles V1 / V2: USB bulk + magic "RMVT".
    Bulk { pid: u16 },
}

pub struct Config {
    pub source: Source,
    pub ffmpeg: PathBuf,
}

/// Sumber yang sudah dibuka dan siap dibaca. Dipisah dari `Source` supaya
/// seluruh kegagalan pembukaan terjadi di satu tempat, sebelum utas apa pun
/// ada - bukan tersebar di dalamnya.
enum Transport {
    Rndis { sock: UdpSocket, goggles: String },
    RndisUsb { tautan: crate::rndis::Tautan, tumpukan: crate::net::Tumpukan, port: u16 },
    Bulk(crate::usb::Feed),
}

impl Transport {
    fn open(src: &Source) -> Result<Transport, String> {
        match src {
            Source::Rndis { goggles, goggles_port, local } => {
                // Port 0: goggles membalas ke port pengirim, dan nomor port
                // TIDAK ada di dalam paket open maupun subscribe (lihat
                // build_open/build_subscribe di bawah). Jadi memilih port tetap
                // tidak memberi apa pun, sementara ia menciptakan kegagalan
                // nyata: instansi kedua aplikasi - atau program lain yang
                // kebetulan memakainya - membuat pipa gagal bind dan layar
                // tinggal hitam.
                let sock = bind_socket(local).map_err(|e| e.to_string())?;
                // Buffer terima dijaga kecil dengan sengaja: kalau hilir
                // tersendat, buffer besar hanya menumpuk video basi dan latensi
                // tak pernah pulih.
                set_rcvbuf(&sock, 1 << 18);
                Ok(Transport::Rndis { sock, goggles: format!("{goggles}:{goggles_port}") })
            }
            Source::RndisUsb { pid, goggles, goggles_port, local } => {
                let ip_peer = goggles
                    .parse()
                    .map_err(|_| format!("alamat goggles tidak sah: {goggles}"))?;
                let ip_kita = local
                    .parse()
                    .map_err(|_| format!("alamat lokal tidak sah: {local}"))?;
                let mut tautan = crate::rndis::Tautan::buka(*pid)?;
                // Port sumber kita: bebas, karena nomornya tidak pernah ikut di
                // dalam paket DJI - goggles membalas ke port pengirim. Diambil
                // dari jam supaya tiap sesi berbeda; tidak ada tabel soket OS
                // yang bisa bentrok di sini, jadi ini murni agar aliran lama
                // tidak pernah tertukar dengan yang baru.
                let port = 40000 + (now_ms() % 20000) as u16;
                let mut tumpukan = crate::net::Tumpukan::baru(ip_kita, ip_peer, port);
                // MAC dari RNDIS cuma dugaan awal; ARP yang menentukan.
                tumpukan.duga_mac_peer(tautan.mac_perangkat);
                let _ = tautan.kirim_frame(&tumpukan.arp_minta());
                Ok(Transport::RndisUsb { tautan, tumpukan, port: *goggles_port })
            }
            Source::Bulk { pid } => crate::usb::Feed::open(*pid).map(Transport::Bulk),
        }
    }
}

impl Pipeline {
    /// Bangun pipa dan mulai.
    ///
    /// Galatnya `String`, bukan `io::Error`: pesan jalur USB harus sampai ke
    /// pengguna apa adanya (mis. "driver belum dipasang, jalankan Zadig"),
    /// dan membungkusnya jadi io::Error hanya menghapus keterangan itu.
    pub fn start(cfg: Config) -> Result<Self, String> {
        let counters = Arc::new(Counters::default());
        let frames = Arc::new(FrameSlot::default());
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let rec: Arc<Mutex<Option<std::fs::File>>> = Arc::new(Mutex::new(None));
        let params = Arc::new(Mutex::new(Vec::new()));

        // Transport disiapkan SEBELUM utas apa pun dijalankan, supaya kegagalan
        // membuka goggles jadi galat yang bisa dibaca pengguna - bukan utas
        // yang mati diam-diam di belakang layar kosong.
        let siap = Transport::open(&cfg.source)?;

        // ffmpeg diawasi, bukan dijalankan sekali: feed goggles akan berhenti
        // dan mulai lagi (user mematikan Share Liveview, goggles reboot, mode
        // 16:9 <-> 4:3 mengubah resolusi). Tiap kali decoder mati, generasi
        // baru dijalankan dan loop sumber diminta menyinkron ulang di SPS.
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let generation = Arc::new(AtomicU64::new(0));
        let geom = Arc::new(AtomicBool::new(false));

        // Antrean ke ffmpeg. Penuh -> paket dibuang supaya loop sumber (dan
        // karenanya ACK di jalur RNDIS) tidak pernah ikut berhenti. Membuang
        // adalah backpressure yang disengaja; yang TIDAK disengaja adalah
        // membuang di keadaan normal.
        //
        // Kapasitas 32 dulu berarti ~45 KB ~= 36 ms stream pada 10 Mbps, dan
        // itu lebih pendek daripada jeda yang PASTI terjadi: tiap kali decoder
        // disegarkan, ffmpeg dibunuh lalu dijalankan ulang, dan selama itu tidak
        // ada yang menguras antrean. Terukur di hardware: 3.341 dari 124.194
        // paket dibuang (2,7%) padahal `Hilang` nol - jaringannya sempurna,
        // kitanya yang membuang. Karena stream ini praktis tanpa keyframe,
        // lubangnya tidak pernah pulih tuntas: `Frame korup` terus bertambah.
        //
        // 512 payload ~= 700 KB ~= 0,5 detik: cukup menelan satu restart
        // ffmpeg, dan sengaja TIDAK lebih besar. Antrean dalam menyembunyikan
        // decoder yang benar-benar tertinggal sebagai latensi, dan di aplikasi
        // FPV latensi lebih mahal daripada satu frame yang hilang.
        let (tx, rx) = sync_channel::<Vec<u8>>(512);

        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();

        spawn_named("gc-decoder", {
            let (counters, frames, log, stop) =
                (counters.clone(), frames.clone(), log.clone(), stop.clone());
            let (rec, child, generation) = (rec.clone(), child.clone(), generation.clone());
            let exe = cfg.ffmpeg.clone();
            let geom = geom.clone();
            move || decoder(exe, rx, rec, counters, frames, log, stop, child, generation, geom)
        });
        spawn_named("gc-http", {
            let frames = frames.clone();
            let stop = stop.clone();
            move || serve_frames(listener, frames, stop)
        });
        spawn_named("gc-src", {
            let counters = counters.clone();
            let stop = stop.clone();
            let generation = generation.clone();
            let params = params.clone();
            let log = log.clone();
            move || match siap {
                Transport::Rndis { sock, goggles } => {
                    recv_loop(sock, goggles, tx, counters, stop, generation, params, geom)
                }
                Transport::RndisUsb { tautan, tumpukan, port } => rndis_loop(
                    tautan, tumpukan, port, tx, counters, stop, generation, params, log, geom,
                ),
                Transport::Bulk(bulk) => {
                    usb_loop(bulk, tx, counters, stop, generation, params, log, geom)
                }
            }
        });

        Ok(Pipeline {
            counters,
            params,
            port,
            perlu_adapter: matches!(cfg.source, Source::Rndis { .. }),
            log,
            stop,
            child,
            rec,
        })
    }

    pub fn record_start(&self, path: &std::path::Path) -> std::io::Result<()> {
        // Tanpa SPS/PPS di depan, berkasnya tidak bisa dibuka pemutar mana pun
        // - dan probe bawaan pemutar tidak akan menemukan SPS berikutnya yang
        // bisa berjarak megabyte. Ditolak sekarang, bukan sesudah terbang.
        let head = self.params.lock().clone();
        if head.is_empty() {
            return Err(std::io::Error::other(
                "parameter stream belum terlihat - tunggu gambar muncul dulu",
            ));
        }
        let mut f = std::fs::File::create(path)?;
        f.write_all(&head)?;
        self.counters.rec_bytes.store(head.len() as u64, Relaxed);
        *self.rec.lock() = Some(f);
        Ok(())
    }

    pub fn record_stop(&self) {
        *self.rec.lock() = None;
    }

    pub fn recording(&self) -> bool {
        self.rec.lock().is_some()
    }

    /// Sudah tidak ada utas yang bisa memulihkan pipa ini. Dipanggil `stats`
    /// untuk memanennya, supaya UI tidak menampilkan "Tersambung, menunggu
    /// gambar" atas sesuatu yang mati permanen.
    pub fn mati(&self) -> bool {
        self.stop.load(Relaxed)
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        // `serve_frames` menggantung di `accept()` dan baru memeriksa `stop`
        // sesudah ada koneksi masuk. Satu sambungan buangan membangunkannya,
        // lalu listener dan FrameSlot-nya ikut ter-drop. Tanpa ini tiap siklus
        // Sambungkan/Putuskan meninggalkan satu utas dan ~11 MB buffer.
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(mut c) = self.child.lock().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn spawn_named<F: FnOnce() + Send + 'static>(name: &str, f: F) {
    let _ = std::thread::Builder::new().name(name.into()).spawn(f);
}

// ------------------------------------------------------------------ jaringan

/// Socket penerima di alamat lokal adapter goggles. Selalu port ephemeral:
/// nomor port kita tidak pernah dipilih sendiri, jadi bentrok port mustahil dan
/// tidak ada yang perlu ditunggu di sini.
fn bind_socket(local: &str) -> std::io::Result<UdpSocket> {
    let sock = UdpSocket::bind((local, 0)).map_err(|e| {
        // Satu-satunya kegagalan yang benar-benar mungkin, dan ia butuh
        // tindakan pengguna - bukan waktu.
        if e.kind() == ErrorKind::AddrNotAvailable {
            std::io::Error::new(
                ErrorKind::AddrNotAvailable,
                format!("alamat {local} belum terpasang di adapter goggles"),
            )
        } else {
            e
        }
    })?;
    sock.set_read_timeout(Some(Duration::from_millis(500)))?;
    Ok(sock)
}

#[cfg(windows)]
fn set_rcvbuf(sock: &UdpSocket, bytes: u32) {
    use std::os::windows::io::AsRawSocket;
    unsafe {
        let v = bytes as i32;
        setsockopt_raw(sock.as_raw_socket() as usize, &v);
    }
}

#[cfg(windows)]
unsafe fn setsockopt_raw(s: usize, v: &i32) {
    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn setsockopt(s: usize, level: i32, optname: i32, optval: *const i8, optlen: i32) -> i32;
    }
    const SOL_SOCKET: i32 = 0xffff;
    const SO_RCVBUF: i32 = 0x1002;
    setsockopt(s, SOL_SOCKET, SO_RCVBUF, v as *const i32 as *const i8, 4);
}

#[cfg(not(windows))]
fn set_rcvbuf(sock: &UdpSocket, bytes: u32) {
    use std::os::fd::AsRawFd;
    unsafe {
        unsafe extern "C" {
            fn setsockopt(s: i32, level: i32, name: i32, val: *const i8, len: u32) -> i32;
        }
        // Angka-angka ini BSD vs Linux, bukan detail kosmetik: di macOS
        // SOL_SOCKET adalah 0xffff (warisan BSD, sama seperti Winsock), bukan 1.
        // Dengan nilai Linux, `setsockopt` di Mac gagal EINVAL - dan karena
        // hasilnya sengaja tidak diperiksa, buffer terima tinggal di ukuran
        // baku tanpa satu pun tanda di log.
        #[cfg(target_os = "macos")]
        const SOL_SOCKET: i32 = 0xffff;
        #[cfg(not(target_os = "macos"))]
        const SOL_SOCKET: i32 = 1;
        #[cfg(target_os = "macos")]
        const SO_RCVBUF: i32 = 0x1002;
        #[cfg(not(target_os = "macos"))]
        const SO_RCVBUF: i32 = 8;
        let v = bytes as i32;
        setsockopt(sock.as_raw_fd(), SOL_SOCKET, SO_RCVBUF, &v as *const i32 as *const i8, 4);
    }
}

/// Penyaring bersama kedua transport. Semua yang ada di bawah "dari mana byte
/// datang" identik: petik parameter set, hitung, lalu suapi decoder tanpa
/// pernah memblokir. Ia dipisah supaya jalur RNDIS dan jalur USB bulk tidak
/// punya dua salinan aturan yang harus dijaga sinkron.
struct Feeder {
    tx: SyncSender<Vec<u8>>,
    counters: Arc<Counters>,
    params: Arc<Mutex<Vec<u8>>>,
    generation: Arc<AtomicU64>,
    my_gen: u64,
    /// Tunggu SPS sebelum menyuapi decoder. Wajib di jalur RNDIS: di sana kita
    /// menyambar stream yang sudah berjalan, jadi paket pertama hampir pasti
    /// tengah frame. Di jalur USB justru dimatikan - lihat `usb_loop`.
    gate: bool,
    synced: bool,
    /// Diangkat saat parameter set berganti, dibaca oleh pengawas decoder.
    geom: Arc<AtomicBool>,
}

impl Feeder {
    fn new(
        tx: SyncSender<Vec<u8>>,
        counters: Arc<Counters>,
        params: Arc<Mutex<Vec<u8>>>,
        generation: Arc<AtomicU64>,
        gate: bool,
        geom: Arc<AtomicBool>,
    ) -> Self {
        let my_gen = generation.load(Relaxed);
        Feeder { tx, counters, params, generation, my_gen, gate, synced: !gate, geom }
    }

    /// `false` = decoder hilang, loop pemanggil harus berhenti.
    fn push(&mut self, payload: &[u8]) -> bool {
        // Decoder dijalankan ulang -> mulai lagi dari SPS, bukan dari tengah frame.
        let gen = self.generation.load(Relaxed);
        if gen != self.my_gen {
            self.my_gen = gen;
            self.synced = !self.gate;
        }

        let has_sps = contains(payload, &SPS);
        if has_sps {
            if let Some(p) = extract_params(payload) {
                let mut g = self.params.lock();
                // Parameter set berganti = geometri stream berganti. Itu terjadi
                // saat goggles beralih 16:9 <-> 4:3, dan saat Camera View
                // Recording dimatikan sehingga sumbernya jadi 4:3 native
                // (terukur: 1920x1080 -> 1440x1080).
                //
                // Kenapa ini harus dipaksa jadi restart: ffmpeg TIDAK mati
                // sendiri saat resolusi berubah. Ia terus berjalan, sementara
                // `read_frames` sudah mengunci panjang frame dari ukuran LAMA -
                // jadi tiap bacaan melewati batas frame, gambarnya tampak
                // melebar, dan log-nya dipenuhi "corrupt decoded frame".
                // Ukuran baru juga tidak akan pernah terbaca, karena
                // `read_stderr` hanya mengisinya saat masih nol.
                //
                // Perbandingan byte aman dari restart berulang: terukur di dua
                // tangkapan mentah dari goggles sungguhan, 160 NAL SPS
                // seluruhnya BYTE-IDENTIK (satu varian). Stream sehat memang
                // mengulang SPS yang sama, jadi hanya perubahan sebenarnya yang
                // lolos ke sini.
                if !g.is_empty() && *g != p {
                    self.geom.store(true, Relaxed);
                }
                *g = p;
            }
        }
        if !self.synced {
            if !has_sps {
                return true;
            }
            self.synced = true;
        }
        self.counters.bytes.fetch_add(payload.len() as u64, Relaxed);
        self.counters.pkts.fetch_add(1, Relaxed);
        self.counters.last_rx_ms.store(now_ms(), Relaxed);

        match self.tx.try_send(payload.to_vec()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.counters.dropped.fetch_add(1, Relaxed);
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

/// Aturan bersama kedua jalur Goggles 3, dipisah supaya tidak ada dua salinan
/// yang harus dijaga sinkron. Keduanya menentukan apakah stream tetap hidup:
///
/// * `ack_dari` - jendela HANYA diambil dari paket video. Terukur: paket `ack`
///   goggles selalu ber-seq 0000, jadi mengambilnya dari paket apa pun membuat
///   kita melaporkan "saya di 0" berulang, goggles mencekik kiriman, dan
///   stream merosot lalu berhenti di ~180 detik.
/// * `catat_hilang` - indeks paket 8-bit yang berputar; selisih lebih dari satu
///   berarti ada yang tidak sampai.
fn paket_video(d: &[u8]) -> bool {
    d.len() > HDR && d[6] == KIND_VIDEO
}

fn ack_dari(d: &[u8]) -> u64 {
    u16::from_le_bytes([d[4], d[5]]) as u64
}

fn catat_hilang(prev: &mut Option<u8>, d: &[u8], counters: &Counters) {
    let idx = d[16];
    if let Some(p) = prev.replace(idx) {
        let gap = idx.wrapping_sub(p);
        if gap > 1 {
            counters.lost.fetch_add(gap as u64 - 1, Relaxed);
        }
    }
}

/// Utas baca jalur RNDIS user space (macOS).
///
/// Bentuknya berbeda dari `recv_loop` di satu hal yang menentukan: di sana ACK
/// dikirim utas terpisah lewat soket kedua, di sini TIDAK BISA - satu pasang
/// endpoint USB tidak boleh ditulis dari dua utas sekaligus. Jadi ACK dan
/// pembacaan berbagi satu utas, dan itu aman justru karena bacaannya
/// bertenggat: `terima` menunggu paling lama 5 ms, lalu giliran ACK diperiksa.
/// Yang harus dijaga adalah jarak antar-ACK tetap ~30 ms; kalau ia melar,
/// goggles menahan kiriman dan stream beku permanen.
#[allow(clippy::too_many_arguments)]
fn rndis_loop(
    mut tautan: crate::rndis::Tautan,
    mut tumpukan: crate::net::Tumpukan,
    port_goggles: u16,
    tx: SyncSender<Vec<u8>>,
    counters: Arc<Counters>,
    stop: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    params: Arc<Mutex<Vec<u8>>>,
    log: Arc<Mutex<Vec<String>>>,
    geom: Arc<AtomicBool>,
) {
    let session = {
        let n = now_ms();
        [(n >> 3) as u8, (n >> 11) as u8]
    };
    let t0 = Instant::now();
    let mut ack: Option<u16> = None;
    // Sebelum paket pertama datang belum ada seq yang bisa di-ACK, jadi jendela
    // dipancing dengan estimasi jam goggles - sama seperti di `recv_loop`.
    let ack_now = |ack: Option<u16>| match ack {
        None => ((t0.elapsed().as_secs_f64() * RATE) as u64 & 0xFFFF) as u16,
        Some(v) => v,
    };

    let mut feed = Feeder::new(tx, counters.clone(), params, generation, true, geom);
    let mut prev_idx: Option<u8> = None;
    let mut balasan: Vec<Vec<u8>> = Vec::new();
    let mut muatan: Vec<Vec<u8>> = Vec::new();
    let mut dibuka = 0u8;
    let mut sejak_sub = Instant::now() - Duration::from_millis(100);
    let mut mati = false;

    while !stop.load(Relaxed) && !mati {
        // Kirim dulu, baru baca: tanpa MAC peer belum ada apa pun yang bisa
        // dikirim, jadi ARP diulang sampai goggles menjawab. Kalau ARP-nya
        // hilang, seluruh jalur diam - dan diam tanpa sebab adalah kegagalan
        // yang paling mahal untuk didiagnosis.
        if tumpukan.mac_peer.is_none() {
            if sejak_sub.elapsed() >= Duration::from_millis(200) {
                sejak_sub = Instant::now();
                if let Err(e) = tautan.kirim_frame(&tumpukan.arp_minta()) {
                    log.lock().push(format!("ARP gagal dikirim: {e}"));
                }
            }
        } else if sejak_sub.elapsed() >= Duration::from_millis(30) {
            sejak_sub = Instant::now();
            // Tiga paket open di awal, lalu subscribe selamanya. Open diulang
            // karena paket pertama bisa jatuh sebelum goggles siap; sesudah itu
            // subscribe-lah yang membawa ACK.
            let pesan = if dibuka < 3 {
                dibuka += 1;
                build_open(ack_now(ack), session)
            } else {
                build_subscribe(ack_now(ack), session)
            };
            if let Some(f) = tumpukan.bungkus_udp(port_goggles, &pesan) {
                if let Err(e) = tautan.kirim_frame(&f) {
                    // Kegagalan tulis di USB berarti kabelnya benar-benar
                    // hilang - pipa harus mengaku mati, bukan menunggu diam.
                    log.lock().push(format!("RNDIS berhenti saat mengirim: {e}"));
                    stop.store(true, Relaxed);
                    break;
                }
            }
        }

        // Tenggat pendek supaya giliran ACK tidak pernah tertunda lebih dari
        // beberapa milidetik. Ini bukan polling sibuk: `terima` benar-benar
        // tidur sampai transfer selesai atau tenggatnya lewat.
        let hasil = tautan.terima(Duration::from_millis(5), |eth| {
            match tumpukan.telan(eth) {
                crate::net::Masuk::Udp { dari, awal, panjang } if dari == port_goggles => {
                    let d = &eth[awal..awal + panjang];
                    if paket_video(d) {
                        ack = Some(ack_dari(d) as u16);
                        catat_hilang(&mut prev_idx, d, &counters);
                        muatan.push(d[HDR..].to_vec());
                    }
                }
                // Jawaban ARP tidak bisa dikirim dari dalam sini - `tautan`
                // sedang dipinjam. Ditumpuk, lalu dikirim sesudah pembacaan
                // selesai; jumlahnya beberapa per sesi, bukan per paket.
                crate::net::Masuk::Balas(f) => balasan.push(f),
                _ => {}
            }
        });

        for f in balasan.drain(..) {
            let _ = tautan.kirim_frame(&f);
        }
        for p in muatan.drain(..) {
            if !feed.push(&p) {
                mati = true;
                break;
            }
        }

        if let Err(e) = hasil {
            log.lock().push(format!("RNDIS berhenti: {e}"));
            stop.store(true, Relaxed);
            break;
        }
    }
}

/// Utas baca jalur USB bulk. Jauh lebih pendek dari `recv_loop` karena
/// protokolnya memang tidak punya ACK, sesi, maupun nomor urut untuk diurus.
#[allow(clippy::too_many_arguments)]
fn usb_loop(
    mut goggles: crate::usb::Feed,
    tx: SyncSender<Vec<u8>>,
    counters: Arc<Counters>,
    stop: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    params: Arc<Mutex<Vec<u8>>>,
    log: Arc<Mutex<Vec<String>>>,
    geom: Arc<AtomicBool>,
) {
    // Gate SPS dimatikan di sini, berbeda dari jalur RNDIS. Alasannya: stream
    // ini dimulai oleh kita sendiri lewat magic, jadi kita tidak menyambar di
    // tengah - dan ketujuh implementasi rujukan menyuapi decoder apa adanya
    // tanpa menunggu apa pun. Menahan sampai SPS pertama justru berisiko layar
    // hitam panjang di hardware yang tidak bisa kami uji, karena kekerapan SPS
    // di V1/V2 tidak terverifikasi dari sumber mana pun.
    let mut feed = Feeder::new(tx, counters, params, generation, false, geom);
    let mut buf = Vec::new();

    while !stop.load(Relaxed) {
        match goggles.next(Duration::from_millis(1000), &mut buf) {
            // Sunyi = goggles belum punya sumber gambar (air unit belum hidup
            // atau belum ter-link). Pancing ulang, jangan berhenti.
            Ok(false) => {
                let _ = goggles.kick();
            }
            Ok(true) => {
                if !feed.push(&buf) {
                    break;
                }
            }
            Err(e) => {
                // Tandai pipa mati, bukan cuma keluar dari loop. Tanpa ini
                // `stats` terus melaporkan running dan UI menyuruh pengguna
                // menyalakan LiveView Sharing untuk pipa yang tidak punya
                // jalan pulih - mencolok ulang kabel pun tidak menolong,
                // karena transport hanya dibuka sekali di `start`.
                log.lock().push(format!("USB berhenti: {e}"));
                stop.store(true, Relaxed);
                break;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn recv_loop(
    sock: UdpSocket,
    goggles: String,
    tx: SyncSender<Vec<u8>>,
    counters: Arc<Counters>,
    stop: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    params: Arc<Mutex<Vec<u8>>>,
    geom: Arc<AtomicBool>,
) {
    let session = {
        let n = now_ms();
        [(n >> 3) as u8, (n >> 11) as u8]
    };
    let t0 = Instant::now();
    let ack = Arc::new(AtomicU64::new(u64::MAX)); // MAX = belum ada paket untuk di-ACK

    // Sebelum paket pertama datang belum ada seq yang bisa di-ACK, jadi jendela
    // dipancing dengan estimasi jam goggles: mulai ~0 tiap sesi, naik RATE/detik.
    let ack_now = {
        let ack = ack.clone();
        move || match ack.load(Relaxed) {
            u64::MAX => ((t0.elapsed().as_secs_f64() * RATE) as u64 & 0xFFFF) as u16,
            v => v as u16,
        }
    };

    for _ in 0..3 {
        let _ = sock.send_to(&build_open(ack_now(), session), &goggles);
        std::thread::sleep(Duration::from_millis(20));
    }

    // Keepalive terpisah: subscribe tiap 30 ms sekaligus membawa ACK.
    spawn_named("gc-keepalive", {
        let sock = sock.try_clone().expect("clone socket");
        let goggles = goggles.clone();
        let stop = stop.clone();
        let ack_now = ack_now.clone();
        move || {
            while !stop.load(Relaxed) {
                let _ = sock.send_to(&build_subscribe(ack_now(), session), &goggles);
                std::thread::sleep(Duration::from_millis(30));
            }
        }
    });

    let mut buf = vec![0u8; 65536];
    let mut prev_idx: Option<u8> = None;
    let mut feed = Feeder::new(tx, counters.clone(), params, generation, true, geom);

    while !stop.load(Relaxed) {
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            // ConnectionReset TIDAK boleh mematikan loop ini. Di Windows, paket
            // keepalive ke port goggles yang sedang tidak mendengar memicu ICMP
            // port-unreachable, dan recv() berikutnya membalas WSAECONNRESET.
            // Itu keadaan normal setiap kali Share Liveview dimatikan atau goggles
            // reboot - kalau loop berhenti, ACK berhenti dan stream mati permanen.
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::ConnectionReset
                ) =>
            {
                continue
            }
            // Bukan timeout: adapternya benar-benar hilang. Sama seperti
            // jalur USB, pipa harus mengaku mati supaya UI jujur - tanpa itu
            // layar bilang "menunggu gambar" atas pipa yang tak bisa pulih.
            Err(_) => {
                stop.store(true, Relaxed);
                break;
            }
        };
        let d = &buf[..n];
        if !paket_video(d) {
            continue;
        }
        // Aturan jendela dan hitungan hilang ada di `ack_dari`/`catat_hilang` -
        // satu salinan, dipakai jalur ini dan jalur RNDIS user space.
        ack.store(ack_dari(d), Relaxed);
        catat_hilang(&mut prev_idx, d, &counters);

        if !feed.push(&buf[HDR..n]) {
            break;
        }
    }
}

fn contains(hay: &[u8], needle: &[u8; 4]) -> bool {
    hay.windows(4).any(|w| w == needle)
}

/// Petik NAL SPS (tipe 7) dan PPS (tipe 8) dari satu payload, dikembalikan
/// sebagai Annex-B siap tulis. None kalau salah satu tidak lengkap di payload
/// ini - pemanggil menyimpan yang terakhir berhasil saja.
fn extract_params(payload: &[u8]) -> Option<Vec<u8>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 < payload.len() {
        if payload[i] == 0 && payload[i + 1] == 0 && payload[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let (mut sps, mut pps) = (None, None);
    for (k, &st) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(payload.len(), |&n| n - 3);
        // Start code boleh 3 atau 4 byte, jadi nol di ekor NAL ini sebenarnya
        // milik prefiks berikutnya. Nol di ekor memang padding yang sah.
        while end > st && payload[end - 1] == 0 {
            end -= 1;
        }
        match payload[st] & 0x1F {
            7 => sps = Some(&payload[st..end]),
            8 => pps = Some(&payload[st..end]),
            _ => {}
        }
    }
    let (sps, pps) = (sps?, pps?);
    let mut out = Vec::with_capacity(sps.len() + pps.len() + 8);
    for nal in [sps, pps] {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    Some(out)
}

// -------------------------------------------------------------------- ffmpeg

fn spawn_ffmpeg(exe: &PathBuf) -> std::io::Result<Child> {
    let mut cmd = Command::new(exe);
    cmd.args([
        "-hide_banner",
        // +showall wajib: stream tanpa IDR, tanpa flag ini decoder menunggu
        // keyframe selamanya dan tidak mengeluarkan gambar apa pun.
        "-flags2", "+showall",
        "-err_detect", "ignore_err",
        "-ec", "favor_inter+guess_mvs",
        "-fflags", "nobuffer",
        "-flags", "low_delay",
        "-probesize", "32768",
        "-f", "h264",
        "-i", "pipe:0",
        // NV12 (1,5 byte/piksel) dipilih daripada RGBA (4): 93 MB/s bukan
        // 249 MB/s, dan konversi ke RGB gratis di shader.
        "-f", "rawvideo",
        "-pix_fmt", "nv12",
        "pipe:1",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: jangan kedipkan konsol
    }
    cmd.spawn()
}

/// Pengawas decoder: menyuapi ffmpeg, dan menjalankan generasi baru setiap kali
/// ffmpeg mati. Tanpa ini, satu kali ffmpeg berhenti berarti jendela beku
/// selamanya walau jaringan sehat - dan ffmpeg memang akan berhenti tiap kali
/// feed goggles putus atau resolusinya berubah.
#[allow(clippy::too_many_arguments)]
fn decoder(
    exe: PathBuf,
    rx: Receiver<Vec<u8>>,
    rec: Arc<Mutex<Option<std::fs::File>>>,
    counters: Arc<Counters>,
    frames: Arc<FrameSlot>,
    log: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    child_slot: Arc<Mutex<Option<Child>>>,
    generation: Arc<AtomicU64>,
    geom: Arc<AtomicBool>,
) {
    while !stop.load(Relaxed) {
        // Penanda dari sesi sebelumnya tidak boleh membunuh sesi baru.
        geom.store(false, Relaxed);
        let mut child = match spawn_ffmpeg(&exe) {
            Ok(c) => c,
            Err(e) => {
                log.lock().push(format!("gagal menjalankan ffmpeg: {e}"));
                stop.store(true, Relaxed);
                return;
            }
        };
        let mut stdin = child.stdin.take().expect("stdin ffmpeg");
        let stdout = child.stdout.take().expect("stdout ffmpeg");
        let stderr = child.stderr.take().expect("stderr ffmpeg");
        counters.width.store(0, Relaxed); // resolusi dibaca ulang; mode 16:9/4:3 bisa berganti
        counters.height.store(0, Relaxed);
        let my_gen = generation.load(Relaxed);
        *child_slot.lock() = Some(child);

        spawn_named("gc-stderr", {
            let (counters, log) = (counters.clone(), log.clone());
            move || read_stderr(stderr, counters, log)
        });
        spawn_named("gc-decode", {
            let (counters, frames, stop) = (counters.clone(), frames.clone(), stop.clone());
            // Pembaca ikut membunuh ffmpeg saat gagal: itu menutup stdin, jadi
            // penyuapan di bawah tidak bisa menggantung di pipa yang penuh.
            let (child_slot, generation) = (child_slot.clone(), generation.clone());
            move || {
                read_frames(stdout, counters, frames, stop, generation.clone(), my_gen);
                if generation.load(Relaxed) == my_gen {
                    if let Some(c) = child_slot.lock().as_mut() {
                        let _ = c.kill();
                    }
                }
            }
        });

        // Suapi sampai ffmpeg mati atau pipa menolak.
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(chunk) => {
                    // Gagal tulis harus MENGHENTIKAN rekaman, bukan diam.
                    // Kalau rec dibiarkan Some, tombol Rekam tetap menyala
                    // merah sementara berkasnya berhenti bertambah - kehilangan
                    // data pada fitur yang justru berbayar.
                    let mut g = rec.lock();
                    if let Some(f) = g.as_mut() {
                        if f.write_all(&chunk).is_ok() {
                            counters.rec_bytes.fetch_add(chunk.len() as u64, Relaxed);
                        } else {
                            *g = None;
                            log.lock()
                                .push("rekaman berhenti: gagal menulis berkas (disk penuh?)".into());
                        }
                    }
                    drop(g);
                    if stdin.write_all(&chunk).is_err() {
                        break;
                    }
                    // Geometri berganti: ffmpeg tidak akan mati sendiri, jadi
                    // kita yang menyegarkannya. Tanpa ini ukuran frame lama
                    // dipakai selamanya dan gambarnya melebar.
                    if geom.swap(false, Relaxed) {
                        log.lock().push("geometri stream berubah - decoder disegarkan".into());
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let dead = child_slot
                        .lock()
                        .as_mut()
                        .is_none_or(|c| matches!(c.try_wait(), Ok(Some(_)) | Err(_)));
                    if dead || stop.load(Relaxed) {
                        break;
                    }
                }
                // Loop jaringan berhenti: pipa memang sedang dimatikan.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }

        drop(stdin);
        if let Some(mut c) = child_slot.lock().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if stop.load(Relaxed) {
            return;
        }
        counters.restarts.fetch_add(1, Relaxed);
        generation.fetch_add(1, Relaxed); // minta loop jaringan menyinkron ulang di SPS
        log.lock().push("decoder dijalankan ulang".into());
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Ambil ukuran gambar dari log ffmpeg, hitung frame korup, dan simpan beberapa
/// baris terakhir untuk ditampilkan di UI kalau ada yang salah.
///
/// Dua penyaringan di sini bukan kosmetik. ffmpeg menulis baris progres
/// (`frame= 3561 fps=32 ...`) beberapa kali per detik, dan pesan korup bisa
/// datang ratusan kali beruntun; keduanya mengusir baris yang benar-benar
/// menjelaskan masalah dari jendela 40 baris. Terbukti: kotak Diagnostik pernah
/// terisi progres saja, nol keterangan. Progres dibuang (angkanya sudah ada di
/// bar sebagai FPS/bitrate), dan baris berulang dipadatkan jadi satu.
fn read_stderr(stderr: std::process::ChildStderr, counters: Arc<Counters>, log: Arc<Mutex<Vec<String>>>) {
    let mut rd = std::io::BufReader::new(stderr);
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    let mut ulang: Option<(String, u32)> = None;
    loop {
        match rd.read(&mut byte) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if byte[0] != b'\n' && byte[0] != b'\r' {
            line.push(byte[0]);
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let s = String::from_utf8_lossy(&line).to_string();
        line.clear();
        catat(s, &counters, &log, &mut ulang);
    }
}

/// Olah satu baris stderr ffmpeg: petik ukuran, hitung korupsi, lalu catat -
/// dengan progres dibuang dan baris berulang dipadatkan.
fn catat(
    s: String,
    counters: &Counters,
    log: &Mutex<Vec<String>>,
    ulang: &mut Option<(String, u32)>,
) {
    if counters.width.load(Relaxed) == 0 {
        if let Some((w, h)) = find_dims(&s) {
            counters.width.store(w as u64, Relaxed);
            counters.height.store(h as u64, Relaxed);
        }
    }
    // Frame korup dihitung, bukan cuma dicatat: hanya dengan angkanya pengguna
    // bisa membedakan korupsi transien (naik lalu berhenti - itu satu putaran
    // intra-refresh, normal) dari yang menetap (terus naik - itu masalah).
    if s.contains("corrupt") {
        counters.corrupt.fetch_add(1, Relaxed);
    }
    // Baris progres ffmpeg dibuang; angkanya sudah tampil di bar.
    if s.starts_with("frame=") || s.starts_with("size=") {
        return;
    }

    let mut g = log.lock();
    if let Some((prev, n)) = ulang.as_mut() {
        if *prev == s {
            *n += 1;
            if let Some(last) = g.last_mut() {
                *last = format!("{s}  ({n}x)");
            }
            return;
        }
    }
    *ulang = Some((s.clone(), 1));
    if g.len() > 40 {
        g.remove(0);
    }
    g.push(s);
}

/// Cari pola `<lebar>x<tinggi>` di baris log. Dibatasi 2-5 digit tiap sisi dan
/// rentang wajar supaya token seperti `0x3231564E` tidak ikut tertangkap.
fn find_dims(s: &str) -> Option<(u32, u32)> {
    let b = s.as_bytes();
    for (i, _) in s.char_indices().filter(|(_, c)| *c == 'x') {
        let l: &str = &s[..i];
        let ln = l.len() - l.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        let r: &str = &s[i + 1..];
        let rn = r.len() - r.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if !(2..=5).contains(&ln) || !(2..=5).contains(&rn) {
            continue;
        }
        if b.get(i + 1 + rn).is_some_and(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        let w: u32 = l[l.len() - ln..].parse().ok()?;
        let h: u32 = r[..rn].parse().ok()?;
        if (16..=8192).contains(&w) && (16..=8192).contains(&h) {
            return Some((w, h));
        }
    }
    None
}

fn read_frames(
    mut stdout: std::process::ChildStdout,
    counters: Arc<Counters>,
    frames: Arc<FrameSlot>,
    stop: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    my_gen: u64,
) {
    // Ukuran gambar baru diketahui setelah ffmpeg mencetak info stream.
    let (w, h) = loop {
        let (w, h) = (counters.width.load(Relaxed), counters.height.load(Relaxed));
        if w > 0 && h > 0 {
            break (w as u32, h as u32);
        }
        // Generasi berganti sementara ukuran belum pernah datang: menyerah,
        // supaya thread ini tidak berputar selamanya setelah decoder diganti.
        if stop.load(Relaxed) || generation.load(Relaxed) != my_gen {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let flen = (w as usize * h as usize * 3) / 2; // NV12

    while !stop.load(Relaxed) {
        let mut buf = frames.buffer(flen);
        if stdout.read_exact(&mut buf).is_err() {
            return;
        }
        counters.decoded.fetch_add(1, Relaxed);
        frames.put(w, h, buf);
    }
}

// ---------------------------------------------------------------- HTTP frame

/// Kirim frame ke webview sebagai satu respons HTTP chunked yang tidak pernah
/// selesai. Dipilih daripada WebSocket karena tidak butuh crate apa pun -
/// chunked encoding cuma butuh panjang dalam hex per potong.
///
/// Tiap potong berisi: `[u16 lebar][u16 tinggi][u32 panjang][data NV12]`.
fn serve_frames(listener: TcpListener, frames: Arc<FrameSlot>, stop: Arc<AtomicBool>) {
    for conn in listener.incoming() {
        if stop.load(Relaxed) {
            return;
        }
        let Ok(mut s) = conn else { continue };
        let frames = frames.clone();
        let stop = stop.clone();
        spawn_named("gc-stream", move || {
            let _ = s.set_nodelay(true);
            // Habiskan baris permintaan; isinya tidak dipakai, hanya satu rute.
            let mut junk = [0u8; 1024];
            let _ = s.read(&mut junk);
            let hdr = "HTTP/1.1 200 OK\r\n\
                       Content-Type: application/octet-stream\r\n\
                       Cache-Control: no-store\r\n\
                       Access-Control-Allow-Origin: *\r\n\
                       Transfer-Encoding: chunked\r\n\r\n";
            if s.write_all(hdr.as_bytes()).is_err() {
                return;
            }
            let mut chunk = Vec::new();
            while !stop.load(Relaxed) {
                let Some(f) = frames.take(Duration::from_millis(500)) else { continue };
                chunk.clear();
                let body = 8 + f.data.len();
                chunk.extend_from_slice(format!("{body:x}\r\n").as_bytes());
                chunk.extend_from_slice(&(f.w as u16).to_le_bytes());
                chunk.extend_from_slice(&(f.h as u16).to_le_bytes());
                chunk.extend_from_slice(&(f.data.len() as u32).to_le_bytes());
                let ok = s.write_all(&chunk).is_ok()
                    && s.write_all(&f.data).is_ok()
                    && s.write_all(b"\r\n").is_ok();
                frames.recycle(f.data);
                if !ok {
                    return;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_xor_selalu_nol() {
        // Aturan yang terbukti di 882/882 paket subscribe klien resmi.
        for len in [34u16, 48] {
            for kind in [KIND_OPEN, KIND_SUB, KIND_VIDEO] {
                let h = head(len, [0x5d, 0xa8], 0x1234, kind);
                assert_eq!(h.iter().fold(0u8, |a, b| a ^ b), 0, "len={len} kind={kind}");
            }
        }
    }

    #[test]
    fn panjang_paket_cocok_dengan_field_panjang() {
        let o = build_open(0x1234, [1, 2]);
        assert_eq!(o.len(), 48);
        assert_eq!(u16::from_le_bytes([o[0], o[1]]) & 0x7fff, 48);
        let s = build_subscribe(0x1234, [1, 2]);
        assert_eq!(s.len(), 34);
        assert_eq!(u16::from_le_bytes([s[0], s[1]]) & 0x7fff, 34);
    }

    /// Sambungan tiga lapis jalur macOS, tanpa hardware: RNDIS_PACKET ->
    /// Ethernet -> IPv4/UDP -> paket video DJI.
    ///
    /// Ini tes yang paling berharga di jalur itu. Ketiga lapisnya punya offset
    /// sendiri (`data_offset` dihitung dari byte ke-8, IHL dari nibble, header
    /// video 20 byte), dan salah satu saja bergeser membuat muatan yang lewat
    /// tetap "masuk akal" - panjangnya wajar, tidak ada galat - tapi decoder
    /// menerima sampah. Kegagalan seperti itu di hardware terlihat sebagai
    /// layar hitam tanpa satu pun pesan.
    #[test]
    fn rndis_ke_paket_video_sampai_utuh() {
        use crate::net::{Masuk, Tumpukan};
        use std::net::Ipv4Addr;

        let kita = Ipv4Addr::new(192, 168, 60, 1);
        let peer = Ipv4Addr::new(192, 168, 60, 2);
        let mut sisi_goggles = Tumpukan::baru(peer, kita, 9003);
        sisi_goggles.mac_peer = Some(crate::net::MAC_KITA);
        let mut sisi_kita = Tumpukan::baru(kita, peer, 40000);
        sisi_kita.mac_peer = Some([0x9e, 1, 2, 3, 4, 5]);

        // Satu paket video DJI: header 20 byte (seq di 4..6, kind di 6, indeks
        // di 16) plus muatan H.264.
        let muatan: Vec<u8> = (0..250u8).collect();
        let mut dji = vec![0u8; HDR];
        dji[4..6].copy_from_slice(&0xBEEFu16.to_le_bytes());
        dji[6] = KIND_VIDEO;
        dji[16] = 42;
        dji.extend_from_slice(&muatan);

        // Goggles membungkusnya: UDP -> IPv4 -> Ethernet -> RNDIS.
        let frame = sisi_goggles.bungkus_udp(40000, &dji).unwrap();
        let transfer = crate::rndis::bangun_paket(&frame);

        // Sisi kita membukanya kembali, lapis demi lapis.
        let mut diterima = Vec::new();
        crate::rndis::untuk_tiap_frame(&transfer, |eth| {
            if let Masuk::Udp { dari, awal, panjang } = sisi_kita.telan(eth) {
                assert_eq!(dari, 9003, "port sumber goggles");
                diterima.push(eth[awal..awal + panjang].to_vec());
            }
        });

        assert_eq!(diterima.len(), 1, "paket video tidak sampai");
        let d = &diterima[0];
        assert_eq!(d, &dji, "muatan UDP berubah di perjalanan");
        assert!(paket_video(d), "kind video tidak terbaca");
        assert_eq!(ack_dari(d), 0xBEEF, "jendela ACK terbaca dari offset salah");
        let counters = Counters::default();
        let mut prev = Some(40u8);
        catat_hilang(&mut prev, d, &counters);
        assert_eq!(counters.lost.load(Relaxed), 1, "indeks 40 -> 42 = satu hilang");
        assert_eq!(&d[HDR..], &muatan[..], "muatan H.264 harus utuh");
    }

    /// Beberapa paket video dalam SATU transfer bulk - bentuk yang memang
    /// dihasilkan perangkat RNDIS berbandwidth besar. Kalau hanya yang pertama
    /// terbaca, videonya patah-patah sementara penghitung hilang tetap nol.
    #[test]
    fn banyak_paket_video_dalam_satu_transfer() {
        use crate::net::{Masuk, Tumpukan};
        use std::net::Ipv4Addr;

        let kita = Ipv4Addr::new(192, 168, 60, 1);
        let peer = Ipv4Addr::new(192, 168, 60, 2);
        let mut sisi_goggles = Tumpukan::baru(peer, kita, 9003);
        sisi_goggles.mac_peer = Some(crate::net::MAC_KITA);
        let mut sisi_kita = Tumpukan::baru(kita, peer, 40000);
        sisi_kita.mac_peer = Some([0x9e, 1, 2, 3, 4, 5]);

        let mut transfer = Vec::new();
        for i in 0..4u8 {
            let mut dji = vec![0u8; HDR];
            dji[6] = KIND_VIDEO;
            dji[16] = i;
            dji.extend_from_slice(&[i; 100]);
            let f = sisi_goggles.bungkus_udp(40000, &dji).unwrap();
            transfer.extend(crate::rndis::bangun_paket(&f));
        }

        let mut n = 0;
        crate::rndis::untuk_tiap_frame(&transfer, |eth| {
            if let Masuk::Udp { awal, panjang, .. } = sisi_kita.telan(eth) {
                let d = &eth[awal..awal + panjang];
                assert_eq!(&d[HDR..], &[n as u8; 100], "urutan paket berubah");
                n += 1;
            }
        });
        assert_eq!(n, 4);
    }

    #[test]
    fn ukuran_gambar_terbaca_bukan_token_hex() {
        let l = "Stream #0:0: Video: h264 (High), yuv420p(progressive), 1920x1080, 30 fps";
        assert_eq!(find_dims(l), Some((1920, 1080)));
        assert_eq!(find_dims("rawvideo (NV12 / 0x3231564E), nv12"), None);
        assert_eq!(find_dims("tidak ada ukuran di sini"), None);
        assert_eq!(find_dims("Video: h264, 1440x1080,"), Some((1440, 1080)));
    }

    #[test]
    fn parameter_set_dipetik_dari_payload() {
        // Rekaman yang tidak dimulai dengan SPS+PPS tidak bisa dibuka pemutar.
        let sps = [0x67u8, 0x64, 0x00, 0x28];
        let pps = [0x68u8, 0xee, 0x3c, 0x80];
        let mut pay = vec![0, 0, 0, 1, 0x09, 0x10]; // AUD dulu, seperti aslinya
        pay.extend_from_slice(&[0, 0, 0, 1]);
        pay.extend_from_slice(&sps);
        pay.extend_from_slice(&[0, 0, 0, 1]);
        pay.extend_from_slice(&pps);
        pay.extend_from_slice(&[0, 0, 0, 1, 0x61, 0xaa]); // awal slice
        let got = extract_params(&pay).expect("SPS+PPS ada di payload ini");
        let mut want = vec![0, 0, 0, 1];
        want.extend_from_slice(&sps);
        want.extend_from_slice(&[0, 0, 0, 1]);
        want.extend_from_slice(&pps);
        assert_eq!(got, want);

        // Hanya slice: tidak ada yang bisa dipetik.
        assert_eq!(extract_params(&[0, 0, 0, 1, 0x61, 0xaa]), None);
        // SPS tanpa PPS juga belum cukup.
        assert_eq!(extract_params(&[0, 0, 0, 1, 0x67, 0x64]), None);
    }

    #[test]
    fn slot_hanya_menyimpan_frame_terbaru() {
        let s = FrameSlot::default();
        s.put(2, 2, vec![1; 6]);
        s.put(2, 2, vec![2; 6]);
        let f = s.take(Duration::from_millis(10)).unwrap();
        assert_eq!(f.data[0], 2, "frame lama harus dibuang, bukan diantre");
        assert!(s.take(Duration::from_millis(10)).is_none());
    }

    // ------------------------------------------------------- feeder bersama

    /// Rakit feeder beserta ujung terima antreannya.
    #[allow(clippy::type_complexity)]
    fn feeder(
        gate: bool,
        kapasitas: usize,
    ) -> (Feeder, Receiver<Vec<u8>>, Arc<Counters>, Arc<Mutex<Vec<u8>>>, Arc<AtomicU64>, Arc<AtomicBool>) {
        let (tx, rx) = sync_channel::<Vec<u8>>(kapasitas);
        let c = Arc::new(Counters::default());
        let p = Arc::new(Mutex::new(Vec::new()));
        let g = Arc::new(AtomicU64::new(0));
        let geo = Arc::new(AtomicBool::new(false));
        (Feeder::new(tx, c.clone(), p.clone(), g.clone(), gate, geo.clone()), rx, c, p, g, geo)
    }

    /// Payload berisi SPS+PPS. `pendek` memilih start code 3 byte (bentuk
    /// jalur USB V1/V2) atau 4 byte (bentuk jalur RNDIS Goggles 3).
    fn dengan_sps_kode(ekor: u8, pendek: bool) -> Vec<u8> {
        let kode: &[u8] = if pendek { &[0, 0, 1] } else { &[0, 0, 0, 1] };
        let mut v = Vec::new();
        v.extend_from_slice(kode);
        v.extend_from_slice(&[0x67, 0x64, 0x00, 0x28]);
        v.extend_from_slice(kode);
        v.extend_from_slice(&[0x68, 0xee, 0x3c, 0x80]);
        v.push(ekor);
        v
    }

    fn dengan_sps(ekor: u8) -> Vec<u8> {
        dengan_sps_kode(ekor, false)
    }

    /// Jalur RNDIS menyambar stream yang sudah berjalan, jadi paket pertama
    /// hampir pasti tengah frame. Menyuapkannya ke decoder menghasilkan derau,
    /// bukan gambar - jadi ia ditahan sampai SPS pertama.
    #[test]
    fn gate_menahan_sampai_sps_lalu_melewatkan_semua() {
        let (mut f, rx, c, _, _, _) = feeder(true, 8);
        assert!(f.push(&[0, 0, 0, 1, 0x61, 0xaa])); // slice: dibuang
        assert!(rx.try_recv().is_err(), "sebelum SPS tidak boleh ada yang lewat");
        assert_eq!(c.pkts.load(Relaxed), 0, "yang dibuang tidak boleh ikut dihitung");

        assert!(f.push(&dengan_sps(1)));
        assert!(rx.try_recv().is_ok());
        // Sesudah sinkron, payload tanpa SPS pun ikut lewat.
        assert!(f.push(&[0, 0, 0, 1, 0x61, 0xbb]));
        assert!(rx.try_recv().is_ok());
    }

    /// Jalur USB bulk memulai stream sendiri lewat magic, jadi tidak ada yang
    /// perlu ditunggu - dan menunggu SPS di sana berisiko layar hitam panjang.
    #[test]
    fn tanpa_gate_payload_pertama_langsung_lewat() {
        let (mut f, rx, c, _, _, _) = feeder(false, 8);
        assert!(f.push(&[0, 0, 0, 1, 0x61, 0xaa]));
        assert_eq!(rx.try_recv().unwrap(), vec![0, 0, 0, 1, 0x61, 0xaa]);
        assert_eq!(c.pkts.load(Relaxed), 1);
    }

    /// Decoder dijalankan ulang -> mulai lagi dari SPS. Tanpa ini, generasi baru
    /// disuapi dari tengah frame dan gambarnya rusak sampai SPS berikutnya.
    #[test]
    fn generasi_baru_menyinkron_ulang_hanya_kalau_digate() {
        let (mut f, rx, _, _, g, _) = feeder(true, 8);
        f.push(&dengan_sps(1));
        rx.try_recv().unwrap();

        g.fetch_add(1, Relaxed);
        assert!(f.push(&[0, 0, 0, 1, 0x61, 0xaa]));
        assert!(rx.try_recv().is_err(), "generasi baru harus menunggu SPS lagi");

        // Jalur USB tidak digate, jadi generasi baru tidak menahan apa pun.
        let (mut f2, rx2, _, _, g2, _) = feeder(false, 8);
        f2.push(&[0, 0, 0, 1, 0x61, 0xaa]);
        rx2.try_recv().unwrap();
        g2.fetch_add(1, Relaxed);
        assert!(f2.push(&[0, 0, 0, 1, 0x61, 0xbb]));
        assert!(rx2.try_recv().is_ok());
    }

    /// Parameter set dipetik di KEDUA jalur: tanpa itu rekaman .h264 tidak bisa
    /// dibuka pemutar. Di jalur USB ia dipetik walau gate mati.
    #[test]
    fn parameter_set_dipetik_walau_tanpa_gate() {
        for gate in [true, false] {
            let (mut f, _rx, _, p, _, _) = feeder(gate, 8);
            f.push(&dengan_sps(1));
            assert!(!p.lock().is_empty(), "gate={gate}");
        }
    }

    /// Antrean penuh -> paket dibuang, BUKAN ditunggu. Di jalur RNDIS memblokir
    /// di sini menghentikan ACK, dan stream goggles beku permanen.
    #[test]
    fn antrean_penuh_membuang_bukan_memblokir() {
        let (mut f, rx, c, _, _, _) = feeder(false, 1);
        assert!(f.push(&[1, 2, 3]));
        assert!(f.push(&[4, 5, 6]), "harus tetap jalan, bukan menggantung");
        assert_eq!(c.dropped.load(Relaxed), 1);
        drop(rx);
        assert!(!f.push(&[7, 8, 9]), "decoder hilang -> loop harus berhenti");
    }

    /// Start code 3 byte harus dikenali sama baiknya. Ini BUKAN kelonggaran
    /// teoretis: jalur USB V1/V2 memakai bentuk itu, dan needle 4 byte akan
    /// melewatkan seluruh SPS di sana - akibatnya rekaman .h264 tidak bisa
    /// dibuka pemutar, dan hitungan frame di UI mati diam-diam.
    #[test]
    fn start_code_tiga_byte_dikenali_sama_dengan_empat() {
        for pendek in [true, false] {
            let (mut f, rx, c, p, _, _) = feeder(true, 8);
            assert!(f.push(&dengan_sps_kode(1, pendek)), "pendek={pendek}");
            assert!(rx.try_recv().is_ok(), "gate tidak terbuka, pendek={pendek}");
            assert!(!p.lock().is_empty(), "params tidak terpetik, pendek={pendek}");
            assert_eq!(c.pkts.load(Relaxed), 1, "pendek={pendek}");
        }
    }

    /// Byte tipe NAL harus benar-benar dicocokkan, bukan cuma prefiksnya -
    /// kalau tidak, gate terbuka di slice biasa dan decoder disuapi dari tengah
    /// frame, yang justru dihindari gate itu.
    #[test]
    fn bukan_sps_tidak_membuka_gate() {
        let (mut f, rx, _, p, _, _) = feeder(true, 8);
        // Start code + NAL tipe 1 (slice non-IDR), bukan 7.
        assert!(f.push(&[0, 0, 1, 0x61, 0xaa, 0xbb]));
        assert!(rx.try_recv().is_err(), "slice biasa tidak boleh membuka gate");
        assert!(p.lock().is_empty());
    }

    /// Inti perbaikan gambar-melebar. ffmpeg TIDAK mati saat resolusi stream
    /// berubah: ia terus menulis frame dengan ukuran lama sementara pembaca
    /// sudah mengunci panjang frame, jadi gambarnya melebar dan ukuran barunya
    /// tidak pernah terbaca. Satu-satunya petunjuk di dalam stream adalah
    /// parameter set yang berganti - jadi itu yang harus memicu restart.
    #[test]
    fn parameter_set_berganti_meminta_decoder_disegarkan() {
        let (mut f, _rx, _, _, _, geo) = feeder(false, 8);

        // SPS pertama: belum ada pembanding, jadi belum boleh minta restart.
        f.push(&dengan_sps_kode(1, false));
        assert!(!geo.load(Relaxed), "SPS pertama bukan perubahan geometri");

        // SPS sama diulang - stream sehat mengirimnya berkala. Bukan perubahan.
        f.push(&dengan_sps_kode(1, false));
        assert!(!geo.load(Relaxed), "SPS identik tidak boleh memicu restart");

        // SPS berbeda: goggles berganti mode. Inilah yang harus memicu.
        let mut lain = vec![0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f];
        lain.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80]);
        f.push(&lain);
        assert!(geo.load(Relaxed), "SPS berubah harus meminta decoder disegarkan");
    }

    /// Payload tanpa SPS tidak boleh menyentuh penanda - kalau ia ikut terangkat
    /// oleh slice biasa, decoder akan direstart berkali-kali per detik.
    #[test]
    fn slice_biasa_tidak_meminta_restart() {
        let (mut f, _rx, _, _, _, geo) = feeder(false, 8);
        f.push(&dengan_sps_kode(1, false));
        for _ in 0..50 {
            f.push(&[0, 0, 1, 0x61, 0xaa, 0xbb]);
        }
        assert!(!geo.load(Relaxed));
    }

    // ------------------------------------------------------------- log ffmpeg

    /// Kotak Diagnostik pernah terisi baris progres saja - nol keterangan.
    /// Progres datang beberapa kali per detik, jadi ia mengusir seluruh jendela
    /// 40 baris dan membuat panel itu tidak berguna justru saat dibutuhkan.
    #[test]
    fn progres_ffmpeg_tidak_boleh_mengusir_keterangan() {
        let c = Counters::default();
        let log = Mutex::new(Vec::new());
        let mut u = None;
        catat("Stream #0:0: Video: h264, 1440x1080".into(), &c, &log, &mut u);
        for i in 0..200 {
            catat(format!("frame= {i} fps=32 q=-0.0 size= 100kB"), &c, &log, &mut u);
            catat(format!("size= {i}kB time=00:00:01.00"), &c, &log, &mut u);
        }
        let g = log.lock();
        assert_eq!(g.len(), 1, "progres tidak boleh ikut tercatat: {g:?}");
        assert!(g[0].contains("1440x1080"));
    }

    /// Pesan korup bisa datang ratusan kali beruntun. Dipadatkan jadi satu baris
    /// berpencacah supaya ia tidak mengusir baris lain - dan tetap terlihat.
    #[test]
    fn baris_berulang_dipadatkan_bukan_ditumpuk() {
        let c = Counters::default();
        let log = Mutex::new(Vec::new());
        let mut u = None;
        for _ in 0..120 {
            catat("pipe:0: corrupt decoded frame in stream 0".into(), &c, &log, &mut u);
        }
        catat("keterangan lain".into(), &c, &log, &mut u);
        let g = log.lock();
        assert_eq!(g.len(), 2, "dapat: {g:?}");
        assert!(g[0].contains("(120x)"), "dapat: {}", g[0]);
        assert_eq!(g[1], "keterangan lain");
    }

    /// Angka korupsi harus ikut naik walau barisnya dipadatkan - kalau tidak,
    /// pencacah di Diagnostik akan berhenti di 1 dan menyembunyikan masalah.
    #[test]
    fn korupsi_dihitung_tiap_kejadian_bukan_tiap_baris_unik() {
        let c = Counters::default();
        let log = Mutex::new(Vec::new());
        let mut u = None;
        for _ in 0..37 {
            catat("pipe:0: corrupt decoded frame in stream 0".into(), &c, &log, &mut u);
        }
        assert_eq!(c.corrupt.load(Relaxed), 37);
        // Progres yang memuat kata itu pun ikut dihitung, tapi tidak dicatat.
        catat("frame= 9 fps=30 corrupt".into(), &c, &log, &mut u);
        assert_eq!(c.corrupt.load(Relaxed), 38);
        assert_eq!(log.lock().len(), 1);
    }
}
