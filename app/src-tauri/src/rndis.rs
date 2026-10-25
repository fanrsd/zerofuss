//! Host RNDIS di **user space**, untuk Goggles 3 / N3 di macOS.
//!
//! Kenapa ini ada. Goggles 3 mengirim video lewat adapter jaringan USB (RNDIS)
//! plus UDP. Di Windows dan Linux kernelnya punya driver RNDIS: adapter muncul,
//! diberi alamat, dan `pipeline::recv_loop` cukup memakai `UdpSocket` biasa.
//! macOS TIDAK punya driver RNDIS sama sekali - itu sebabnya tethering USB
//! Android juga tidak jalan di sana tanpa HoRNDIS, dan HoRNDIS adalah kext yang
//! sudah mati di Apple Silicon.
//!
//! Justru ketiadaan driver itu yang membuka jalan: kalau tidak ada modul kernel
//! yang memegang interface RNDIS goggles, aplikasi boleh mengklaimnya sendiri.
//! Jadi di macOS Zerofuss TIDAK meminta OS menyediakan jaringan - ia menjadi
//! host RNDIS-nya sendiri, lalu menumpangkan ARP/IPv4/UDP secukupnya di atas
//! (lihat `net.rs`). Hasilnya, di Mac jalur Goggles 3 malah lebih sedikit
//! langkahnya daripada di Windows: nol driver, nol adapter, nol alamat statis,
//! nol prompt admin.
//!
//! Pendekatan yang sama sudah terbukti di macOS Apple Silicon oleh dua proyek
//! tethering Android berbasis libusb - JellyBrick/ReRNDIS dan
//! hknsglm/android-usb-tether-macos - keduanya tanpa kext dan tanpa mengubah
//! SIP. Angka-angka protokol di bawah dicocokkan dengan `include/rndis.h` milik
//! yang kedua, bukan dari ingatan.
//!
//! Bedanya dengan proyek-proyek itu: mereka membuat antarmuka `utun` supaya
//! SELURUH jaringan OS bisa lewat. Kami tidak butuh itu. Yang dibutuhkan hanya
//! satu aliran UDP ke `192.168.60.2:9003`, jadi tumpukan IP-nya cukup beberapa
//! ratus baris di dalam proses - tanpa antarmuka virtual, tanpa root, tanpa
//! menyentuh routing atau DNS mesin pengguna.
//!
//! Status: TERBUKTI di hardware. MacBook Air (Apple Silicon) + Goggles 3,
//! `bun run tauri dev` - interface diklaim, INITIALIZE dibalas, dan gambar
//! muncul di jendela. Angka yang terukur di sesi itu: MAC perangkat
//! `ca:dc:a4:48:ce:f0`, `MaxTransferSize` 1566-an (gadget RNDIS Linux:
//! MTU + 44 + 22), `PacketAlignmentFactor` 0, dan ARP TIDAK dijawab goggles -
//! video tetap jalan dengan MAC dari RNDIS sebagai tujuan, jadi ARP di sini
//! pelengkap, bukan syarat.

use nusb::transfer::{Bulk, ControlIn, ControlOut, ControlType, In, Out, Recipient};
use nusb::{DeviceInfo, MaybeFuture};
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ protokol

/// Tipe pesan RNDIS. `PACKET` dan `INDICATE_STATUS` sama-sama 0x00000001 di
/// spesifikasinya - yang membedakan adalah salurannya: PACKET hanya lewat pipa
/// bulk, INDICATE_STATUS hanya lewat saluran kontrol. Karena itu keduanya tidak
/// pernah perlu dibedakan di satu tempat.
const MSG_PACKET: u32 = 0x0000_0001;
const MSG_INIT: u32 = 0x0000_0002;
const MSG_INIT_C: u32 = 0x8000_0002;
const MSG_QUERY: u32 = 0x0000_0004;
const MSG_QUERY_C: u32 = 0x8000_0004;
const MSG_SET: u32 = 0x0000_0005;
const MSG_SET_C: u32 = 0x8000_0005;

const STATUS_SUCCESS: u32 = 0x0000_0000;

const OID_CURRENT_PACKET_FILTER: u32 = 0x0001_010E;
const OID_802_3_PERMANENT_ADDRESS: u32 = 0x0101_0101;

/// Filter paket yang diminta: kiriman untuk MAC kita, broadcast (ARP), dan
/// semua multicast. Tanpa memasang filter sama sekali, banyak perangkat RNDIS
/// tidak mengirim apa pun - jadi ini bukan pengaturan opsional.
const FILTER_DIRECTED: u32 = 0x0000_0001;
const FILTER_ALL_MULTICAST: u32 = 0x0000_0004;
const FILTER_BROADCAST: u32 = 0x0000_0008;

/// Permintaan kelas di interface kontrol: SEND_ENCAPSULATED_COMMAND (0x00) dan
/// GET_ENCAPSULATED_RESPONSE (0x01).
const REQ_SEND: u8 = 0x00;
const REQ_GET: u8 = 0x01;

/// Kelas interface pasangan RNDIS. Kontrolnya kelas 0xE0 (Wireless Controller)
/// subclass 0x01 protokol 0x03 - itulah tanda tangan RNDIS, dan itu yang bikin
/// macOS tidak punya driver untuknya: tidak ada yang cocok dengan kombinasi itu.
const KELAS_KONTROL: (u8, u8, u8) = (0xE0, 0x01, 0x03);
/// Interface data pasangannya berkelas CDC-Data (0x0A).
const KELAS_DATA: u8 = 0x0A;

/// Ukuran transfer maksimum yang kita sanggupi, dikirim di INITIALIZE.
const MAX_TRANSFER: u32 = 0x8000;

/// Panjang header RNDIS_PACKET_MSG. `data_offset` dihitung dari AWAL medan
/// `data_offset` itu sendiri (byte ke-8), jadi nilainya 44 - 8 = 36.
const HDR_PAKET: usize = 44;
const OFFSET_DATA: u32 = (HDR_PAKET - 8) as u32;

/// Ukuran satu transfer masuk dan banyaknya yang dijaga menggantung. Alasannya
/// sama seperti di `usb.rs`: di jeda antara satu bacaan selesai dan bacaan
/// berikutnya diajukan, host tidak menerima apa pun - dan goggles tidak
/// menunggu.
const CHUNK: usize = 32 * 1024;
const PENDING: usize = 8;

fn u32le(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn taruh(buf: &mut [u8], i: usize, v: u32) {
    buf[i..i + 4].copy_from_slice(&v.to_le_bytes());
}

/// INITIALIZE: 24 byte, tanpa muatan.
pub fn bangun_init(req: u32) -> [u8; 24] {
    let mut b = [0u8; 24];
    taruh(&mut b, 0, MSG_INIT);
    taruh(&mut b, 4, 24);
    taruh(&mut b, 8, req);
    taruh(&mut b, 12, 1); // versi mayor
    taruh(&mut b, 16, 0); // versi minor
    taruh(&mut b, 20, MAX_TRANSFER);
    b
}

/// Yang kita pakai dari INITIALIZE_CMPLT.
///
/// Tata letaknya (52 byte, semua little-endian):
///
///    0 MessageType     16 MajorVersion   32 MaxPacketsPerTransfer
///    4 MessageLength   20 MinorVersion   36 MaxTransferSize
///    8 RequestID       24 DeviceFlags    40 PacketAlignmentFactor
///   12 Status          28 Medium         44 Reserved1  48 Reserved2
///
/// Offset 36 vs 40 itu bukan detail: versi pertama kode ini membaca
/// MaxTransferSize dari byte ke-40 - yang sebenarnya PacketAlignmentFactor -
/// dan di hardware Goggles 3 sungguhan medan itu bernilai 0, jadi hasilnya
/// terkerek jadi 1024 oleh batas bawah `buka()` dan terlihat seperti angka
/// yang masuk akal. Diagnostik `--uji-rndis` di Mac yang memperlihatkannya:
/// gadget RNDIS Linux (yang dipakai goggles) melaporkan MTU + 44 + 22, jadi
/// nilai sebenarnya ~1566, bukan 1024 bulat.
#[derive(Debug, PartialEq, Eq)]
pub struct Init {
    pub max_transfer: u32,
    pub max_paket: u32,
    pub alignment: u32,
    pub medium: u32,
}

pub fn urai_init_cmplt(b: &[u8]) -> Result<Init, String> {
    if b.len() < 52 {
        return Err(format!("INITIALIZE_CMPLT terlalu pendek: {} byte", b.len()));
    }
    if u32le(b, 0) != MSG_INIT_C {
        return Err(format!("balasan bukan INITIALIZE_CMPLT: 0x{:08x}", u32le(b, 0)));
    }
    let status = u32le(b, 12);
    if status != STATUS_SUCCESS {
        return Err(format!("goggles menolak INITIALIZE: status 0x{status:08x}"));
    }
    Ok(Init {
        max_transfer: u32le(b, 36),
        max_paket: u32le(b, 32),
        alignment: u32le(b, 40),
        medium: u32le(b, 28),
    })
}

/// QUERY tanpa data masukan. `offset` dihitung dari medan `request_id` (byte
/// ke-8), jadi 20 menunjuk tepat ke ujung header 28 byte.
pub fn bangun_query(req: u32, oid: u32) -> [u8; 28] {
    let mut b = [0u8; 28];
    taruh(&mut b, 0, MSG_QUERY);
    taruh(&mut b, 4, 28);
    taruh(&mut b, 8, req);
    taruh(&mut b, 12, oid);
    taruh(&mut b, 16, 0); // panjang data masukan
    taruh(&mut b, 20, 20); // offset data masukan
    b
}

/// Petik data hasil QUERY. Offsetnya relatif ke byte ke-8, sama seperti di
/// permintaan - salah menafsirkannya sebagai offset dari awal pesan membuat
/// MAC perangkat terbaca bergeser 8 byte, dan itu tidak akan terlihat sebagai
/// galat, hanya sebagai jaringan yang diam.
pub fn urai_query_cmplt(b: &[u8]) -> Result<&[u8], String> {
    if b.len() < 24 {
        return Err(format!("QUERY_CMPLT terlalu pendek: {} byte", b.len()));
    }
    if u32le(b, 0) != MSG_QUERY_C {
        return Err(format!("balasan bukan QUERY_CMPLT: 0x{:08x}", u32le(b, 0)));
    }
    let status = u32le(b, 12);
    if status != STATUS_SUCCESS {
        return Err(format!("QUERY ditolak: status 0x{status:08x}"));
    }
    let len = u32le(b, 16) as usize;
    let awal = 8usize.saturating_add(u32le(b, 20) as usize);
    b.get(awal..awal + len)
        .ok_or_else(|| format!("QUERY_CMPLT menunjuk keluar buffer: {awal}+{len} dari {}", b.len()))
}

/// SET dengan muatan OID.
pub fn bangun_set(req: u32, oid: u32, data: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; 28 + data.len()];
    taruh(&mut b, 0, MSG_SET);
    taruh(&mut b, 4, (28 + data.len()) as u32);
    taruh(&mut b, 8, req);
    taruh(&mut b, 12, oid);
    taruh(&mut b, 16, data.len() as u32);
    taruh(&mut b, 20, 20);
    b[28..].copy_from_slice(data);
    b
}

pub fn urai_set_cmplt(b: &[u8]) -> Result<(), String> {
    if b.len() < 16 {
        return Err(format!("SET_CMPLT terlalu pendek: {} byte", b.len()));
    }
    if u32le(b, 0) != MSG_SET_C {
        return Err(format!("balasan bukan SET_CMPLT: 0x{:08x}", u32le(b, 0)));
    }
    let status = u32le(b, 12);
    if status != STATUS_SUCCESS {
        return Err(format!("SET ditolak: status 0x{status:08x}"));
    }
    Ok(())
}

/// Bungkus satu frame Ethernet jadi RNDIS_PACKET_MSG.
pub fn bangun_paket(eth: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; HDR_PAKET + eth.len()];
    taruh(&mut b, 0, MSG_PACKET);
    taruh(&mut b, 4, (HDR_PAKET + eth.len()) as u32);
    taruh(&mut b, 8, OFFSET_DATA);
    taruh(&mut b, 12, eth.len() as u32);
    b[HDR_PAKET..].copy_from_slice(eth);
    b
}

/// Ambil semua frame Ethernet dari satu transfer bulk.
///
/// Satu transfer bisa memuat BEBERAPA pesan berurutan - perangkat RNDIS
/// diizinkan menggabungkannya, dan yang bandwidth-nya besar memang melakukannya.
/// Kalau hanya pesan pertama yang dibaca, sisanya hilang tanpa jejak: gejalanya
/// video yang patah-patah dengan penghitung kehilangan tetap nol.
///
/// Pesan yang bukan PACKET dilewati, bukan bikin gagal: perangkat boleh
/// menyelipkan pesan lain, dan menghentikan seluruh pembacaan karena itu
/// membuat jalur video mati untuk alasan yang tidak penting.
pub fn untuk_tiap_frame(buf: &[u8], mut f: impl FnMut(&[u8])) {
    let mut i = 0usize;
    while i + 8 <= buf.len() {
        let tipe = u32le(buf, i);
        let panjang = u32le(buf, i + 4) as usize;
        // Panjang nol atau melebihi sisa buffer = pesan rusak. Berhenti, jangan
        // maju satu byte mencari-cari: aliran ini tidak punya penanda sinkron,
        // jadi menebak posisi berikutnya hanya menghasilkan sampah.
        if panjang < 8 || i + panjang > buf.len() {
            return;
        }
        if tipe == MSG_PACKET && panjang >= HDR_PAKET {
            let pesan = &buf[i..i + panjang];
            let awal = 8usize.saturating_add(u32le(pesan, 8) as usize);
            let len = u32le(pesan, 12) as usize;
            if let Some(eth) = pesan.get(awal..awal + len) {
                f(eth);
            }
        }
        i += panjang;
    }
}

// ------------------------------------------------------------------- tautan

/// Nomor interface pasangan RNDIS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pasangan {
    pub kontrol: u8,
    pub data: u8,
}

/// Cari pasangan interface RNDIS dari descriptor, bukan dari nomor tetap.
///
/// Descriptor Goggles N3 yang terekam memang menaruhnya di IF0 + IF1, tapi
/// nomor itu tidak dijanjikan siapa pun - satu pembaruan firmware yang
/// menambah interface di depan sudah cukup untuk menggesernya. Kelas
/// interface-nya, sebaliknya, ditentukan spesifikasi RNDIS.
pub fn cari_pasangan(d: &DeviceInfo) -> Option<Pasangan> {
    let kontrol = d.interfaces().find(|i| {
        (i.class(), i.subclass(), i.protocol()) == KELAS_KONTROL
    })?;
    // Interface data adalah tetangga tepat sesudahnya - itu aturan pengelompokan
    // fungsi USB, dan satu-satunya cara memasangkannya tanpa membaca IAD.
    let nomor = kontrol.interface_number();
    let data = d
        .interfaces()
        .find(|i| i.interface_number() == nomor + 1 && i.class() == KELAS_DATA)?;
    Some(Pasangan { kontrol: nomor, data: data.interface_number() })
}

/// Tautan RNDIS yang sudah hidup: interface terklaim, INITIALIZE selesai, MAC
/// perangkat diketahui, filter paket terpasang, dan antrean transfer masuk
/// terisi.
pub struct Tautan {
    kontrol: nusb::Interface,
    nomor_kontrol: u16,
    keluar: nusb::Endpoint<Bulk, Out>,
    masuk: nusb::Endpoint<Bulk, In>,
    req: u32,
    /// MAC perangkat (sisi goggles). Dipakai sebagai tujuan awal sebelum ARP
    /// menjawab, dan sebagai penanda tautan sudah benar-benar bicara.
    pub mac_perangkat: [u8; 6],
    /// Nilai apa adanya dari INITIALIZE_CMPLT. Disimpan lengkap karena
    /// `--uji-rndis` mencetaknya, dan itulah satu-satunya cara melihat apa yang
    /// benar-benar dilaporkan hardware - offset yang salah baca terlihat di
    /// sini sebagai angka yang tidak masuk akal.
    pub max_transfer: u32,
    pub max_paket: u32,
    pub alignment: u32,
    pub medium: u32,
}

impl Tautan {
    /// Buka goggles, klaim pasangan RNDIS, dan bawa tautannya sampai siap
    /// mengirim frame.
    ///
    /// Dijalankan sinkron dari `Pipeline::start`, sama seperti `usb::Feed::open`:
    /// setiap kegagalan di sini harus jadi kalimat yang bisa ditindak pengguna,
    /// bukan layar hitam.
    pub fn buka(pid: u16) -> Result<Tautan, String> {
        let list = nusb::list_devices()
            .wait()
            .map_err(|e| format!("USB tidak bisa dibaca: {e}"))?;
        let info = list
            .into_iter()
            .find(|d| d.vendor_id() == crate::usb::VID && d.product_id() == pid)
            .ok_or_else(|| {
                "Goggles 3 / N3 tidak terlihat di USB. Pastikan kabelnya kabel data \
                 (bukan charge-only), goggles menyala, dan Settings -> About -> \
                 OTG Wired Connection sudah ON."
                    .to_string()
            })?;
        let pasangan = cari_pasangan(&info).ok_or_else(|| {
            "Goggles terlihat tapi tidak memaparkan interface RNDIS. Nyalakan \
             OTG Wired Connection di goggles, lalu cabut-colok kabelnya."
                .to_string()
        })?;

        let dev = info.open().wait().map_err(jelaskan)?;
        if crate::usb::Os::HOST.perlu_set_configuration() && dev.active_configuration().is_err() {
            dev.set_configuration(1).wait().map_err(jelaskan)?;
        }

        let kontrol = dev.claim_interface(pasangan.kontrol).wait().map_err(jelaskan)?;
        let data = dev.claim_interface(pasangan.data).wait().map_err(jelaskan)?;

        // Endpoint bulk dicari dari descriptor, bukan ditebak: alamatnya
        // (0x81/0x01 di N3) tidak dijanjikan spesifikasi apa pun.
        let (alamat_masuk, alamat_keluar) = alamat_bulk(&data).ok_or_else(|| {
            "Interface data RNDIS goggles tidak punya pasangan endpoint bulk.".to_string()
        })?;
        let keluar = data
            .endpoint::<Bulk, Out>(alamat_keluar)
            .map_err(|e| format!("endpoint kirim RNDIS tidak bisa dibuka: {e}"))?;
        let masuk = data
            .endpoint::<Bulk, In>(alamat_masuk)
            .map_err(|e| format!("endpoint terima RNDIS tidak bisa dibuka: {e}"))?;

        let mut t = Tautan {
            kontrol,
            nomor_kontrol: pasangan.kontrol as u16,
            keluar,
            masuk,
            req: 0,
            mac_perangkat: [0; 6],
            max_transfer: MAX_TRANSFER,
            max_paket: 0,
            alignment: 0,
            medium: 0,
        };

        let req = t.req_baru();
        let init = urai_init_cmplt(&t.perintah(&bangun_init(req))?)?;
        // Nilai mentah disimpan apa adanya - yang dipakai untuk membatasi
        // ukuran tulis sudah dijepit di `kirim_frame`, jadi menjepitnya di sini
        // hanya menyembunyikan apa yang sebenarnya dilaporkan hardware.
        t.max_transfer = init.max_transfer;
        t.max_paket = init.max_paket;
        t.alignment = init.alignment;
        t.medium = init.medium;

        let req = t.req_baru();
        let jawab = t.perintah(&bangun_query(req, OID_802_3_PERMANENT_ADDRESS))?;
        let mac = urai_query_cmplt(&jawab)?;
        if mac.len() < 6 {
            return Err(format!("MAC perangkat cuma {} byte", mac.len()));
        }
        t.mac_perangkat.copy_from_slice(&mac[..6]);

        let filter = FILTER_DIRECTED | FILTER_BROADCAST | FILTER_ALL_MULTICAST;
        let req = t.req_baru();
        let pesan = bangun_set(req, OID_CURRENT_PACKET_FILTER, &filter.to_le_bytes());
        urai_set_cmplt(&t.perintah(&pesan)?)?;

        while t.masuk.pending() < PENDING {
            let b = t.masuk.allocate(CHUNK);
            t.masuk.submit(b);
        }
        Ok(t)
    }

    fn req_baru(&mut self) -> u32 {
        self.req += 1;
        self.req
    }

    /// Kirim satu pesan kontrol, lalu ambil balasannya.
    ///
    /// Balasannya diambil dengan MENJAJAL berulang, bukan dengan menunggu
    /// notifikasi di endpoint interrupt. Alasannya praktis: notifikasi itu hanya
    /// memberi tahu "balasan sudah siap", dan perangkat tetap wajib menyimpan
    /// balasannya sampai diambil - jadi menjajal beberapa kali memberi hasil
    /// yang sama tanpa satu pun endpoint tambahan yang harus dibuka dan
    /// dijaga. Ini pula yang dilakukan implementasi libusb yang sudah terbukti
    /// jalan di macOS.
    fn perintah(&mut self, pesan: &[u8]) -> Result<Vec<u8>, String> {
        self.kontrol
            .control_out(
                ControlOut {
                    control_type: ControlType::Class,
                    recipient: Recipient::Interface,
                    request: REQ_SEND,
                    value: 0,
                    index: self.nomor_kontrol,
                    data: pesan,
                },
                Duration::from_millis(1000),
            )
            .wait()
            .map_err(|e| format!("perintah RNDIS gagal dikirim: {e}"))?;

        let batas = Instant::now() + Duration::from_millis(1500);
        let mut terakhir = String::new();
        while Instant::now() < batas {
            match self
                .kontrol
                .control_in(
                    ControlIn {
                        control_type: ControlType::Class,
                        recipient: Recipient::Interface,
                        request: REQ_GET,
                        value: 0,
                        index: self.nomor_kontrol,
                        length: 1024,
                    },
                    Duration::from_millis(500),
                )
                .wait()
            {
                // Balasan kosong = belum siap, bukan gagal. Coba lagi.
                Ok(v) if v.len() >= 8 => return Ok(v),
                Ok(_) => {}
                Err(e) => terakhir = e.to_string(),
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(if terakhir.is_empty() {
            "goggles tidak membalas perintah RNDIS".into()
        } else {
            format!("goggles tidak membalas perintah RNDIS: {terakhir}")
        })
    }

    /// Kirim satu frame Ethernet.
    pub fn kirim_frame(&mut self, eth: &[u8]) -> Result<(), String> {
        self.keluar
            .transfer_blocking(bangun_paket(eth).into(), Duration::from_millis(500))
            .into_result()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Ambil satu transfer masuk dan panggil `f` untuk tiap frame Ethernet di
    /// dalamnya. `Ok(false)` = tidak ada apa pun dalam `batas`.
    ///
    /// Buffer transfer dikembalikan ke antrean SEBELUM status diperiksa - kalau
    /// ia hilang di jalur galat, antrean menyusut tiap ada gangguan sesaat, dan
    /// sesudah delapan kali tidak ada transfer menggantung lagi sama sekali.
    pub fn terima(
        &mut self,
        batas: Duration,
        f: impl FnMut(&[u8]),
    ) -> Result<bool, String> {
        let Some(c) = self.masuk.wait_next_complete(batas) else {
            return Ok(false);
        };
        let ada = !c.buffer.is_empty();
        untuk_tiap_frame(&c.buffer, f);
        let status = c.status;
        self.masuk.submit(c.buffer);
        status.map_err(|e| e.to_string())?;
        Ok(ada)
    }
}

/// Alamat endpoint bulk IN dan OUT dari interface yang sudah diklaim.
fn alamat_bulk(iface: &nusb::Interface) -> Option<(u8, u8)> {
    let d = iface.descriptor()?;
    let mut masuk = None;
    let mut keluar = None;
    for e in d.endpoints() {
        if e.transfer_type() != nusb::descriptors::TransferType::Bulk {
            continue;
        }
        let a = e.address();
        if a & 0x80 != 0 {
            masuk = masuk.or(Some(a));
        } else {
            keluar = keluar.or(Some(a));
        }
    }
    Some((masuk?, keluar?))
}

/// Kalimat galat yang menyebut tindakannya. Berbeda dari `usb::pesan` karena
/// penyebab yang mungkin di sini lain: tidak ada driver untuk dipasang, dan
/// "sedang dipakai" berarti ada driver RNDIS pihak ketiga (HoRNDIS) atau
/// instansi Zerofuss lain yang memegang interface-nya.
fn jelaskan(e: nusb::Error) -> String {
    match e.kind() {
        nusb::ErrorKind::Busy | nusb::ErrorKind::PermissionDenied => {
            "Interface RNDIS goggles sedang dipegang program atau driver lain. \
             Tutup instansi Zerofuss yang lain, dan kalau ada driver RNDIS pihak \
             ketiga terpasang (HoRNDIS), nonaktifkan - jalur ini tidak \
             membutuhkannya."
                .into()
        }
        nusb::ErrorKind::Disconnected => "Goggles terlepas saat dibuka.".into(),
        _ => format!("interface RNDIS goggles tidak bisa dibuka: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Angka protokol dikunci: kalau ada yang menggesernya, jalur Goggles 3 di
    /// macOS gagal di hardware yang tidak ada di meja kami.
    #[test]
    fn angka_protokol_tetap() {
        assert_eq!(MSG_INIT, 0x0000_0002);
        assert_eq!(MSG_INIT_C, 0x8000_0002);
        assert_eq!(MSG_QUERY, 0x0000_0004);
        assert_eq!(MSG_SET, 0x0000_0005);
        assert_eq!(MSG_PACKET, 0x0000_0001);
        assert_eq!(OID_802_3_PERMANENT_ADDRESS, 0x0101_0101);
        assert_eq!(OID_CURRENT_PACKET_FILTER, 0x0001_010E);
        assert_eq!(KELAS_KONTROL, (0xE0, 0x01, 0x03));
        assert_eq!(KELAS_DATA, 0x0A);
        assert_eq!(REQ_SEND, 0x00);
        assert_eq!(REQ_GET, 0x01);
    }

    #[test]
    fn init_dibangun_sesuai_spesifikasi() {
        let b = bangun_init(7);
        assert_eq!(b.len(), 24);
        assert_eq!(u32le(&b, 0), MSG_INIT);
        assert_eq!(u32le(&b, 4), 24, "msg_len harus panjang pesan sendiri");
        assert_eq!(u32le(&b, 8), 7);
        assert_eq!(u32le(&b, 12), 1, "versi mayor RNDIS");
        assert_eq!(u32le(&b, 20), 0x8000);
    }

    /// Vektor ini adalah bentuk yang dilaporkan gadget RNDIS Linux - yang
    /// dipakai Goggles 3 - dengan nilai yang khas: `MaxPacketsPerTransfer` 1,
    /// `MaxTransferSize` MTU+44+22 = 1566, `PacketAlignmentFactor` 0.
    ///
    /// Ini tes regresi, bukan tes tulis-ulang: versi pertama membaca
    /// `MaxTransferSize` dari byte ke-40 (yang sebenarnya alignment) dan
    /// lolos tes buatannya sendiri karena vektornya ikut salah. Di hardware ia
    /// menghasilkan "max transfer 1024" - angka bulat yang tampak masuk akal.
    /// Dengan vektor realistis ini, offset yang salah menghasilkan 0, dan 0
    /// tidak bisa disalahartikan.
    #[test]
    fn init_cmplt_dibaca_di_offset_yang_benar() {
        let mut b = [0u8; 52];
        taruh(&mut b, 0, MSG_INIT_C);
        taruh(&mut b, 4, 52);
        taruh(&mut b, 8, 1);
        taruh(&mut b, 12, STATUS_SUCCESS);
        taruh(&mut b, 16, 1); // versi mayor
        taruh(&mut b, 24, 0); // device flags
        taruh(&mut b, 28, 0); // medium 802.3
        taruh(&mut b, 32, 1); // max paket per transfer
        taruh(&mut b, 36, 1566); // max transfer size
        taruh(&mut b, 40, 0); // packet alignment factor
        assert_eq!(
            urai_init_cmplt(&b),
            Ok(Init { max_transfer: 1566, max_paket: 1, alignment: 0, medium: 0 })
        );

        // Dan kuncinya: alignment TIDAK boleh terbaca sebagai max transfer.
        taruh(&mut b, 40, 0x2000);
        let i = urai_init_cmplt(&b).unwrap();
        assert_eq!(i.max_transfer, 1566, "max transfer terbaca dari medan alignment");
        assert_eq!(i.alignment, 0x2000);
    }

    #[test]
    fn init_gagal_kalau_status_bukan_sukses() {
        let mut b = [0u8; 52];
        taruh(&mut b, 0, MSG_INIT_C);
        taruh(&mut b, 12, 0xC000_0001);
        let e = urai_init_cmplt(&b).unwrap_err();
        assert!(e.contains("c0000001"), "{e}");
        // Pesan lain sama sekali juga harus ditolak, bukan diterima diam-diam.
        let mut lain = [0u8; 52];
        taruh(&mut lain, 0, MSG_SET_C);
        assert!(urai_init_cmplt(&lain).is_err());
        assert!(urai_init_cmplt(&[0u8; 8]).unwrap_err().contains("pendek"));
    }

    /// Offset data di QUERY_CMPLT dihitung dari byte ke-8. Vektor ini menaruh
    /// MAC di posisi yang benar dan sengaja menyisakan byte lain di sekitarnya,
    /// supaya pembacaan yang bergeser terlihat.
    #[test]
    fn mac_perangkat_dipetik_dari_query_cmplt() {
        let mut b = vec![0u8; 32];
        taruh(&mut b, 0, MSG_QUERY_C);
        taruh(&mut b, 4, 32);
        taruh(&mut b, 8, 3);
        taruh(&mut b, 12, STATUS_SUCCESS);
        taruh(&mut b, 16, 6); // panjang data
        taruh(&mut b, 20, 16); // offset dari byte ke-8 -> data di byte 24
        b[24..30].copy_from_slice(&[0x02, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert_eq!(urai_query_cmplt(&b).unwrap(), &[0x02, 0x11, 0x22, 0x33, 0x44, 0x55]);
    }

    #[test]
    fn query_cmplt_yang_menunjuk_keluar_buffer_ditolak() {
        let mut b = vec![0u8; 24];
        taruh(&mut b, 0, MSG_QUERY_C);
        taruh(&mut b, 12, STATUS_SUCCESS);
        taruh(&mut b, 16, 64); // panjang mustahil
        taruh(&mut b, 20, 16);
        assert!(urai_query_cmplt(&b).unwrap_err().contains("keluar buffer"));
    }

    #[test]
    fn set_membawa_muatan_oid() {
        let filter: u32 = FILTER_DIRECTED | FILTER_BROADCAST | FILTER_ALL_MULTICAST;
        assert_eq!(filter, 0x0D, "directed|broadcast|all-multicast");
        let b = bangun_set(9, OID_CURRENT_PACKET_FILTER, &filter.to_le_bytes());
        assert_eq!(b.len(), 32);
        assert_eq!(u32le(&b, 0), MSG_SET);
        assert_eq!(u32le(&b, 4), 32);
        assert_eq!(u32le(&b, 12), OID_CURRENT_PACKET_FILTER);
        assert_eq!(u32le(&b, 16), 4);
        assert_eq!(u32le(&b, 20), 20);
        assert_eq!(u32le(&b, 28), 0x0D);
    }

    #[test]
    fn set_cmplt_diperiksa_statusnya() {
        let mut ok = [0u8; 16];
        taruh(&mut ok, 0, MSG_SET_C);
        taruh(&mut ok, 12, STATUS_SUCCESS);
        assert!(urai_set_cmplt(&ok).is_ok());
        taruh(&mut ok, 12, 0xC000_00BB);
        assert!(urai_set_cmplt(&ok).is_err());
    }

    /// Bungkus lalu buka: frame yang keluar harus identik byte demi byte, dan
    /// header-nya 44 byte dengan `data_offset` 36.
    #[test]
    fn paket_bulak_balik_utuh() {
        let eth: Vec<u8> = (0..100u8).collect();
        let p = bangun_paket(&eth);
        assert_eq!(p.len(), 44 + 100);
        assert_eq!(u32le(&p, 8), 36, "data_offset dihitung dari byte ke-8");
        assert_eq!(u32le(&p, 12), 100);
        let mut keluar = Vec::new();
        untuk_tiap_frame(&p, |f| keluar.push(f.to_vec()));
        assert_eq!(keluar, vec![eth]);
    }

    /// Beberapa pesan dalam satu transfer harus terbaca SEMUA. Kalau hanya yang
    /// pertama, video patah-patah sementara penghitung kehilangan tetap nol -
    /// kegagalan yang tidak menunjuk ke sebabnya.
    #[test]
    fn beberapa_paket_dalam_satu_transfer_terbaca_semua() {
        let mut buf = Vec::new();
        buf.extend(bangun_paket(&[1, 2, 3]));
        buf.extend(bangun_paket(&[4, 5]));
        buf.extend(bangun_paket(&[6]));
        let mut keluar = Vec::new();
        untuk_tiap_frame(&buf, |f| keluar.push(f.to_vec()));
        assert_eq!(keluar, vec![vec![1, 2, 3], vec![4, 5], vec![6]]);
    }

    /// Pesan bukan-PACKET dilewati, dan pesan rusak menghentikan pembacaan
    /// tanpa panik - keduanya jalur yang benar-benar akan terjadi di kabel.
    #[test]
    fn pesan_lain_dilewati_dan_yang_rusak_tidak_memanikkan() {
        let mut buf = Vec::new();
        buf.extend(bangun_set(1, OID_CURRENT_PACKET_FILTER, &[0, 0, 0, 0]));
        buf.extend(bangun_paket(&[7, 7]));
        let mut keluar = Vec::new();
        untuk_tiap_frame(&buf, |f| keluar.push(f.to_vec()));
        assert_eq!(keluar, vec![vec![7, 7]]);

        // Panjang mustahil: berhenti, jangan menebak.
        let mut rusak = bangun_paket(&[1, 2, 3]);
        taruh(&mut rusak, 4, 0xFFFF);
        let mut n = 0;
        untuk_tiap_frame(&rusak, |_| n += 1);
        assert_eq!(n, 0);

        // Panjang nol akan membuat pemindaian berputar selamanya kalau tidak
        // ditolak - ini penjaga terhadap hang, bukan terhadap data hilang.
        let mut nol = vec![0u8; 16];
        taruh(&mut nol, 0, MSG_PACKET);
        taruh(&mut nol, 4, 0);
        untuk_tiap_frame(&nol, |_| panic!("tidak boleh ada frame"));

        untuk_tiap_frame(&[], |_| panic!("buffer kosong tidak punya frame"));
        untuk_tiap_frame(&[1, 2, 3], |_| panic!("potongan lebih pendek dari header"));
    }
}
