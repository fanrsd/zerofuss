//! Jalur USB bulk untuk **DJI FPV Goggles V1 / V2** - keluarga yang lebih tua
//! dan protokolnya jauh lebih sederhana daripada jalur RNDIS Goggles 3.
//!
//! Seluruh percakapannya:
//!
//!   klaim interface 3  ->  tulis magic "RMVT" ke bulk OUT 0x03
//!                      ->  baca terus dari bulk IN 0x84
//!
//! Yang keluar dari bulk IN adalah H.264 Annex-B **mentah**: tidak ada header,
//! tidak ada nomor urut, tidak ada ACK, tidak ada sesi, dan tidak ada perintah
//! berhenti - menutup handle sudah cukup. Bandingkan dengan `pipeline.rs` yang
//! harus menjaga ACK tetap mengalir setiap 30 ms atau stream beku permanen.
//!
//! Dasar protokol dibaca dari implementasi yang sudah terbukti jalan di
//! hardware, bukan dari tebakan: `fpvout/live.fpvout.com` (`src/Goggles.ts`),
//! `fpv-wtf/voc-poc`, `fpvout/fpvout-cpp`, `fpvout/fpvout-c`, `fpvout/fpvout-go`,
//! dan `fpvout/DigiView-Android`. Ketujuhnya memakai angka yang sama persis.
//!
//! Pustakanya `nusb`, BUKAN `rusb`/libusb, dan itu keputusan lisensi: libusb
//! adalah LGPL-2.1 sementara crate binding-nya mengaku MIT, jadi menautnya
//! statis ke biner tertutup ini memicu kewajiban relink LGPL pasal 6. `nusb`
//! Apache-2.0/MIT, Rust murni - sekalian menghapus kompilasi C dan DLL titipan.

use nusb::transfer::{Bulk, In, Out};
use nusb::{DeviceInfo, MaybeFuture};
use std::time::Duration;

pub const VID: u16 = 0x2ca3;

/// FPV Goggles V1 & V2. Satu PID untuk keduanya.
///
/// Perlu dicatat sebagai inferensi, bukan fakta terverifikasi: tujuh proyek
/// open source hanya punya PID tunggal ini sambil menyatakan mendukung "V1 &
/// V2", tapi tidak satu pun dokumen menyebut PID V1 secara terpisah. Karena
/// itu `Settings::usb_pid` ada sebagai jalan keluar - kalau ada V1 dengan PID
/// lain, ia bisa dipakai tanpa membangun ulang biner.
pub const PID_BULK: u16 = 0x001f;

/// Goggles 3 dan Goggles N3 - keluarga RNDIS. Dikenali **hanya** supaya bisa
/// dibedakan, bukan untuk dilayani di sini.
///
/// Ini terukur di hardware kami sendiri, bukan dikutip: dengan WinUSB ter-bind
/// ke MI_03 sebuah Goggles 3, LiveView Sharing menyala, dan video sedang
/// mengalir lewat RNDIS di saat yang sama - `claim_interface(3)` berhasil,
/// descriptor endpoint-nya IDENTIK dengan V1/V2 (OUT 0x03 / IN 0x84, max 512),
/// tapi tulisan RMVT selalu `Cancelled` bahkan dengan timeout 3 detik dan
/// sesudah `clear_halt`, dan endpoint IN tidak pernah mengeluarkan satu byte
/// pun. Interface itu dorman. Lihat `uji-usb/README.txt` untuk angkanya.
///
/// Konsekuensinya untuk kode ini: deteksi keluarga HARUS bersandar pada PID,
/// bukan pada bentuk descriptor - keduanya tidak bisa dibedakan dari
/// descriptor-nya sama sekali.
pub const PID_RNDIS: u16 = 0x0020;

const IFACE: u8 = 3;
const EP_OUT: u8 = 0x03;
const EP_IN: u8 = 0x84;
const MAGIC: [u8; 4] = [0x52, 0x4d, 0x56, 0x54]; // "RMVT"

/// Ukuran satu transfer masuk. Kelipatan max packet (512 B) supaya tidak ada
/// potongan yang terbuang di ujung buffer.
const CHUNK: usize = 64 * 1024;

/// Banyak transfer yang dijaga tetap menggantung. Inilah alasan memakai antrean
/// daripada baca-tunggu-baca: di jeda antara satu bacaan selesai dan bacaan
/// berikutnya diajukan, host tidak menerima apa pun - dan goggles tidak
/// menunggu. Delapan transfer membuat jeda itu tidak pernah ada.
const PENDING: usize = 8;

/// Apa yang tercolok di port USB.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Found {
    /// Tidak ada goggles DJI sama sekali.
    None,
    /// V1/V2 terlihat, dan interface videonya sudah punya driver WinUSB.
    Bulk,
    /// V1/V2 terlihat, tapi interface videonya belum dipasangi WinUSB.
    /// Dibedakan dari `Bulk` karena tindakan penggunanya berbeda jauh - yang
    /// satu tekan Sambungkan, yang satu harus memasang driver dulu.
    BulkNoDriver,
    /// Goggles 3 / N3. Dilayani jalur RNDIS, bukan modul ini.
    Rndis,
}

/// Sistem operasi tempat biner ini jalan.
///
/// Enum, bukan `cfg!` telanjang di tiap tempat, karena dua hal di modul ini
/// berbeda per OS - kalimat galat dan perlu-tidaknya pemasangan driver - dan
/// keduanya harus tetap TERKOMPILASI dan TERUJI di mesin mana pun tesnya
/// dijalankan. Dengan `cfg!` telanjang, kalimat macOS tidak akan pernah
/// dilihat compiler di Windows, dan salah tulis di sana baru ketahuan setelah
/// ada Mac di tangan.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    Windows,
    MacOs,
    Linux,
}

impl Os {
    /// OS tempat biner ini dibangun.
    pub const HOST: Os = if cfg!(windows) {
        Os::Windows
    } else if cfg!(target_os = "macos") {
        Os::MacOs
    } else {
        Os::Linux
    };

    /// Apakah jalur bulk V1/V2 butuh pemasangan driver manual di OS ini.
    ///
    /// Hanya Windows. Interface 3 goggles berkelas vendor (0xFF), dan Windows
    /// tidak punya driver untuk kelas itu - jadi ia harus ditunjuk ke WinUSB
    /// lewat Zadig. macOS dan Linux membiarkan interface kelas vendor tanpa
    /// driver kernel, jadi ia bisa langsung diklaim dari user space; di sana
    /// keadaan "driver belum ada" tidak pernah ada.
    pub const fn perlu_driver(self) -> bool {
        matches!(self, Os::Windows)
    }

    /// Apakah keluarga Goggles 3 / N3 bisa dipakai di OS ini sama sekali.
    ///
    /// Ketiga OS: bisa - hanya jalurnya berbeda. Windows dan Linux memakai
    /// adapter RNDIS milik kernel; macOS memakai host RNDIS di user space
    /// (`crate::rndis`), karena di sana tidak ada driver RNDIS yang memegang
    /// interface-nya.
    pub const fn rndis(self) -> bool {
        true
    }

    /// Apakah jalur Goggles 3 / N3 di OS ini lewat host RNDIS user space.
    ///
    /// macOS: ya, dan itu bukan pilihan gaya - macOS tidak punya driver RNDIS
    /// bawaan (sebab yang sama membuat tethering USB Android tidak jalan di
    /// sana), dan HoRNDIS sudah mati di Apple Silicon karena kext pihak ketiga
    /// tidak berlaku lagi. Karena tidak ada yang memegang interface itu,
    /// Zerofuss mengklaimnya sendiri - hasilnya justru lebih sedikit langkah
    /// daripada di Windows: nol driver, nol adapter, nol alamat statis, nol
    /// prompt admin.
    ///
    /// Windows dan Linux: TIDAK. Di sana kernel punya driver RNDIS dan sudah
    /// memegang interface-nya, jadi klaim dari user space akan ditolak.
    pub const fn rndis_usb(self) -> bool {
        matches!(self, Os::MacOs)
    }

    /// Apakah tombol "Setel adapter" / "Bersihkan" bermakna di OS ini.
    ///
    /// Hanya di jalur adapter OS, karena hanya di sana ada adapter yang perlu
    /// diberi alamat statis. Di macOS keduanya harus disembunyikan: tidak ada
    /// adapter untuk disetel, dan tombol yang tidak bisa berhasil adalah
    /// jebakan.
    pub const fn setel_adapter(self) -> bool {
        !matches!(self, Os::MacOs)
    }

    /// Apakah konfigurasi USB harus dipilih sendiri sebelum interface diklaim.
    ///
    /// Windows/Linux sudah mengaktifkan config 1 saat enumerasi. macOS kadang
    /// tidak melakukannya untuk device berkelas vendor, dan tanpa config aktif
    /// `claim_interface` gagal dengan galat yang tidak menyebut sebabnya.
    pub const fn perlu_set_configuration(self) -> bool {
        matches!(self, Os::MacOs)
    }

    /// Nama untuk webview. Kontrak dengan `main.js`, jadi jangan diubah.
    pub const fn nama(self) -> &'static str {
        match self {
            Os::Windows => "windows",
            Os::MacOs => "macos",
            Os::Linux => "linux",
        }
    }
}

/// Lihat sekali apa yang tercolok.
///
/// `boleh_klaim` harus `false` selagi pipa hidup: status driver hanya bisa
/// diketahui dengan mencoba mengklaim interface, dan interface itu sedang kita
/// pegang sendiri - hasilnya akan terbaca "dipakai program lain".
pub fn probe(pid_bulk: u16, boleh_klaim: bool) -> Found {
    let Ok(list) = nusb::list_devices().wait() else { return Found::None };
    let mut hasil = Found::None;
    for d in list {
        if d.vendor_id() != VID {
            continue;
        }
        if d.product_id() == pid_bulk {
            // V1/V2 menang atas Rndis kalau dua-duanya tercolok: ia yang
            // dilayani modul ini, dan ia yang butuh tindakan pengguna.
            // Status driver hanya perlu ditanyakan di OS yang memang punya
            // langkah pemasangan driver. Di macOS/Linux percobaan klaim ini
            // cuma membuka device empat kali sedetik tanpa pernah bisa
            // menghasilkan keadaan baru.
            return if Os::HOST.perlu_driver() && boleh_klaim && !ada_driver(&d) {
                Found::BulkNoDriver
            } else {
                Found::Bulk
            };
        }
        if d.product_id() == PID_RNDIS {
            hasil = Found::Rndis;
        }
    }
    hasil
}

/// Nama produk dan serial goggles yang tercolok, untuk kotak identitas di UI.
///
/// Dipakai di jalur RNDIS user space, tempat tidak ada adapter jaringan yang
/// bisa dilaporkan. Keduanya dibaca dari descriptor string yang sudah diambil
/// saat enumerasi, jadi tidak ada I/O tambahan di sini.
pub fn identitas(pid: u16) -> Option<(String, String)> {
    nusb::list_devices().wait().ok()?.find_map(|d| {
        (d.vendor_id() == VID && d.product_id() == pid).then(|| {
            (
                d.product_string().unwrap_or("Goggles DJI").to_string(),
                d.serial_number().unwrap_or("-").to_string(),
            )
        })
    })
}

/// Apakah interface video sudah bisa diklaim - dengan kata lain, apakah
/// drivernya sudah terpasang.
///
/// nusb tidak memaparkan driver per-interface (`InterfaceInfo` tidak punya
/// medan itu, dan driver device komposit selalu `usbccgp`), jadi jawabannya
/// dicari dengan benar-benar mencoba - lalu langsung dilepas.
fn ada_driver(d: &DeviceInfo) -> bool {
    match d.open().wait() {
        Ok(dev) => match dev.claim_interface(IFACE).wait() {
            Ok(_) => true,
            Err(e) => !kind_tanpa_driver(e.kind()),
        },
        Err(e) => !kind_tanpa_driver(e.kind()),
    }
}

/// Membedakan "belum ada driver" dari "sedang dipakai".
///
/// Ini pembedaan yang menentukan: `Busy` dan `PermissionDenied` justru
/// MEMBUKTIKAN drivernya ada - interface-nya cuma sudah dipegang program lain
/// (DigiView, tab browser). Kalau keduanya ikut dihitung sebagai tanpa driver,
/// pengguna disuruh memasang ulang driver yang sudah benar, dan masalah
/// sebenarnya - program lain yang harus ditutup - tidak pernah disebut.
fn kind_tanpa_driver(k: nusb::ErrorKind) -> bool {
    matches!(k, nusb::ErrorKind::Unsupported | nusb::ErrorKind::NotFound)
}

/// Interface video yang sudah diklaim dan sudah dipancing, beserta antrean
/// transfer masuknya. Melepas klaim saat di-drop; tidak ada paket berhenti yang
/// perlu dikirim ke goggles.
pub struct Feed {
    keluar: nusb::Endpoint<Bulk, Out>,
    masuk: nusb::Endpoint<Bulk, In>,
}

impl Feed {
    /// Buka goggles, klaim interface video, kirim magic pembuka, dan isi
    /// antrean transfer masuk.
    ///
    /// Sengaja dijalankan **sinkron** dari `Pipeline::start`, bukan di dalam
    /// utas baca: kalau driver belum ada atau kabelnya charge-only, pengguna
    /// harus dapat pesan yang bisa ditindak - bukan layar hitam tanpa sebab.
    pub fn open(pid: u16) -> Result<Feed, String> {
        let list = nusb::list_devices()
            .wait()
            .map_err(|e| format!("USB tidak bisa dibaca: {e}"))?;
        let info = list
            .into_iter()
            .find(|d| d.vendor_id() == VID && d.product_id() == pid)
            .ok_or_else(|| {
                "FPV Goggles V1/V2 tidak terlihat di USB. Pastikan kabelnya kabel data \
                 (bukan charge-only) dan goggles menyala."
                    .to_string()
            })?;

        let dev = info.open().wait().map_err(jelaskan)?;
        // `set_configuration` sengaja tidak dipanggil di Windows/Linux: config 1
        // sudah aktif. Hanya macOS yang kadang tidak memilihkannya untuk device
        // kelas vendor. `cfg!`, bukan `#[cfg]`, supaya cabang ini ikut diperiksa
        // compiler saat dibangun di Windows.
        if Os::HOST.perlu_set_configuration() && dev.active_configuration().is_err() {
            dev.set_configuration(1).wait().map_err(jelaskan)?;
        }

        let iface = dev.claim_interface(IFACE).wait().map_err(jelaskan)?;
        let keluar = iface
            .endpoint::<Bulk, Out>(EP_OUT)
            .map_err(|e| format!("endpoint perintah goggles tidak bisa dibuka: {e}"))?;
        let masuk = iface
            .endpoint::<Bulk, In>(EP_IN)
            .map_err(|e| format!("endpoint video goggles tidak bisa dibuka: {e}"))?;

        let mut f = Feed { keluar, masuk };
        f.kick().map_err(|e| format!("goggles tidak menerima perintah mulai: {e}"))?;
        while f.masuk.pending() < PENDING {
            let b = f.masuk.allocate(CHUNK);
            f.masuk.submit(b);
        }
        Ok(f)
    }

    /// Ambil satu potongan ke `keluar`, lalu kembalikan buffer transfer ke
    /// antrean supaya tidak pernah ada jeda baca.
    ///
    /// `Ok(false)` = tidak ada yang datang dalam `batas`, artinya goggles diam
    /// (belum ada sumber gambar). `Err` = transfer gagal, biasanya tercabut.
    pub(crate) fn next(&mut self, batas: Duration, keluar: &mut Vec<u8>) -> Result<bool, String> {
        let Some(c) = self.masuk.wait_next_complete(batas) else {
            return Ok(false);
        };
        keluar.clear();
        keluar.extend_from_slice(&c.buffer);
        let status = c.status;
        // Buffer dikembalikan SEBELUM status diperiksa. Kalau ia hilang di jalur
        // galat, antrean menyusut tiap kali ada gangguan sesaat - dan setelah
        // delapan kali tidak ada transfer menggantung lagi sama sekali.
        self.masuk.submit(c.buffer);
        status.map_err(|e| e.to_string())?;
        Ok(!keluar.is_empty())
    }

    /// Kirim magic pembuka. Dipakai sekali saat membuka, lalu diulang setiap
    /// kali bacaan kosong - pola dari DigiView-Android, yang paling tahan
    /// banting dari semua implementasi rujukan: goggles yang belum punya sumber
    /// gambar diam saja, dan ia mulai mengirim setelah dipancing lagi.
    pub(crate) fn kick(&mut self) -> Result<(), String> {
        self.keluar
            .transfer_blocking(MAGIC.to_vec().into(), Duration::from_millis(500))
            .into_result()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Kalimat untuk "interface tidak bisa diklaim", per OS.
///
/// Tiga kalimat, karena tindakan yang benar berbeda total: di Windows ada
/// driver yang harus dipasang, di macOS tidak ada driver untuk dipasang sama
/// sekali, dan di Linux ini hampir selalu soal izin udev. Menyuruh pengguna
/// Mac menjalankan Zadig - program Windows - adalah jalan buntu.
fn pesan_tanpa_driver(os: Os) -> &'static str {
    match os {
        Os::Windows => {
            "Interface video goggles belum dipasangi WinUSB. Buka Panduan -> \
             \"FPV Goggles V1/V2\" dan pasang driver dengan Zadig di entri \
             \"Bulk interface\"."
        }
        // Di macOS tidak ada driver untuk dipasang: interface 3 berkelas
        // vendor dan tidak dipegang kernel, jadi kegagalan di sini berarti
        // devicenya tidak terbuka - kabel charge-only, goggles mati, atau
        // OTG Wired Connection belum menyala.
        Os::MacOs => {
            "Interface video goggles tidak bisa dibuka. macOS tidak perlu \
             driver tambahan untuk jalur ini, jadi periksa yang lain: pakai \
             kabel data (bukan charge-only), nyalakan Settings -> About -> \
             OTG Wired Connection di goggles, lalu cabut-colok kabelnya."
        }
        Os::Linux => {
            "Interface video goggles tidak bisa diklaim. Di Linux ini biasanya \
             soal izin: tambahkan aturan udev untuk VID 2ca3 (mode 0666), \
             lalu cabut-colok kabelnya."
        }
    }
}

/// Terjemahkan galat nusb jadi kalimat yang menyebut tindakannya.
fn jelaskan(e: nusb::Error) -> String {
    pesan(e.kind(), &e)
}

fn pesan(k: nusb::ErrorKind, asli: &dyn std::fmt::Display) -> String {
    pesan_os(k, asli, Os::HOST)
}

/// Pemetaan kind -> kalimat, dipisah supaya bisa diuji: `nusb::Error` tidak
/// bisa dibangun dari luar crate-nya, dan `os` ikut jadi parameter supaya
/// ketiga varian teruji dari satu mesin.
///
/// Ini bukan kosmetik. `Unsupported` adalah galat yang PASTI dilihat tiap
/// pengguna Windows sebelum memasang driver, dan teks aslinya terdengar seperti
/// aplikasinya yang tidak mendukung Windows.
fn pesan_os(k: nusb::ErrorKind, asli: &dyn std::fmt::Display, os: Os) -> String {
    match k {
        nusb::ErrorKind::Unsupported | nusb::ErrorKind::NotFound => {
            pesan_tanpa_driver(os).into()
        }
        nusb::ErrorKind::Busy | nusb::ErrorKind::PermissionDenied => {
            "Interface video goggles sedang dipakai program lain. Tutup DigiView \
             atau tab browser yang tersambung, lalu coba lagi."
                .into()
        }
        nusb::ErrorKind::Disconnected => "Goggles terlepas saat dibuka.".into(),
        _ => format!("goggles tidak bisa dibuka: {asli}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Angka protokol dikunci di tes: kalau ada yang mengubahnya, jalur V1/V2
    /// akan diam-diam gagal di hardware yang tidak kami punya.
    #[test]
    fn angka_protokol_tetap() {
        assert_eq!(VID, 0x2ca3);
        assert_eq!(PID_BULK, 0x001f);
        assert_eq!(IFACE, 3);
        assert_eq!(EP_OUT, 0x03);
        assert_eq!(EP_IN, 0x84);
        assert_eq!(&MAGIC, b"RMVT");
    }

    /// Potongan harus kelipatan max packet 512 B, kalau tidak ada sisa ruang
    /// yang tidak pernah terpakai di ujung tiap transfer.
    #[test]
    fn potongan_kelipatan_max_packet() {
        assert_eq!(CHUNK % 512, 0);
        assert!(PENDING >= 2, "satu transfer menggantung = ada jeda baca");
    }

    /// Goggles 3 dan V1/V2 tidak boleh tertukar: keduanya VID 0x2ca3, dan
    /// mengirim RMVT ke Goggles 3 hanya menghasilkan layar hitam.
    #[test]
    fn pid_dua_keluarga_berbeda() {
        assert_ne!(PID_BULK, PID_RNDIS);
    }

    /// Kalimat Windows harus menyebut alat yang dipakai; kalimat macOS TIDAK
    /// boleh menyebutnya, karena Zadig tidak ada di Mac dan menyuruh mencarinya
    /// adalah jalan buntu. Ketiganya diuji dari mesin mana pun - itu gunanya
    /// `os` jadi parameter.
    #[test]
    fn galat_tanpa_driver_menyebut_tindakan_yang_benar_per_os() {
        let win = pesan_tanpa_driver(Os::Windows);
        assert!(win.contains("Zadig"));
        assert!(win.contains("Bulk interface"));

        let mac = pesan_tanpa_driver(Os::MacOs);
        assert!(!mac.contains("Zadig"), "{mac}");
        assert!(!mac.contains("WinUSB"), "{mac}");
        assert!(mac.contains("OTG Wired Connection"), "{mac}");

        let linux = pesan_tanpa_driver(Os::Linux);
        assert!(!linux.contains("Zadig"), "{linux}");
        assert!(linux.contains("udev"), "{linux}");

        for os in [Os::Windows, Os::MacOs, Os::Linux] {
            for k in [nusb::ErrorKind::Unsupported, nusb::ErrorKind::NotFound] {
                let s = pesan_os(k, &"asli", os);
                assert_eq!(s, pesan_tanpa_driver(os), "{os:?} {k:?}");
                // Teks asli nusb menyesatkan; ia tidak boleh ikut muncul.
                assert!(!s.contains("asli"), "{s}");
            }
        }
    }

    /// Kemampuan per OS adalah kontrak dengan UI dan dengan pemilihan jalur:
    /// `perlu_driver` menyalakan petunjuk Zadig, `rndis_usb` memilih host RNDIS
    /// user space, dan `setel_adapter` menyalakan tombol Setel adapter. Satu
    /// saja terbalik dan pengguna dikirim ke langkah yang tidak mungkin
    /// berhasil - atau lebih buruk, jalur yang salah dibuka diam-diam.
    #[test]
    fn kemampuan_per_os() {
        assert!(Os::Windows.perlu_driver());
        assert!(!Os::MacOs.perlu_driver());
        assert!(!Os::Linux.perlu_driver());

        // Goggles 3 / N3 didukung di ketiganya - yang berbeda cuma jalurnya.
        for os in [Os::Windows, Os::MacOs, Os::Linux] {
            assert!(os.rndis(), "{os:?}");
        }

        // User space HANYA di macOS: di Windows dan Linux kernel sudah memegang
        // interface RNDIS, jadi klaim dari user space pasti ditolak.
        assert!(Os::MacOs.rndis_usb());
        assert!(!Os::Windows.rndis_usb());
        assert!(!Os::Linux.rndis_usb());

        // Tombol Setel adapter hanya bermakna di jalur adapter OS, dan itu
        // kebalikan tepat dari jalur user space.
        for os in [Os::Windows, Os::MacOs, Os::Linux] {
            assert_eq!(os.setel_adapter(), !os.rndis_usb(), "{os:?}");
        }
        // `perlu_set_configuration` juga khas macOS - dikunci di sini supaya
        // ketiga kemampuan macOS tidak bisa bergeser satu-satu tanpa terlihat.
        assert!(Os::MacOs.perlu_set_configuration());
        assert!(!Os::Windows.perlu_set_configuration());

        assert_eq!(Os::Windows.nama(), "windows");
        assert_eq!(Os::MacOs.nama(), "macos");
        assert_eq!(Os::Linux.nama(), "linux");
    }

    #[test]
    fn galat_dipakai_program_lain_menyebut_tutup_program() {
        for k in [nusb::ErrorKind::Busy, nusb::ErrorKind::PermissionDenied] {
            let s = pesan(k, &"asli");
            assert!(s.contains("program lain"), "{k:?} -> {s}");
        }
    }

    /// Probe tidak boleh panik walau tidak ada apa pun tercolok - ia dipanggil
    /// dari `stats` empat kali sedetik.
    #[test]
    fn probe_aman_tanpa_hardware() {
        for boleh in [true, false] {
            let _ = probe(PID_BULK, boleh);
        }
    }

    /// Pembedaan yang menentukan: `Busy` MEMBUKTIKAN drivernya ada - interface
    /// itu cuma sudah dipegang program lain. Kalau ia ikut dihitung sebagai
    /// tanpa driver, pengguna DigiView disuruh memasang ulang driver yang sudah
    /// benar, dan masalah sebenarnya tidak pernah disebut.
    #[test]
    fn dipakai_program_lain_bukan_berarti_tanpa_driver() {
        for k in [nusb::ErrorKind::Busy, nusb::ErrorKind::PermissionDenied] {
            assert!(!kind_tanpa_driver(k), "{k:?}");
        }
        for k in [nusb::ErrorKind::Unsupported, nusb::ErrorKind::NotFound] {
            assert!(kind_tanpa_driver(k), "{k:?}");
        }
        // Tercabut juga bukan soal driver.
        assert!(!kind_tanpa_driver(nusb::ErrorKind::Disconnected));
    }
}
