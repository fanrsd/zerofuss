//! Zerofuss - live view goggles DJI di jendela desktop, lewat kabel USB.
//!
//! Rust memegang transport + decode; webview cuma menggambar dan menyediakan UI.
//! Video tidak lewat IPC Tauri (3 MB/frame terlalu mahal untuk JSON) melainkan
//! lewat satu respons HTTP chunked di loopback - lihat `pipeline::serve_frames`.
//!
//! Tiga jalur, dua keluarga goggles:
//!
//!   Goggles 3 / N3, Windows & Linux   adapter RNDIS milik OS + UDP  (`pipeline`)
//!   Goggles 3 / N3, macOS             host RNDIS di user space      (`rndis`+`net`)
//!   FPV Goggles V1 / V2, semua OS     USB bulk + magic "RMVT"       (`usb`)
//!
//! Jalur macOS ada karena macOS tidak punya driver RNDIS sama sekali - dan
//! justru itu yang membuatnya mungkin: tidak ada modul kernel yang memegang
//! interface RNDIS goggles, jadi aplikasi boleh mengklaimnya sendiri lalu
//! membentuk ARP/IPv4/UDP-nya di dalam proses. Di Windows dan Linux kernel yang
//! memegang interface itu, jadi di sana jalurnya tetap lewat adapter OS.
//!
//! Semua yang di hilir - decode, rekaman, penghitung, server frame - sama.

mod net;
mod pipeline;
mod rndis;
mod usb;

use parking_lot::Mutex;
use pipeline::{Config, Pipeline, Source};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, State};

/// Alamat jalur goggles TIDAK ada di UI, dan itu keputusan.
///
/// Keempat nilai lama (alamat goggles, port video, alamat PC, port kontrol)
/// tidak satu pun bisa dipilih bebas: `192.168.60.2:9003` ditentukan firmware
/// goggles, dan `192.168.60.1` adalah alamat yang harus dipasang di adapter
/// RNDIS agar berada di subnet yang sama. Menampilkannya sebagai kotak isian
/// hanya menawarkan cara merusak sambungan sendiri.
///
/// Port kontrol lokal dibuang sama sekali: `build_open`/`build_subscribe` di
/// pipeline.rs membuktikan nomor port tidak pernah ikut di dalam paket -
/// goggles membalas ke port pengirim. Jadi socket-nya sekarang mengambil port
/// ephemeral, dan seluruh kelas kegagalan "port 12346 dipegang aplikasi lain"
/// ikut hilang.
///
/// Nilainya tetap ada di settings.json sebagai jalan keluar darurat kalau ada
/// mesin dengan tabrakan subnet - tapi tanpa UI, karena itu bukan pengaturan
/// yang perlu dipikirkan pengguna.
#[derive(Serialize, Deserialize, Clone)]
pub struct Settings {
    pub goggles: String,
    pub goggles_port: u16,
    pub local: String,
    pub rec_dir: String,
    pub auto_start: bool,
    /// Onboarding sudah pernah dituntaskan. Disimpan supaya modal langkah
    /// hanya memaksa diri sekali; sesudahnya ia hanya muncul kalau diminta.
    #[serde(default)]
    pub onboarded: bool,
    /// PID USB FPV Goggles V1/V2. Tanpa UI, seperti alamat jalur di atas -
    /// tapi ada, dan alasannya jujur: `0x001f` diambil dari tujuh proyek open
    /// source yang semuanya memakai satu PID sambil menyatakan mendukung "V1 &
    /// V2", dan tidak satu pun dokumen menyebut PID V1 secara terpisah. Kalau
    /// ternyata ada V1 dengan PID lain, pemiliknya bisa jalan tanpa menunggu
    /// rilis baru. Kami tidak punya V1 untuk memastikannya.
    #[serde(default = "pid_bulk_bawaan")]
    pub usb_pid: u16,
}

/// Nilai bawaan `usb_pid` untuk settings.json yang ditulis sebelum kolom ini ada.
fn pid_bulk_bawaan() -> u16 {
    usb::PID_BULK
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            goggles: "192.168.60.2".into(),
            goggles_port: 9003,
            local: "192.168.60.1".into(),
            rec_dir: String::new(),
            auto_start: true,
            onboarded: false,
            usb_pid: usb::PID_BULK,
        }
    }
}

/// Laju dihitung dari selisih dua cuplikan penghitung, bukan rata-rata sejak
/// mulai - kalau tidak, angka di UI tidak akan pernah turun saat feed berhenti.
struct Sample {
    at: Instant,
    bytes: u64,
    decoded: u64,
    mbps: f64,
    fps: f64,
}

#[derive(Default)]
struct AppState {
    pipe: Mutex<Option<Pipeline>>,
    settings: Mutex<Settings>,
    sample: Mutex<Option<Sample>>,
    /// Hasil probe adapter beserta kapan diambil. Probe-nya memanggil
    /// PowerShell (~300 ms), jadi tidak boleh ikut polling 4 Hz; cache pendek
    /// membuatnya aman dipanggil dari `stats`.
    adapter: Mutex<Option<(Instant, Probe)>>,
}

#[derive(Serialize, Default)]
struct Stats {
    running: bool,
    port: u16,
    width: u32,
    height: u32,
    mbps: f64,
    fps: f64,
    loss: f64,
    pkts: u64,
    lost: u64,
    dropped: u64,
    decoded: u64,
    /// Berapa kali decoder disegarkan sendiri. Ini angka self-heal: naik sekali
    /// saat geometri stream berganti atau ffmpeg mati, lalu tenang.
    restarts: u64,
    /// Frame yang selesai dengan galat. Yang penting bukan nilainya, tapi
    /// apakah ia masih bertambah - lihat komentar di Counters::corrupt.
    corrupt: u64,
    /// Tiga keadaan, bukan boolean: "siap" | "belum-ip" | "tidak-ada".
    /// UI perlu membedakan "adapter belum punya alamat" (Setel adapter berguna)
    /// dari "goggles belum tercolok" (Setel adapter tidak akan berhasil, jadi
    /// tombolnya harus mati).
    adapter: String,
    recording: bool,
    rec_bytes: u64,
}

// ------------------------------------------------------------------ perintah

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> Settings {
    state.settings.lock().clone()
}

#[tauri::command]
fn set_settings(app: AppHandle, state: State<'_, AppState>, s: Settings) -> Result<(), String> {
    *state.settings.lock() = s.clone();
    save_settings(&app, &s).map_err(|e| e.to_string())
}

#[tauri::command]
fn start(app: AppHandle, state: State<'_, AppState>) -> Result<u16, String> {
    if let Some(p) = state.pipe.lock().as_ref() {
        return Ok(p.port);
    }
    let s = state.settings.lock().clone();
    let ffmpeg = find_ffmpeg(&app).ok_or_else(|| {
        "ffmpeg tidak ditemukan - taruh ffmpeg di folder aplikasi, tambahkan ke PATH, atau set ZEROFUSS_FFMPEG".to_string()
    })?;
    let pipe = Pipeline::start(Config { source: pilih_sumber(usb::probe(s.usb_pid, true), s), ffmpeg })?;
    let port = pipe.port;
    let mut g = state.pipe.lock();
    if let Some(ada) = g.as_ref() {
        // Kalah balapan: pakai pipa yang sudah ada, dan pipa baru ini ter-drop
        // di akhir fungsi - lengkap dengan ffmpeg dan seluruh utasnya.
        return Ok(ada.port);
    }
    *g = Some(pipe);
    drop(g);
    *state.sample.lock() = None;
    Ok(port)
}

/// Jalur dipilih dari apa yang TERCOLOK, bukan dari setelan: pengguna tidak
/// perlu tahu keluarga goggles-nya memakai USB bulk atau RNDIS.
///
/// Salah arah di sini adalah jalan buntu, bukan cuma pesan jelek: pemilik
/// V1/V2 yang diarahkan ke jalur RNDIS akan diminta memasang IP statis yang
/// tidak akan dipakai apa pun, di adapter yang tidak pernah ada.
fn pilih_sumber(ada: usb::Found, s: Settings) -> Source {
    match ada {
        // Driver belum terpasang tetap dicoba, bukan ditolak di sini: dengan
        // begitu pesan galatnya datang dari `usb::jelaskan` - satu tempat -
        // bukan ringkasan kedua yang harus dijaga tetap sebunyi.
        usb::Found::Bulk | usb::Found::BulkNoDriver => Source::Bulk { pid: s.usb_pid },
        // Goggles 3 / N3. Jalurnya ditentukan OS, bukan setelan: di macOS
        // interface RNDIS-nya diklaim sendiri, di Windows/Linux dipinjam dari
        // adapter yang sudah dibuat kernel.
        usb::Found::Rndis | usb::Found::None if usb::Os::HOST.rndis_usb() => Source::RndisUsb {
            pid: usb::PID_RNDIS,
            goggles: s.goggles,
            goggles_port: s.goggles_port,
            local: s.local,
        },
        usb::Found::Rndis | usb::Found::None => Source::Rndis {
            goggles: s.goggles,
            goggles_port: s.goggles_port,
            local: s.local,
        },
    }
}

#[tauri::command]
fn stop(state: State<'_, AppState>) {
    state.pipe.lock().take(); // Drop mematikan ffmpeg dan seluruh thread
    *state.sample.lock() = None;
}

#[tauri::command]
fn stats(state: State<'_, AppState>) -> Stats {
    let s = state.settings.lock().clone();
    let mut guard = state.pipe.lock();

    // Utas sumbernya bisa berhenti sendiri - kabel dicabut, adapter hilang,
    // ffmpeg gagal dijalankan ulang - dan sesudah itu tidak ada apa pun yang
    // bisa memulihkan pipa: transport hanya dibuka sekali di `start`. Dipanen
    // di sini supaya UI menampilkan "Terputus" dengan tombol Sambungkan hidup,
    // bukan "Tersambung, menunggu gambar" selamanya.
    // Kabel tercabut TIDAK terlihat dari socket: terukur di hardware, goggles
    // yang tersambung tapi tidak mengirim gambar juga menghasilkan timeout
    // murni - nol paket, nol reset. Jadi sunyi tidak bisa dipakai membedakan
    // keduanya, dan pengatur waktu berbasis sunyi akan mematikan sesi yang sah.
    // Yang definitif adalah hilangnya alamat lokal: itu OS yang memberi tahu
    // perangkatnya sudah dilepas. Hanya berlaku di jalur adapter OS - pemilik
    // V1/V2 tidak pernah punya alamat itu, dan jalur RNDIS user space di macOS
    // juga tidak: di sana alamatnya hidup di dalam proses ini, jadi memeriksa
    // bind akan MEMBUNUH pipa yang sehat satu detik setelah lahir.
    if guard.as_ref().is_some_and(|p| {
        p.perlu_adapter && std::net::UdpSocket::bind((s.local.as_str(), 0)).is_err()
    }) {
        guard.take();
        *state.sample.lock() = None;
    }

    if guard.as_ref().is_some_and(|p| p.mati()) {
        guard.take();
        *state.sample.lock() = None;
    }

    let Some(p) = guard.as_ref() else {
        return Stats {
            adapter: adapter_state(&state, &s, true),
            ..Default::default()
        };
    };
    let c = &p.counters;
    let (bytes, decoded) = (c.bytes.load(Relaxed), c.decoded.load(Relaxed));

    // Jendela minimal 0,5 detik: di bawah itu fps memantul-mantul tak berguna.
    let mut samp = state.sample.lock();
    let (mbps, fps) = match samp.as_mut() {
        Some(prev) => {
            let dt = prev.at.elapsed().as_secs_f64();
            if dt >= 0.5 {
                prev.mbps = (bytes - prev.bytes) as f64 * 8.0 / dt / 1e6;
                prev.fps = (decoded - prev.decoded) as f64 / dt;
                prev.at = Instant::now();
                prev.bytes = bytes;
                prev.decoded = decoded;
            }
            (prev.mbps, prev.fps)
        }
        None => {
            *samp = Some(Sample { at: Instant::now(), bytes, decoded, mbps: 0.0, fps: 0.0 });
            (0.0, 0.0)
        }
    };

    let (pkts, lost) = (c.pkts.load(Relaxed), c.lost.load(Relaxed));
    Stats {
        running: true,
        port: p.port,
        width: c.width.load(Relaxed) as u32,
        height: c.height.load(Relaxed) as u32,
        mbps,
        fps,
        loss: if pkts + lost == 0 { 0.0 } else { 100.0 * lost as f64 / (pkts + lost) as f64 },
        pkts,
        lost,
        dropped: c.dropped.load(Relaxed),
        decoded,
        restarts: c.restarts.load(Relaxed),
        corrupt: c.corrupt.load(Relaxed),
        adapter: adapter_cached(&state, &s),
        recording: p.recording(),
        rec_bytes: c.rec_bytes.load(Relaxed),
    }
}

#[tauri::command]
fn log_tail(state: State<'_, AppState>) -> Vec<String> {
    match state.pipe.lock().as_ref() {
        Some(p) => p.log.lock().clone(),
        None => Vec::new(),
    }
}

/// Rekam aliran H.264 mentah apa adanya - nol re-encode, nol beban CPU.
/// Hasilnya bisa diputar VLC/mpv, atau dibungkus ke mp4 tanpa transcode:
///   ffmpeg -i rekaman.h264 -c copy rekaman.mp4
#[tauri::command]
fn toggle_record(app: AppHandle, state: State<'_, AppState>) -> Result<Option<String>, String> {
    let guard = state.pipe.lock();
    let p = guard.as_ref().ok_or("belum tersambung")?;
    if p.recording() {
        p.record_stop();
        return Ok(None);
    }
    let dir = {
        let s = state.settings.lock();
        if s.rec_dir.is_empty() {
            default_rec_dir(&app)
        } else {
            PathBuf::from(&s.rec_dir)
        }
    };
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("zerofuss-{}.h264", stamp()));
    p.record_start(&path).map_err(|e| e.to_string())?;
    Ok(Some(path.to_string_lossy().into()))
}

#[tauri::command]
fn open_rec_dir(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let dir = {
        let s = state.settings.lock();
        if s.rec_dir.is_empty() { default_rec_dir(&app) } else { PathBuf::from(&s.rec_dir) }
    };
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    tauri_plugin_opener::OpenerExt::opener(&app)
        .open_path(dir.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// Pasang IP statis di adapter RNDIS goggles. Butuh hak admin, jadi dijalankan
/// sebagai skrip terangkat - inilah satu langkah yang menghalangi pemakaian
/// pertama kali, karena goggles tidak melayani DHCP.
///
/// `cfg!`, BUKAN `#[cfg]`, di seluruh percabangan OS di berkas ini - dan itu
/// keputusan verifikasi, bukan gaya. Isi ketiga cabang di bawah semuanya kode
/// portabel (bikin string, tulis berkas sementara, jalankan proses), jadi dengan
/// `cfg!` compiler memeriksa KETIGANYA di setiap host. Dengan `#[cfg]`, cabang
/// macOS tidak pernah dilihat compiler di mesin Windows tempat aplikasi ini
/// dikembangkan: salah tulis di sana baru ketahuan saat ada Mac di tangan.
/// Cabang yang tidak terpakai dibuang optimizer; ini jalur dingin, ditekan
/// manusia sekali per pemakaian.
#[tauri::command]
fn setup_adapter(state: State<'_, AppState>) -> Result<String, String> {
    let s = state.settings.lock().clone();
    // Keadaan adapter berubah sesudah ini; buang cache supaya UI ikut.
    *state.adapter.lock() = None;

    // macOS: jangan menjanjikan apa pun. Recon N3Live mengukur bahwa macOS
    // MEMANG mengikat fungsi RNDIS goggles jadi antarmuka jaringan, tapi
    // link-nya tetap "media inactive" dan tidak pernah naik - jadi memasang
    // alamat statis di sana pun tidak akan menghasilkan satu paket pun.
    // HoRNDIS sudah tidak bisa dipakai sebagai jalan keluar: ia kext, dan kext
    // tidak berlaku lagi di Apple Silicon.
    //
    // Tombolnya sudah disembunyikan di UI (`sistem.rndis == false`), jadi pesan
    // ini jaring terakhir - bukan alur normal.
    if !usb::Os::HOST.rndis() {
        return Err("Goggles 3 / N3 lewat kabel belum jalan di macOS: macOS tidak punya \
                    host RNDIS yang berfungsi, jadi tidak ada adapter yang bisa disetel. \
                    Yang jalan di Mac adalah FPV Goggles V1/V2 - tanpa driver, tinggal \
                    colok. Untuk Goggles 3 / N3, pakai Windows atau Linux."
            .into());
    }
    if !cfg!(windows) {
        return Ok(format!("Set alamat manual {}/24 di adapter RNDIS goggles.", s.local));
    }

    let script = format!(
        r#"$a = Get-NetAdapter | Where-Object {{ $_.InterfaceDescription -like '*Remote NDIS*' }} | Select-Object -First 1
if (-not $a) {{ Write-Host 'Adapter goggles tidak ditemukan. Colok USB-C dan nyalakan OTG Wired Connection.'; Start-Sleep 6; exit 1 }}
Get-NetIPAddress -InterfaceIndex $a.ifIndex -AddressFamily IPv4 -ErrorAction SilentlyContinue |
  Where-Object PrefixOrigin -eq 'Manual' | Remove-NetIPAddress -Confirm:$false
New-NetIPAddress -InterfaceIndex $a.ifIndex -IPAddress {ip} -PrefixLength 24 | Out-Null
Write-Host "Terpasang {ip}/24 di $($a.Name)"; Start-Sleep 3"#,
        ip = s.local
    );
    jalankan_terangkat("zerofuss-adapter.ps1", &script)?;
    Ok("Setujui prompt admin, lalu tekan Sambungkan.".into())
}

/// Kembalikan adapter goggles ke DHCP otomatis - pasangan dari setup_adapter,
/// supaya IP statis tidak tertinggal di mesin setelah goggles dilepas.
#[tauri::command]
fn cleanup_adapter(state: State<'_, AppState>) -> Result<String, String> {
    *state.adapter.lock() = None;

    // Tidak ada IP statis yang pernah dipasang Zerofuss di macOS, jadi tidak
    // ada apa pun untuk dibersihkan - dan mengaku sedang membersihkan sesuatu
    // yang tidak ada adalah pesan yang bohong.
    if !usb::Os::HOST.rndis() {
        return Ok("Tidak ada yang perlu dibersihkan: Zerofuss tidak pernah memasang \
                   alamat statis di macOS."
            .into());
    }
    if !cfg!(windows) {
        return Ok("Set IPv4 adapter RNDIS goggles kembali ke otomatis/DHCP.".into());
    }

    let script = r#"$a = Get-NetAdapter | Where-Object { $_.InterfaceDescription -like '*Remote NDIS*' } | Select-Object -First 1
if (-not $a) { Write-Host 'Adapter goggles tidak ditemukan.'; Start-Sleep 5; exit 1 }
Get-NetIPAddress -InterfaceIndex $a.ifIndex -AddressFamily IPv4 -ErrorAction SilentlyContinue |
  Where-Object PrefixOrigin -eq 'Manual' | Remove-NetIPAddress -Confirm:$false
Set-NetIPInterface -InterfaceIndex $a.ifIndex -AddressFamily IPv4 -Dhcp Enabled
Write-Host "IPv4 di $($a.Name) dikembalikan ke otomatis"; Start-Sleep 3"#;
    jalankan_terangkat("zerofuss-cleanup.ps1", script)?;
    Ok("Setujui prompt admin. IPv4 adapter kembali otomatis.".into())
}

/// Tulis skrip PowerShell ke folder sementara, lalu jalankan terangkat.
///
/// Dipisah karena setup dan cleanup memakai pola yang sama persis, dan pola itu
/// punya satu detail yang mudah salah: yang diangkat adalah `Start-Process ...
/// -Verb RunAs` di dalam PowerShell tanpa jendela, bukan skripnya langsung -
/// tanpa perantara itu tidak ada prompt UAC sama sekali.
fn jalankan_terangkat(nama: &str, script: &str) -> Result<(), String> {
    let path = std::env::temp_dir().join(nama);
    std::fs::write(&path, script).map_err(|e| e.to_string())?;
    let arg = format!(
        "Start-Process powershell -Verb RunAs -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','{}'",
        path.display()
    );
    run_hidden("powershell", &["-NoProfile", "-Command", &arg]).map_err(|e| e.to_string())
}

/// Identitas adapter goggles: nama, deskripsi, MAC, IPv4.
///
/// Kosong berarti tidak ada adapter RNDIS sama sekali: goggles belum tercolok,
/// atau OTG Wired Connection belum dinyalakan.
///
/// Memakai `ipconfig /all`, BUKAN `Get-NetAdapter`. Alasannya terukur:
/// `Get-NetAdapter` itu panggilan CIM/WMI dan butuh **3,6 detik** per
/// pemanggilan di mesin uji - dengan cache 2 detik, probe-nya bertumpuk dan
/// memakan lebih dari satu inti penuh selama layar kosong tampil. `ipconfig`
/// mengembalikan data yang sama dalam **37 ms**.
///
/// Yang dicocokkan adalah NILAI (`Remote NDIS`, pola MAC, pola dotted-quad),
/// bukan label barisnya - label `ipconfig` diterjemahkan mengikuti bahasa
/// Windows, nilainya tidak. Itu sekalian membuat parsernya tahan lokalisasi.
fn probe_adapter(local: &str, pid_rndis: u16) -> Vec<String> {
    if cfg!(windows) {
        return capture("ipconfig", &["/all"]).map(|o| urai_ipconfig(&o)).unwrap_or_default();
    }
    // Di jalur RNDIS user space (macOS) TIDAK ADA adapter jaringan untuk
    // dilaporkan - itu intinya. Yang berguna di kotak identitas Setelan justru
    // identitas perangkatnya: nama produk dan serial yang dilaporkan USB, plus
    // alamat yang dipakai tumpukan di dalam proses. Kosong berarti goggles
    // belum tercolok atau OTG Wired Connection belum menyala - kesimpulan yang
    // sama dengan adapter yang tidak muncul di Windows.
    if usb::Os::HOST.rndis_usb() {
        return usb::identitas(pid_rndis)
            .map(|(produk, serial)| {
                vec![produk, "USB langsung, tanpa adapter".to_string(), serial, local.to_string()]
            })
            .unwrap_or_default();
    }
    Vec::new()
}

/// Ambil blok adapter yang deskripsinya menyebut `Remote NDIS`, lalu keluarkan
/// `[nama, deskripsi, MAC, IPv4]`. Kosong kalau adapternya tidak ada.
///
/// Dipisah dari pemanggilan proses supaya bisa diuji: pemindaian bloknya cukup
/// berliku untuk salah tanpa diketahui, dan kalau ia salah, UI akan bilang
/// "goggles belum terdeteksi" padahal goggles sedang tercolok - kebingungan
/// yang paling mahal di seluruh aplikasi ini.
fn urai_ipconfig(keluaran: &str) -> Vec<String> {
    let nilai = |l: &str| l.rsplit(':').next().unwrap_or("").trim().to_string();

    // Blok adapter dimulai pada baris tanpa indentasi yang berakhir ':'.
    let mut nama = String::new();
    let mut blok: Vec<&str> = Vec::new();
    let mut hasil: Option<(String, Vec<&str>)> = None;
    for baris in keluaran.lines() {
        let kosong = baris.trim().is_empty();
        if !kosong && !baris.starts_with(' ') && !baris.starts_with('\t') {
            if blok.iter().any(|l| l.contains("Remote NDIS")) {
                hasil = Some((nama.clone(), std::mem::take(&mut blok)));
                break;
            }
            nama = baris.trim().trim_end_matches(':').to_string();
            blok.clear();
        } else if !kosong {
            blok.push(baris);
        }
    }
    // Blok terakhir tidak diikuti header, jadi diperiksa terpisah.
    if hasil.is_none() && blok.iter().any(|l| l.contains("Remote NDIS")) {
        hasil = Some((nama, blok));
    }
    let Some((nama, blok)) = hasil else {
        return Vec::new();
    };

    let deskripsi = blok
        .iter()
        .find(|l| l.contains("Remote NDIS"))
        .map(|l| nilai(l))
        .unwrap_or_default();
    // MAC dikenali dari bentuknya (6 oktet dipisah '-'), bukan dari labelnya.
    let mac = blok
        .iter()
        .map(|l| nilai(l))
        .find(|v| v.len() == 17 && v.split('-').count() == 6)
        .unwrap_or_default();
    // Dotted-quad pertama di dalam blok adalah alamatnya: label sebelum itu
    // semuanya Yes/No, dan subnet mask selalu sesudahnya.
    let ip = blok
        .iter()
        .filter_map(|l| {
            let v = nilai(l);
            let v = v.split('(').next().unwrap_or("").trim();
            let empat = v.split('.').count() == 4
                && v.split('.').all(|o| !o.is_empty() && o.chars().all(|c| c.is_ascii_digit()));
            empat.then(|| v.to_string())
        })
        .next()
        .unwrap_or_default();

    vec![nama, deskripsi, mac, ip]
}

/// Satu potret keadaan perangkat. Kedua probe di dalamnya memanggil sistem -
/// `ipconfig` 37 ms, enumerasi USB 20 ms - jadi keduanya diambil bersamaan dan
/// disimpan bersamaan. Memisahkannya berarti dua cache yang bisa tidak sinkron.
struct Probe {
    /// Identitas adapter RNDIS: nama, deskripsi, MAC, IPv4. Kosong = tidak ada.
    adapter: Vec<String>,
    /// Keluarga goggles yang terlihat di USB.
    usb: usb::Found,
}

/// `paksa` dipakai tepat setelah Setel adapter / Bersihkan, supaya UI tidak
/// menampilkan keadaan basi selama beberapa detik.
fn probe_cached(state: &AppState, s: &Settings, paksa: bool, boleh_klaim: bool) -> Probe {
    const TTL: Duration = Duration::from_secs(2);
    let mut c = state.adapter.lock();
    if !paksa {
        if let Some((at, p)) = c.as_ref() {
            if at.elapsed() < TTL {
                return Probe { adapter: p.adapter.clone(), usb: p.usb };
            }
        }
    }
    let p = Probe {
        adapter: probe_adapter(&s.local, usb::PID_RNDIS),
        usb: usb::probe(s.usb_pid, boleh_klaim),
    };
    *c = Some((Instant::now(), Probe { adapter: p.adapter.clone(), usb: p.usb }));
    p
}

#[tauri::command]
fn adapter_info(state: State<'_, AppState>, paksa: bool) -> Vec<String> {
    let s = state.settings.lock().clone();
    // Aman di sini: perintah ini tidak memegang kunci pipa apa pun.
    let boleh = state.pipe.lock().is_none();
    probe_cached(&state, &s, paksa, boleh).adapter
}

/// Keadaan perangkat untuk UI. Lima keadaan, karena tindakan yang berguna
/// berbeda di masing-masing - dan tombol yang hidup tapi pasti gagal adalah
/// jebakan.
///
/// Jalur cepatnya penting: kalau alamat lokal sudah bisa di-bind, adapter RNDIS
/// jelas ada dan sudah bersetel - tidak perlu memanggil proses apa pun. Jadi
/// selama pemakaian normal (streaming) tidak ada probe yang jalan sama sekali.
/// Pemilik V1/V2 tidak punya adapter itu, jadi jalur cepat ini tidak pernah
/// salah mengaku atas nama mereka.
/// `boleh_klaim` DIBERIKAN pemanggil, tidak dihitung di sini - dan itu bukan
/// gaya. `stats` memanggil fungsi ini sambil memegang `state.pipe.lock()`;
/// mengambil kunci yang sama di dalam sini membekukan `stats` selamanya, karena
/// `parking_lot::Mutex` tidak reentrant. Gejalanya menyesatkan: jendela tetap
/// muncul (perintah Tauri jalan di utas worker, jadi utas utama sehat dan
/// pengaman splash 6 detik tetap bekerja) tapi UI mati total tanpa satu pun
/// pesan galat. Jangan menyentuh `state.pipe` dari sini.
fn adapter_state(state: &AppState, s: &Settings, boleh_klaim: bool) -> String {
    if std::net::UdpSocket::bind((s.local.as_str(), 0)).is_ok() {
        return "siap".into();
    }
    let p = probe_cached(state, s, false, boleh_klaim);
    nama_keadaan(p.usb, !p.adapter.is_empty(), usb::Os::HOST.rndis_usb()).into()
}

/// Keadaan adapter selagi pipa HIDUP: jalur cepat saja, lalu nilai cache apa
/// adanya - tanpa pernah memicu probe baru.
///
/// Dipisah karena janji di `adapter_state` ("selama streaming tidak ada probe
/// yang jalan") hanya benar di jalur RNDIS, yang lolos di bind. Pemilik V1/V2
/// tidak punya adapter `192.168.60.1`, jadi bind-nya SELALU gagal dan `stats`
/// yang dipanggil 4x/detik akan melahirkan `ipconfig /all` tiap kali TTL
/// habis - sepanjang penerbangan, sambil kunci pipa dipegang.
///
/// Keadaan basi tidak merugikan di sini: selagi streaming, satu-satunya yang
/// dilakukan UI dengan nilai ini adalah memilih teks, dan tombol yang
/// bergantung padanya sudah tidak bisa ditekan.
fn adapter_cached(state: &AppState, s: &Settings) -> String {
    if std::net::UdpSocket::bind((s.local.as_str(), 0)).is_ok() {
        return "siap".into();
    }
    match state.adapter.lock().as_ref() {
        Some((_, p)) => {
            nama_keadaan(p.usb, !p.adapter.is_empty(), usb::Os::HOST.rndis_usb()).into()
        }
        None => "tidak-ada".into(),
    }
}

/// Pemetaan hasil probe -> nama keadaan. Dipisah karena ia yang menentukan
/// tombol mana yang hidup, dan itu satu-satunya bagian yang bisa diuji tanpa
/// hardware. Nama-namanya adalah kontrak dengan `main.js`; ada tes yang
/// memastikan tabel di sana memuat semuanya.
///
/// `jalur_usb` datang dari `usb::Os::HOST.rndis_usb()` dan diberikan pemanggil,
/// tidak dibaca di sini, supaya keenam keadaan bisa diuji untuk KEDUA jenis OS
/// dari satu mesin.
fn nama_keadaan(usb: usb::Found, ada_adapter: bool, jalur_usb: bool) -> &'static str {
    match usb {
        // Jalur USB bulk menang atas keadaan adapter: pemilik V1/V2 tidak punya
        // adapter RNDIS, jadi mengabarkan keadaan adapter ke mereka hanya
        // mengirim mereka memasang IP statis yang tidak akan dipakai apa pun.
        usb::Found::Bulk => "bulk-siap",
        usb::Found::BulkNoDriver => "bulk-driver",
        // Goggles 3 / N3 terlihat di USB, dan di OS ini jalurnya user space
        // (macOS): tidak ada adapter maupun alamat yang perlu disetel, jadi
        // satu-satunya langkah berikutnya adalah menekan Sambungkan. Keadaan
        // tersendiri karena tindakan penggunanya berbeda dari "belum-ip" -
        // di sana masih ada satu langkah admin yang harus dikerjakan.
        usb::Found::Rndis if jalur_usb => "rndis-usb-siap",
        usb::Found::Rndis | usb::Found::None if !ada_adapter => "tidak-ada",
        _ => "belum-ip",
    }
}

/// Kemampuan mesin ini. Diambil webview sekali saat boot, dan jadi SATU-SATUNYA
/// sumber pengetahuan OS di sisi UI.
///
/// Tanpa ini `main.js` harus menebak lewat `navigator.userAgent` - yang tidak
/// tahu apa pun soal driver maupun RNDIS, dan yang akan membuat pengguna Mac
/// disuruh menjalankan Zadig (program Windows) atau menekan "Setel adapter"
/// yang pasti gagal. Pengetahuan itu ada di Rust; kirim jawabannya, jangan
/// biarkan UI menyimpulkan sendiri.
#[derive(Serialize)]
struct Sistem {
    /// "windows" | "macos" | "linux".
    os: &'static str,
    /// Keluarga Goggles 3 / N3 didukung di OS ini. Benar di ketiganya.
    rndis: bool,
    /// Jalur Goggles 3 / N3 di sini lewat host RNDIS user space (macOS): tanpa
    /// adapter, tanpa alamat statis, tanpa admin.
    rndis_usb: bool,
    /// Tombol Setel adapter / Bersihkan bermakna di OS ini.
    setel_adapter: bool,
    /// Jalur bulk (V1/V2) butuh pemasangan driver manual di OS ini.
    perlu_driver: bool,
    /// Decoder sudah ada di mesin ini. `false` berarti gambar tidak akan pernah
    /// muncul sampai ffmpeg dipasang - dan itu harus dikatakan SEBELUM tombol
    /// Sambungkan ditekan, bukan sebagai galat sesudahnya.
    ffmpeg: bool,
}

#[tauri::command]
fn sistem(app: AppHandle) -> Sistem {
    let os = usb::Os::HOST;
    Sistem {
        os: os.nama(),
        rndis: os.rndis(),
        rndis_usb: os.rndis_usb(),
        setel_adapter: os.setel_adapter(),
        perlu_driver: os.perlu_driver(),
        ffmpeg: find_ffmpeg(&app).is_some(),
    }
}

/// Dipanggil frontend setelah gambar pertama siap: tutup splash, tampilkan
/// jendela utama. Ada juga pengaman berbasis waktu di `run()` supaya jendela
/// tetap muncul walau frontend gagal memanggil ini.
#[tauri::command]
fn app_ready(app: AppHandle) {
    reveal_main(&app);
}

/// Membuka berkas lisensi/pemberitahuan yang ikut terpasang.
///
/// Namanya datang dari webview, jadi wajib lewat daftar putih - tanpa itu
/// perintah ini jadi jalan membuka berkas apa pun di mesin pengguna.
#[tauri::command]
fn open_doc(app: AppHandle, name: String) -> Result<(), String> {
    if !matches!(name.as_str(), "LICENSE.txt" | "THIRD-PARTY-NOTICES.txt") {
        return Err("berkas tidak dikenal".into());
    }
    let p = app.path().resource_dir().map_err(|e| e.to_string())?.join(name);
    tauri_plugin_opener::OpenerExt::opener(&app)
        .open_path(p.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

// ------------------------------------------------------------------- penolong

fn stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d-%H%M%S").to_string()
}

/// Folder rekaman bawaan: `Videos\Zerofuss` di Windows, `~/Movies/Zerofuss` di
/// macOS - `video_dir()` sudah memetakannya per OS - dengan home sebagai jaring
/// kalau sistem tidak melaporkan folder itu.
fn default_rec_dir(app: &AppHandle) -> PathBuf {
    app.path()
        .video_dir()
        .or_else(|_| app.path().home_dir())
        .map(|d| d.join("Zerofuss"))
        .unwrap_or_else(|_| PathBuf::from("Zerofuss"))
}

/// Urutan pencarian ffmpeg: env, sebelah exe, folder resource bundle, lokasi
/// baku Homebrew di macOS, lalu PATH.
fn find_ffmpeg(app: &AppHandle) -> Option<PathBuf> {
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    if let Ok(p) = std::env::var("ZEROFUSS_FFMPEG") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut cands = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            cands.push(d.join(name));
            cands.push(d.join("ffmpeg").join(name));
        }
    }
    if let Ok(d) = app.path().resource_dir() {
        cands.push(d.join(name));
    }
    // macOS: aplikasi yang dibuka dari Finder mewarisi PATH minimal
    // (`/usr/bin:/bin:/usr/sbin:/sbin`), BUKAN PATH shell pengguna. Jadi ffmpeg
    // hasil `brew install ffmpeg` tidak akan pernah ketemu lewat jalur PATH di
    // bawah, walau `which ffmpeg` di Terminal menemukannya - kegagalan yang
    // terlihat seperti aplikasinya rusak. Kedua lokasi baku Homebrew karena itu
    // disebut langsung.
    if cfg!(target_os = "macos") {
        cands.push(PathBuf::from("/opt/homebrew/bin/ffmpeg")); // Apple Silicon
        cands.push(PathBuf::from("/usr/local/bin/ffmpeg")); // Intel
    }
    cands.into_iter().find(|p| p.is_file()).or_else(|| {
        // Terakhir: percaya PATH.
        run_hidden(name, &["-version"]).ok().map(|_| PathBuf::from(name))
    })
}

fn run_hidden(exe: &str, args: &[&str]) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.status().map(|_| ())
}

pub(crate) fn capture(exe: &str, args: &[&str]) -> std::io::Result<String> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args).stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output()?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Tutup splash dan tampilkan jendela utama. Aman dipanggil berkali-kali.
fn reveal_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("splash") {
        let _ = w.close();
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn settings_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("settings.json"))
}

fn load_settings(app: &AppHandle) -> Settings {
    settings_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_settings(app: &AppHandle, s: &Settings) -> std::io::Result<()> {
    let Some(p) = settings_path(app) else { return Ok(()) };
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(p, serde_json::to_string_pretty(s).unwrap_or_default())
}

/// Diagnostik jalur Goggles 3 / N3 user space, dijalankan dari terminal lewat
/// `--uji-rndis`. Mengembalikan kode keluar proses.
///
/// Ini satu-satunya cara membuktikan jalur itu di hardware, dan ia sengaja
/// mencetak tiap tahap secara terpisah: kalau ada yang gagal, yang penting
/// bukan "gagal" - tapi tahap KEBERAPA. Klaim interface, INITIALIZE, MAC
/// perangkat, filter paket, ARP, paket open DJI, dan byte video pertama adalah
/// tujuh kegagalan dengan tujuh tindakan yang sama sekali berbeda.
pub fn uji_rndis() -> i32 {
    let os = usb::Os::HOST;
    println!("Zerofuss - uji jalur RNDIS user space (Goggles 3 / N3)");
    println!("  OS               : {}", os.nama());
    println!("  jalur user space : {}", if os.rndis_usb() { "ya" } else { "TIDAK" });
    if !os.rndis_usb() {
        println!(
            "\nDi OS ini kernel yang memegang interface RNDIS, jadi jalur user space\n\
             pasti ditolak. Yang dipakai di sini adalah adapter jaringan + alamat\n\
             statis - uji lewat aplikasinya, bukan lewat flag ini."
        );
        return 2;
    }

    print!("  klaim + INITIALIZE ... ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut tautan = match rndis::Tautan::buka(usb::PID_RNDIS) {
        Ok(t) => {
            println!("OK");
            t
        }
        Err(e) => {
            println!("GAGAL\n\n  {e}");
            return 1;
        }
    };
    let mac = tautan.mac_perangkat;
    println!(
        "  MAC perangkat    : {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );
    println!(
        "  INITIALIZE_CMPLT : max transfer {} B, max paket/transfer {}, alignment {}, medium {}",
        tautan.max_transfer, tautan.max_paket, tautan.alignment, tautan.medium
    );

    let s = Settings::default();
    let (Ok(ip_kita), Ok(ip_peer)) = (s.local.parse(), s.goggles.parse()) else {
        println!("  alamat di settings.json tidak sah");
        return 1;
    };
    let mut tumpukan = net::Tumpukan::baru(ip_kita, ip_peer, 40001);
    tumpukan.duga_mac_peer(mac);

    // ARP: buktikan lalu lintas benar-benar bolak-balik, bukan cuma keluar.
    print!("  ARP {} ... ", s.goggles);
    let _ = std::io::Write::flush(&mut std::io::stdout());
    // `dijawab` HARUS datang dari `Masuk::MacPeer`, bukan dari membandingkan MAC
    // dengan yang dilaporkan RNDIS. Terukur di Goggles 3: MAC Ethernet-nya
    // SAMA dengan MAC fungsi RNDIS (memang satu interface), jadi perbandingan
    // itu melaporkan "tidak dijawab" walau goggles menjawab.
    let mut dijawab = false;
    for _ in 0..10 {
        let _ = tautan.kirim_frame(&tumpukan.arp_minta());
        let mut balas = Vec::new();
        let _ = tautan.terima(std::time::Duration::from_millis(200), |eth| {
            match tumpukan.telan(eth) {
                net::Masuk::Balas(f) => balas.push(f),
                net::Masuk::MacPeer => dijawab = true,
                _ => {}
            }
        });
        for f in balas {
            let _ = tautan.kirim_frame(&f);
        }
        if dijawab {
            break;
        }
    }
    println!(
        "{}",
        if dijawab {
            "dijawab"
        } else {
            "tidak dijawab - lanjut dengan MAC dari RNDIS (ini normal, video tetap jalan)"
        }
    );

    // Tautan diagnostik DILEPAS sebelum pipa dibuka, dan itu wajib: satu
    // interface USB hanya boleh dipegang satu pemilik, jadi membiarkannya hidup
    // membuat `Pipeline::start` gagal dengan "sedang dipegang program lain" -
    // program lain yang dimaksud adalah alat uji ini sendiri. Versi pertama
    // melepasnya SESUDAH `start`, dan justru itu yang terlihat di Mac
    // pertama kali dijalankan.
    drop(tautan);

    // Paket open DJI, lalu hitung apa yang datang selama 5 detik.
    print!("  video 5 detik    ... ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let pipe = match Pipeline::start(Config {
        source: Source::RndisUsb {
            pid: usb::PID_RNDIS,
            goggles: s.goggles.clone(),
            goggles_port: s.goggles_port,
            local: s.local.clone(),
        },
        ffmpeg: std::env::var("ZEROFUSS_FFMPEG").unwrap_or_else(|_| "ffmpeg".into()).into(),
    }) {
        Ok(p) => p,
        Err(e) => {
            println!("GAGAL membuka pipa\n\n  {e}");
            println!(
                "\nKalau pesannya soal interface dipegang program lain: TUTUP jendela\n\
                 Zerofuss (termasuk `bun run tauri dev`) dulu. Aplikasi dan alat uji\n\
                 ini tidak bisa memegang interface RNDIS yang sama bersamaan."
            );
            return 1;
        }
    };
    // Dua jendela, bukan satu total. Alasannya terukur di jalur ini: stream
    // goggles praktis tanpa keyframe, jadi decoder dipaksa mengeluarkan gambar
    // sebelum keyframe (`+showall`) dan frame-frame pertama MEMANG korup sampai
    // satu putaran intra-refresh selesai (~4 detik, sekali saja). Total korup
    // apa adanya karena itu tidak bisa membedakan "normal saat mulai" dari "ada
    // yang salah" - yang membedakan adalah apakah ia MASIH naik di jendela
    // kedua.
    use std::sync::atomic::Ordering::Relaxed;
    let c = &pipe.counters;
    std::thread::sleep(std::time::Duration::from_secs(5));
    let (p1, b1, k1, d1) = (
        c.pkts.load(Relaxed),
        c.bytes.load(Relaxed),
        c.corrupt.load(Relaxed),
        c.decoded.load(Relaxed),
    );
    std::thread::sleep(std::time::Duration::from_secs(3));
    let (p2, b2, k2, d2) = (
        c.pkts.load(Relaxed),
        c.bytes.load(Relaxed),
        c.corrupt.load(Relaxed),
        c.decoded.load(Relaxed),
    );

    // Laju diambil dari jendela KEDUA: lima detik pertama memuat pembukaan
    // sesi (open, ARP, decoder menyusul), jadi rata-rata di sana selalu lebih
    // rendah daripada laju sesi yang sudah mapan.
    let mbps = ((b2 - b1) as f64 * 8.0) / 3.0 / 1e6;
    let fps = (d2 - d1) as f64 / 3.0;
    let pps = (p2 - p1) as f64 / 3.0;
    println!(
        "{p2} paket, {} KB, {} hilang, {} dibuang",
        b2 / 1024,
        c.lost.load(Relaxed),
        c.dropped.load(Relaxed)
    );
    println!("  laju mapan       : {mbps:.1} Mbps, {pps:.0} paket/detik");
    println!(
        "  gambar           : {}x{}, {:.0} frame/detik terdekode, {} restart decoder",
        c.width.load(Relaxed),
        c.height.load(Relaxed),
        fps,
        c.restarts.load(Relaxed)
    );
    println!(
        "  frame korup      : {k1} di 5 detik pertama, +{} di 3 detik berikutnya",
        k2 - k1
    );

    if p1 == 0 {
        println!(
            "\nNol paket video. Yang harus diperiksa, berurutan:\n\
             - LiveView Sharing menyala di goggles (5D ke bawah > Shortcut Menu)\n\
             - air unit / drone hidup dan sudah ter-link (tanpa gambar, goggles diam)\n\
             - Settings > About > OTG Wired Connection = ON"
        );
        return 1;
    }
    if d2 == 0 {
        println!(
            "\nPaket masuk tapi NOL frame terdekode. Jalur USB-nya sehat; yang bermasalah\n\
             decoder. Periksa ffmpeg yang dipakai (ZEROFUSS_FFMPEG) - lihat baris log di\n\
             bawah."
        );
        for baris in pipe.log.lock().iter().rev().take(10) {
            println!("    {baris}");
        }
        return 1;
    }

    println!("\nJalur RNDIS user space BEKERJA di mesin ini.");

    // Putusan soal frame korup HARUS bersandar pada dua hal sekaligus: laju
    // korupnya, dan apakah jalur USB kehilangan sesuatu. Kalau `hilang` dan
    // `dibuang` dua-duanya nol, maka byte yang diterima decoder SAMA PERSIS
    // dengan yang dikirim goggles - jadi kerusakan apa pun yang tersisa lahir
    // di HULU jalur ini: di tautan radio air unit -> goggles, atau di
    // encoder-nya sendiri. Tidak ada satu pun perubahan di sisi Mac yang bisa
    // memperbaikinya, dan menyuruh pengguna mengirim log untuk itu hanya
    // membuang waktunya.
    let frame2 = (d2 - d1).max(1);
    let korup2 = k2 - k1;
    let persen = korup2 as f64 * 100.0 / frame2 as f64;
    let bersih = c.lost.load(Relaxed) == 0 && c.dropped.load(Relaxed) == 0;

    if korup2 == 0 {
        if k1 > 0 {
            println!(
                "Frame korup berhenti bertambah - itu satu putaran intra-refresh di awal,\n\
                 persis yang diharapkan dari stream tanpa keyframe. Gambarnya bersih."
            );
        }
    } else if bersih && persen < 5.0 {
        println!(
            "Frame korup: {korup2} dari {frame2} frame ({persen:.1}%) di jendela kedua.\n\
             Jalur USB-nya bersih - nol paket hilang, nol dibuang - jadi byte yang masuk\n\
             decoder sama persis dengan yang dikirim goggles. Sisa korup sebesar ini\n\
             datang dari HULU: tautan radio air unit -> goggles, bukan dari kabel atau\n\
             dari Mac. Buktinya bisa kamu lihat sendiri: angkanya berubah kalau drone\n\
             didekatkan/dijauhkan, dan tidak berubah kalau kabel USB-nya diganti."
        );
    } else {
        println!(
            "Frame korup {korup2} dari {frame2} frame ({persen:.1}%) di jendela kedua, dengan\n\
             {} paket hilang dan {} dibuang. Ini di luar pola intra-refresh - kirim baris\n\
             log di bawah.",
            c.lost.load(Relaxed),
            c.dropped.load(Relaxed)
        );
        for baris in pipe.log.lock().iter().rev().take(6) {
            println!("    {baris}");
        }
    }
    0
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .setup(|app| {
            let s = load_settings(app.handle());
            *app.state::<AppState>().settings.lock() = s;

            // Splash tidak boleh bisa menyandera aplikasi: kalau frontend gagal
            // memanggil app_ready, jendela utama tetap muncul setelah 6 detik.
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(6));
                reveal_main(&handle);
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            set_settings,
            start,
            stop,
            stats,
            sistem,
            log_tail,
            toggle_record,
            open_rec_dir,
            setup_adapter,
            cleanup_adapter,
            adapter_info,
            app_ready,
            open_doc,
        ])
        .run(tauri::generate_context!())
        .expect("gagal menjalankan Zerofuss");
}

#[cfg(test)]
mod tests {
    use super::{adapter_state, nama_keadaan, pilih_sumber, urai_ipconfig, AppState, Settings};
    use crate::pipeline::Source;
    use crate::usb::Found;

    /// Keluaran `ipconfig /all` Windows berbahasa Inggris, dipotong ke tiga
    /// adapter supaya jelas bahwa blok yang benar yang terpilih - bukan blok
    /// pertama, dan bukan blok yang kebetulan punya IPv4 lebih dulu.
    const INGGRIS: &str = "\
Windows IP Configuration

   Host Name . . . . . . . . . . . . : DESKTOP-TEST

Ethernet adapter Ethernet:

   Description . . . . . . . . . . . : Realtek PCIe GbE Family Controller
   Physical Address. . . . . . . . . : AA-BB-CC-DD-EE-01
   IPv4 Address. . . . . . . . . . . : 192.168.3.134(Preferred)
   Subnet Mask . . . . . . . . . . . : 255.255.255.0

Ethernet adapter Ethernet 3:

   Description . . . . . . . . . . . : Remote NDIS based Internet Sharing Device
   Physical Address. . . . . . . . . : 12-34-56-78-9A-BC
   DHCP Enabled. . . . . . . . . . . : No
   Autoconfiguration Enabled . . . . : Yes
   IPv4 Address. . . . . . . . . . . : 192.168.60.1(Preferred)
   Subnet Mask . . . . . . . . . . . : 255.255.255.0

Wireless LAN adapter Wi-Fi:

   Description . . . . . . . . . . . : Realtek 8852BE Wireless LAN WiFi 6 PCI-E NIC
   Physical Address. . . . . . . . . : AA-BB-CC-DD-EE-02
   IPv4 Address. . . . . . . . . . . : 10.0.0.5(Preferred)
";

    #[test]
    fn blok_rndis_terpilih_bukan_yang_pertama() {
        let v = urai_ipconfig(INGGRIS);
        assert_eq!(v, vec![
            "Ethernet adapter Ethernet 3",
            "Remote NDIS based Internet Sharing Device",
            "12-34-56-78-9A-BC",
            "192.168.60.1",
        ]);
    }

    /// Label `ipconfig` mengikuti bahasa Windows; nilainya tidak. Parser yang
    /// bergantung pada label akan diam-diam gagal di mesin non-Inggris - dan
    /// gagalnya terlihat seperti "goggles belum terdeteksi".
    #[test]
    fn label_terlokalisasi_tetap_terurai() {
        let id = INGGRIS
            .replace("Description . . . . . . . . . . . :", "Deskripsi . . . . . . . . . . . . :")
            .replace("Physical Address. . . . . . . . . :", "Alamat Fisik. . . . . . . . . . . :")
            .replace("IPv4 Address. . . . . . . . . . . :", "Alamat IPv4 . . . . . . . . . . . :")
            .replace("Subnet Mask . . . . . . . . . . . :", "Subnet Mask . . . . . . . . . . . :");
        assert_eq!(urai_ipconfig(&id)[3], "192.168.60.1");
        assert_eq!(urai_ipconfig(&id)[2], "12-34-56-78-9A-BC");
    }

    /// Adapter ada tapi belum diberi alamat - inilah keadaan "belum-ip", dan
    /// tombol Setel adapter harus hidup tepat di sini.
    #[test]
    fn adapter_tanpa_ip_tetap_terdeteksi() {
        let tanpa = INGGRIS.replace("   IPv4 Address. . . . . . . . . . . : 192.168.60.1(Preferred)\n", "")
                           .replace("   Subnet Mask . . . . . . . . . . . : 255.255.255.0\n\nWireless", "\nWireless");
        let v = urai_ipconfig(&tanpa);
        assert_eq!(v[1], "Remote NDIS based Internet Sharing Device");
        assert_eq!(v[3], "", "IP harus kosong, dapat: {}", v[3]);
    }

    /// Goggles tidak tercolok: kosong, dan itu yang membuat UI mematikan
    /// tombol Setel adapter alih-alih menawarkan prompt admin yang pasti gagal.
    #[test]
    fn tanpa_rndis_hasilnya_kosong() {
        let v = urai_ipconfig(&INGGRIS.replace("Remote NDIS based Internet Sharing Device", "Some Other NIC"));
        assert!(v.is_empty(), "dapat: {v:?}");
    }

    /// Blok RNDIS di paling akhir berkas tidak diikuti header - jalur yang
    /// mudah terlewat saat memindai blok.
    #[test]
    fn blok_rndis_di_akhir_berkas() {
        let akhir = "\
Ethernet adapter Ethernet:

   Description . . . . . . . . . . . : Realtek PCIe GbE Family Controller
   IPv4 Address. . . . . . . . . . . : 192.168.3.134(Preferred)

Ethernet adapter Ethernet 3:

   Description . . . . . . . . . . . : Remote NDIS based Internet Sharing Device
   Physical Address. . . . . . . . . : 12-34-56-78-9A-BC
   IPv4 Address. . . . . . . . . . . : 192.168.60.1(Preferred)
";
        assert_eq!(urai_ipconfig(akhir)[3], "192.168.60.1");
    }

    // ------------------------------------------------------ keadaan perangkat

    /// V1/V2 harus menang atas keadaan adapter. Kalau tidak, pemiliknya
    /// diarahkan memasang IP statis yang tidak akan dipakai apa pun - dan
    /// tombol yang benar-benar dia butuhkan tetap tersembunyi.
    ///
    /// Ini harus benar di KEDUA jenis OS: jalur bulk jalan di Windows maupun
    /// macOS, jadi tidak ada satu pun keadaan bulk yang boleh bergantung pada
    /// `rndis_ok`.
    #[test]
    fn bulk_menang_atas_keadaan_adapter() {
        for ada_adapter in [true, false] {
            for rndis_ok in [true, false] {
                assert_eq!(nama_keadaan(Found::Bulk, ada_adapter, rndis_ok), "bulk-siap");
                assert_eq!(
                    nama_keadaan(Found::BulkNoDriver, ada_adapter, rndis_ok),
                    "bulk-driver"
                );
            }
        }
    }

    /// Driver belum terpasang harus jadi keadaan TERSENDIRI. Menyamakannya
    /// dengan "siap" memberi tombol Sambungkan yang pasti gagal; menyamakannya
    /// dengan "tidak terdeteksi" menyuruh orang memeriksa kabel yang sudah benar.
    #[test]
    fn driver_belum_ada_bukan_siap_dan_bukan_tidak_ada() {
        let s = nama_keadaan(Found::BulkNoDriver, false, true);
        assert_ne!(s, "bulk-siap");
        assert_ne!(s, "tidak-ada");
    }

    #[test]
    fn jalur_adapter_os_tidak_berubah() {
        assert_eq!(nama_keadaan(Found::None, false, false), "tidak-ada");
        assert_eq!(nama_keadaan(Found::None, true, false), "belum-ip");
        // Goggles 3 terlihat di USB tapi adapternya belum muncul: tetap
        // "tidak-ada", karena tindakan penggunanya sama - dan kami tidak punya
        // bukti cukup untuk menjanjikan diagnosis yang lebih tajam.
        assert_eq!(nama_keadaan(Found::Rndis, false, false), "tidak-ada");
        assert_eq!(nama_keadaan(Found::Rndis, true, false), "belum-ip");
    }

    /// Di OS berjalur user space (macOS), Goggles 3 yang tercolok langsung
    /// SIAP - tidak ada adapter maupun alamat yang perlu disetel, jadi
    /// keadaannya tidak boleh sama dengan "belum-ip" (yang berarti masih ada
    /// satu langkah admin) maupun "tidak-ada" (yang menyuruh periksa kabel).
    #[test]
    fn goggles3_di_jalur_user_space_langsung_siap() {
        assert_eq!(nama_keadaan(Found::Rndis, false, true), "rndis-usb-siap");
        // Keberadaan adapter tidak relevan di jalur ini: tumpukan IP-nya ada di
        // dalam proses, bukan di OS.
        assert_eq!(nama_keadaan(Found::Rndis, true, true), "rndis-usb-siap");
        // Tanpa goggles sama sekali, jawabannya tetap sama di semua OS.
        assert_eq!(nama_keadaan(Found::None, false, true), "tidak-ada");
    }

    /// Kontrak lintas bahasa. Keadaan yang dikirim Rust tapi tidak ada di tabel
    /// `main.js` membuat UI tersangkut di "memeriksa…" selamanya - kegagalan
    /// yang tidak akan terlihat di tes Rust mana pun, dan tidak akan menabrak
    /// apa pun. Jadi tabelnya diperiksa di sini.
    #[test]
    fn semua_keadaan_ada_di_tabel_ui() {
        const JS: &str = include_str!("../../src/main.js");
        let semua = [
            nama_keadaan(Found::Bulk, false, true),
            nama_keadaan(Found::BulkNoDriver, false, true),
            nama_keadaan(Found::None, false, true),
            nama_keadaan(Found::None, true, true),
            nama_keadaan(Found::Rndis, false, true),
            "siap", // jalur cepat di adapter_state, tidak lewat nama_keadaan
        ];
        for k in semua {
            // Kunci objek JS: `siap:` untuk pengenal biasa, `"bulk-siap":`
            // untuk yang bertanda hubung.
            let punya = JS.contains(&format!("\"{k}\":")) || JS.contains(&format!("\n    {k}:"));
            assert!(punya, "keadaan `{k}` tidak ada di tabel main.js");
            // Harus ada di KEDUA tabel: label status di panel, dan layar kosong.
            let jumlah = JS.matches(&format!("\"{k}\"")).count() + JS.matches(&format!("\n      {k}:")).count();
            assert!(jumlah >= 2, "keadaan `{k}` cuma muncul {jumlah}x di main.js");
        }
    }

    /// Kontrak kedua dengan `main.js`: perintah `sistem` adalah satu-satunya
    /// sumber pengetahuan OS di UI, jadi UI harus benar-benar memanggilnya.
    /// Kalau panggilannya hilang, seluruh percabangan per-OS mati diam-diam dan
    /// pengguna Mac kembali melihat petunjuk Windows.
    #[test]
    fn ui_memanggil_perintah_sistem() {
        const JS: &str = include_str!("../../src/main.js");
        assert!(JS.contains("invoke(\"sistem\")"), "main.js tidak memanggil perintah sistem");
        assert!(
            !JS.contains("navigator.platform") && !JS.contains("userAgent"),
            "UI menebak OS sendiri; pakai hasil perintah `sistem`"
        );
    }

    // ------------------------------------------------------- pemilihan jalur

    /// Salah arah di sini adalah jalan buntu: pemilik V1/V2 diminta memasang IP
    /// statis di adapter yang tidak pernah ada.
    #[test]
    fn v1v2_selalu_ke_jalur_bulk() {
        for ada in [Found::Bulk, Found::BulkNoDriver] {
            let s = Settings { usb_pid: 0x1f, ..Default::default() };
            assert_eq!(pilih_sumber(ada, s), Source::Bulk { pid: 0x1f }, "{ada:?}");
        }
    }

    /// Goggles 3 terlihat di USB TAPI harus tetap lewat RNDIS - magic RMVT sudah
    /// diuji dan ditolak firmware-nya, jadi mengarahkannya ke jalur bulk hanya
    /// menghasilkan layar hitam yang tidak pernah terisi.
    #[test]
    fn goggles3_dan_kosong_tetap_ke_jalur_rndis() {
        for ada in [Found::Rndis, Found::None] {
            let s = Settings::default();
            assert_eq!(
                pilih_sumber(ada, s),
                Source::Rndis {
                    goggles: "192.168.60.2".into(),
                    goggles_port: 9003,
                    local: "192.168.60.1".into(),
                },
                "{ada:?}"
            );
        }
    }

    /// Jalan keluar PID dipakai sungguhan, bukan cuma tersimpan: kalau ada V1
    /// dengan PID lain, mengisinya di settings.json harus benar-benar berpengaruh.
    #[test]
    fn pid_dari_settings_ikut_terpakai() {
        let s = Settings { usb_pid: 0x99, ..Default::default() };
        assert_eq!(pilih_sumber(Found::Bulk, s), Source::Bulk { pid: 0x99 });
    }

    // --------------------------------------------------------- kunci & jalur

    /// `stats` memanggil `adapter_state` SAMBIL memegang `state.pipe.lock()`.
    /// Kalau `adapter_state` ikut mengambil kunci itu, `parking_lot::Mutex` yang
    /// tidak reentrant membekukan perintah `stats` selamanya - dan gejalanya
    /// menyesatkan: jendela tetap muncul lewat pengaman splash 6 detik, tapi UI
    /// mati total tanpa satu pun pesan galat. Itu pernah terjadi; ini penjaganya.
    #[test]
    fn adapter_state_tidak_mengambil_kunci_pipa() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let st = AppState::default();
            // Alamat TEST-NET-3: dijamin tidak bisa di-bind, jadi jalur cepat
            // dilewati dan probe yang sebenarnya ikut teruji.
            let s = Settings { local: "203.0.113.99".into(), ..Default::default() };
            let _pegang = st.pipe.lock();
            let _ = adapter_state(&st, &s, true);
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok(),
            "adapter_state membeku sambil kunci pipa dipegang - ia mengambil kunci yang sama"
        );
    }
}
