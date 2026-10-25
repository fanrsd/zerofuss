//! Tumpukan jaringan seukuran kebutuhan: Ethernet + ARP + IPv4 + UDP, satu
//! aliran, di dalam proses.
//!
//! Ini pasangan `rndis.rs`. Setelah tautan RNDIS hidup, yang tersisa adalah
//! mengisi frame Ethernet-nya sendiri, karena tidak ada OS yang melakukannya
//! untuk kita di jalur ini.
//!
//! Kenapa bukan `utun` seperti dua proyek tethering yang jadi acuan
//! (JellyBrick/ReRNDIS, hknsglm/android-usb-tether-macos): mereka menyalurkan
//! SELURUH jaringan mesin lewat perangkat, jadi mereka wajib membuat antarmuka
//! virtual dan menyentuh routing serta DNS - dan itu butuh root. Zerofuss cuma
//! butuh SATU percakapan UDP dengan `192.168.60.2:9003`. Membuat antarmuka
//! sistem untuk itu berarti meminta hak admin, mengubah tabel rute mesin
//! pengguna, dan menambahkan satu keadaan yang bisa tertinggal kotor kalau
//! aplikasi mati - semuanya demi satu soket. Tiga ratus baris di bawah ini
//! menggantikan seluruh kelas masalah itu.
//!
//! Yang SENGAJA tidak ada di sini, karena jalur ini tidak memakainya:
//! TCP, DHCP (goggles tidak melayaninya - itu sebabnya jalur Windows perlu
//! alamat statis), IP fragmentation (MTU jalurnya 1472, dan paket video
//! terbesar masih di bawah itu), IPv6, dan ICMP.
//!
//! Semua fungsi di berkas ini murni - masukan byte, keluaran byte - jadi
//! seluruhnya teruji tanpa hardware, tanpa Mac, dan tanpa goggles.

use std::net::Ipv4Addr;

pub const ETH_HDR: usize = 14;
const ETH_ARP: u16 = 0x0806;
const ETH_IPV4: u16 = 0x0800;
const IP_UDP: u8 = 17;
const BROADCAST: [u8; 6] = [0xff; 6];

/// MAC host kami: locally administered (bit 0x02 di oktet pertama), unicast.
///
/// Dikarang sendiri dan itu benar: sisi host tidak punya perangkat fisik apa
/// pun di jalur ini, jadi tidak ada MAC "asli" untuk dipakai. Yang wajib hanya
/// dua hal - bit locally-administered menyala supaya tidak menabrak ruang OUI
/// milik vendor mana pun, dan nilainya tetap antar sesi supaya cache ARP di
/// goggles tidak perlu diperbarui setiap kali aplikasi dibuka.
pub const MAC_KITA: [u8; 6] = [0x02, 0x5A, 0x46, 0x00, 0x00, 0x01];

fn be16(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}

/// Checksum internet (RFC 1071): jumlah 16-bit one's complement.
///
/// Panjang ganjil ditangani dengan menganggap byte terakhir sebagai oktet
/// tinggi - itu bukan kelonggaran, itu aturannya, dan muatan UDP memang boleh
/// berpanjang ganjil.
pub fn checksum(bagian: &[&[u8]]) -> u16 {
    let mut jumlah: u32 = 0;
    let mut ganjil: Option<u8> = None;
    for b in bagian {
        let mut i = 0;
        // Sisa byte dari bagian sebelumnya dipasangkan dengan byte pertama di
        // sini: bagian-bagiannya adalah satu aliran, bukan blok terpisah.
        if let Some(hi) = ganjil.take() {
            let lo = if b.is_empty() { 0 } else { i += 1; b[0] };
            jumlah += u32::from(u16::from_be_bytes([hi, lo]));
        }
        while i + 1 < b.len() {
            jumlah += u32::from(be16(b, i));
            i += 2;
        }
        if i < b.len() {
            ganjil = Some(b[i]);
        }
    }
    if let Some(hi) = ganjil {
        jumlah += u32::from(u16::from_be_bytes([hi, 0]));
    }
    while jumlah >> 16 != 0 {
        jumlah = (jumlah & 0xffff) + (jumlah >> 16);
    }
    !(jumlah as u16)
}

/// Apa yang datang dari satu frame Ethernet.
#[derive(Debug, PartialEq, Eq)]
pub enum Masuk {
    /// Muatan UDP untuk port kita, beserta port pengirimnya.
    Udp { dari: u16, awal: usize, panjang: usize },
    /// Goggles bertanya siapa pemilik alamat kita - jawabannya sudah dibentuk.
    Balas(Vec<u8>),
    /// ARP reply: MAC peer sudah diketahui sekarang.
    MacPeer,
    /// Bukan untuk kita, atau bukan yang dipakai jalur ini.
    Lain,
}

/// Satu percakapan: alamat kita, alamat goggles, dan MAC peer begitu diketahui.
pub struct Tumpukan {
    pub ip_kita: Ipv4Addr,
    pub ip_peer: Ipv4Addr,
    pub port_kita: u16,
    pub mac_peer: Option<[u8; 6]>,
    ident: u16,
}

impl Tumpukan {
    /// `port_kita` dipilih pemanggil. Nomor port kita tidak pernah ikut di
    /// dalam paket DJI - goggles membalas ke port pengirim - jadi nilainya
    /// bebas; yang penting ia tetap selama sesi.
    pub fn baru(ip_kita: Ipv4Addr, ip_peer: Ipv4Addr, port_kita: u16) -> Tumpukan {
        Tumpukan { ip_kita, ip_peer, port_kita, mac_peer: None, ident: 1 }
    }

    /// Anggap MAC peer sudah diketahui dari sumber lain (mis. MAC perangkat
    /// yang dilaporkan RNDIS).
    ///
    /// Ini jalan pintas yang berguna tapi TIDAK bisa diandalkan sendiri: MAC
    /// yang dilaporkan `OID_802_3_PERMANENT_ADDRESS` adalah MAC fungsi RNDIS,
    /// dan alamat IP `.2` bisa saja dipegang antarmuka lain di dalam goggles
    /// dengan MAC berbeda. Karena itu ARP tetap dikirim, dan jawabannya
    /// menimpa nilai ini.
    pub fn duga_mac_peer(&mut self, mac: [u8; 6]) {
        if self.mac_peer.is_none() && mac != [0; 6] {
            self.mac_peer = Some(mac);
        }
    }

    fn eth(&self, tujuan: [u8; 6], tipe: u16, muatan_len: usize) -> Vec<u8> {
        let mut f = Vec::with_capacity(ETH_HDR + muatan_len);
        f.extend_from_slice(&tujuan);
        f.extend_from_slice(&MAC_KITA);
        f.extend_from_slice(&tipe.to_be_bytes());
        f
    }

    /// ARP request broadcast: "siapa yang punya ip_peer".
    pub fn arp_minta(&self) -> Vec<u8> {
        let mut f = self.eth(BROADCAST, ETH_ARP, 28);
        f.extend_from_slice(&[0, 1]); // hardware: Ethernet
        f.extend_from_slice(&ETH_IPV4.to_be_bytes());
        f.push(6); // panjang MAC
        f.push(4); // panjang IPv4
        f.extend_from_slice(&[0, 1]); // opcode: request
        f.extend_from_slice(&MAC_KITA);
        f.extend_from_slice(&self.ip_kita.octets());
        f.extend_from_slice(&[0; 6]); // MAC tujuan belum diketahui
        f.extend_from_slice(&self.ip_peer.octets());
        f
    }

    fn arp_jawab(&self, mac_penanya: [u8; 6], ip_penanya: [u8; 4]) -> Vec<u8> {
        let mut f = self.eth(mac_penanya, ETH_ARP, 28);
        f.extend_from_slice(&[0, 1]);
        f.extend_from_slice(&ETH_IPV4.to_be_bytes());
        f.push(6);
        f.push(4);
        f.extend_from_slice(&[0, 2]); // opcode: reply
        f.extend_from_slice(&MAC_KITA);
        f.extend_from_slice(&self.ip_kita.octets());
        f.extend_from_slice(&mac_penanya);
        f.extend_from_slice(&ip_penanya);
        f
    }

    /// Bungkus muatan UDP jadi frame Ethernet siap kirim. `None` kalau MAC peer
    /// belum diketahui - pemanggil harus mengirim ARP dulu, bukan mengarang
    /// tujuan.
    pub fn bungkus_udp(&mut self, port_tujuan: u16, muatan: &[u8]) -> Option<Vec<u8>> {
        let mac = self.mac_peer?;
        let udp_len = 8 + muatan.len();
        let total = 20 + udp_len;

        let mut f = self.eth(mac, ETH_IPV4, total);
        let awal_ip = f.len();

        self.ident = self.ident.wrapping_add(1);
        f.push(0x45); // versi 4, header 20 byte
        f.push(0); // DSCP/ECN
        f.extend_from_slice(&(total as u16).to_be_bytes());
        f.extend_from_slice(&self.ident.to_be_bytes());
        // Flags/fragment 0: jalur ini tidak pernah memotong paket. DF sengaja
        // TIDAK diset - tidak ada router di kabel ini yang perlu memberi tahu
        // apa pun soal MTU.
        f.extend_from_slice(&[0, 0]);
        f.push(64); // TTL
        f.push(IP_UDP);
        f.extend_from_slice(&[0, 0]); // checksum, diisi setelah header lengkap
        f.extend_from_slice(&self.ip_kita.octets());
        f.extend_from_slice(&self.ip_peer.octets());
        let ck = checksum(&[&f[awal_ip..awal_ip + 20]]);
        f[awal_ip + 10..awal_ip + 12].copy_from_slice(&ck.to_be_bytes());

        let awal_udp = f.len();
        f.extend_from_slice(&self.port_kita.to_be_bytes());
        f.extend_from_slice(&port_tujuan.to_be_bytes());
        f.extend_from_slice(&(udp_len as u16).to_be_bytes());
        f.extend_from_slice(&[0, 0]); // checksum
        f.extend_from_slice(muatan);

        // Checksum UDP di IPv4 sebenarnya opsional (nol = tidak dihitung), tapi
        // dihitung di sini: biayanya satu lintasan atas paket kecil, dan kalau
        // firmware goggles ternyata membuang paket berchecksum nol, gejalanya
        // adalah stream yang tidak pernah mulai tanpa satu pun petunjuk.
        let (a, b) = (self.ip_kita.octets(), self.ip_peer.octets());
        let panjang = (udp_len as u16).to_be_bytes();
        let bagian: [&[u8]; 5] =
            [&a, &b, &[0, IP_UDP], &panjang, &f[awal_udp..]];
        let ck = checksum(&bagian);
        // Nol punya arti khusus ("tidak dihitung"), jadi hasil nol dikirim
        // sebagai 0xFFFF - keduanya setara di aritmetika one's complement.
        let ck = if ck == 0 { 0xffff } else { ck };
        f[awal_udp + 6..awal_udp + 8].copy_from_slice(&ck.to_be_bytes());
        Some(f)
    }

    /// Telan satu frame masuk.
    ///
    /// Muatan UDP dikembalikan sebagai rentang di dalam `frame`, bukan sebagai
    /// `Vec` baru: jalur ini melewatkan belasan megabit per detik, dan satu
    /// alokasi per paket video adalah biaya yang tidak perlu ada.
    pub fn telan(&mut self, frame: &[u8]) -> Masuk {
        if frame.len() < ETH_HDR {
            return Masuk::Lain;
        }
        // Frame untuk MAC lain diabaikan. Filter RNDIS sudah menyaring di sisi
        // perangkat, tapi ALL_MULTICAST ikut dinyalakan supaya ARP lewat, jadi
        // penyaringan di sini tetap perlu.
        let tujuan = &frame[0..6];
        if tujuan != MAC_KITA && tujuan != BROADCAST && tujuan[0] & 1 == 0 {
            return Masuk::Lain;
        }
        match be16(frame, 12) {
            ETH_ARP => self.arp(frame),
            ETH_IPV4 => self.ipv4(frame),
            _ => Masuk::Lain,
        }
    }

    fn arp(&mut self, frame: &[u8]) -> Masuk {
        let p = &frame[ETH_HDR..];
        if p.len() < 28 {
            return Masuk::Lain;
        }
        let mac_pengirim: [u8; 6] = p[8..14].try_into().unwrap();
        let ip_pengirim = Ipv4Addr::new(p[14], p[15], p[16], p[17]);
        let ip_tujuan = Ipv4Addr::new(p[24], p[25], p[26], p[27]);
        match be16(p, 6) {
            // Request untuk alamat kita: jawab. Tanpa ini goggles tidak pernah
            // tahu ke MAC mana video harus dikirim, dan seluruh jalur diam
            // walau kita rajin mengirim.
            1 if ip_tujuan == self.ip_kita => {
                if ip_pengirim == self.ip_peer {
                    self.mac_peer = Some(mac_pengirim);
                }
                Masuk::Balas(self.arp_jawab(mac_pengirim, p[14..18].try_into().unwrap()))
            }
            2 if ip_pengirim == self.ip_peer => {
                self.mac_peer = Some(mac_pengirim);
                Masuk::MacPeer
            }
            _ => Masuk::Lain,
        }
    }

    fn ipv4(&mut self, frame: &[u8]) -> Masuk {
        let p = &frame[ETH_HDR..];
        if p.len() < 20 || p[0] >> 4 != 4 {
            return Masuk::Lain;
        }
        let ihl = ((p[0] & 0x0f) as usize) * 4;
        if ihl < 20 || p.len() < ihl || p[9] != IP_UDP {
            return Masuk::Lain;
        }
        // Fragmen dibuang: jalur ini tidak pernah menghasilkannya, jadi
        // fragmen berarti ada yang tidak kami pahami - dan menyatukannya
        // dengan menebak lebih buruk daripada membuangnya.
        if be16(p, 6) & 0x3fff != 0 {
            return Masuk::Lain;
        }
        let sumber = Ipv4Addr::new(p[12], p[13], p[14], p[15]);
        let tujuan = Ipv4Addr::new(p[16], p[17], p[18], p[19]);
        if sumber != self.ip_peer || tujuan != self.ip_kita {
            return Masuk::Lain;
        }
        // Panjang total di header yang menentukan, bukan panjang frame:
        // Ethernet memberi padding sampai 60 byte, dan paket keepalive DJI
        // lebih pendek dari itu - tanpa pemotongan ini muatan yang diteruskan
        // ikut membawa padding nol.
        let total = be16(p, 2) as usize;
        let akhir = total.min(p.len());
        let udp = p.get(ihl..akhir).unwrap_or(&[]);
        if udp.len() < 8 {
            return Masuk::Lain;
        }
        if be16(udp, 2) != self.port_kita {
            return Masuk::Lain;
        }
        let udp_len = (be16(udp, 4) as usize).max(8).min(udp.len());
        Masuk::Udp {
            dari: be16(udp, 0),
            awal: ETH_HDR + ihl + 8,
            panjang: udp_len - 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEER: Ipv4Addr = Ipv4Addr::new(192, 168, 60, 2);
    const KITA: Ipv4Addr = Ipv4Addr::new(192, 168, 60, 1);
    const MAC_PEER: [u8; 6] = [0x9e, 0x11, 0x22, 0x33, 0x44, 0x55];

    fn tumpukan() -> Tumpukan {
        let mut t = Tumpukan::baru(KITA, PEER, 40000);
        t.mac_peer = Some(MAC_PEER);
        t
    }

    /// Vektor dari RFC 1071 bagian 3: jumlah checksum atas 8 byte contoh.
    /// Angka ini bukan hasil implementasi sendiri, jadi ia benar-benar menguji.
    #[test]
    fn checksum_cocok_dengan_vektor_rfc1071() {
        let data = [0x00u8, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(checksum(&[&data]), 0x220d);
    }

    /// Sifat yang menentukan: menghitung ulang checksum atas header yang SUDAH
    /// memuat checksum-nya harus menghasilkan nol. Ini yang dilakukan penerima,
    /// jadi kalau tes ini lolos, header kita lolos di sisi goggles.
    #[test]
    fn header_ip_yang_dibangun_lolos_verifikasi_penerima() {
        let f = tumpukan().bungkus_udp(9003, &[1, 2, 3, 4, 5]).unwrap();
        let ip = &f[ETH_HDR..ETH_HDR + 20];
        assert_eq!(checksum(&[ip]), 0, "checksum header IP tidak sah");
    }

    #[test]
    fn checksum_udp_lolos_verifikasi_penerima() {
        let muatan = [0xaau8; 37]; // panjang ganjil, jalur yang mudah salah
        let f = tumpukan().bungkus_udp(9003, &muatan).unwrap();
        let ip = &f[ETH_HDR..];
        let udp = &ip[20..];
        let pseudo: [&[u8]; 4] = [
            &KITA.octets(),
            &PEER.octets(),
            &[0, IP_UDP],
            &(udp.len() as u16).to_be_bytes(),
        ];
        let mut bagian: Vec<&[u8]> = pseudo.to_vec();
        bagian.push(udp);
        assert_eq!(checksum(&bagian), 0, "checksum UDP tidak sah");
    }

    /// Panjang ganjil di tengah rangkaian bagian: byte terakhir satu bagian
    /// harus berpasangan dengan byte pertama bagian berikutnya, bukan dianggap
    /// akhir aliran. Salah di sini menghasilkan checksum yang salah HANYA untuk
    /// muatan berpanjang tertentu - bug yang muncul sesekali dan sulit dilacak.
    #[test]
    fn checksum_menyambung_bagian_berpanjang_ganjil() {
        let utuh = [1u8, 2, 3, 4, 5];
        assert_eq!(checksum(&[&utuh]), checksum(&[&utuh[..1], &utuh[1..]]));
        assert_eq!(checksum(&[&utuh]), checksum(&[&utuh[..3], &utuh[3..]]));
        assert_eq!(checksum(&[&utuh]), checksum(&[&utuh[..2], &[], &utuh[2..]]));
    }

    #[test]
    fn frame_udp_bulak_balik() {
        let mut t = tumpukan();
        let muatan: Vec<u8> = (0..200u8).collect();
        let mut f = t.bungkus_udp(9003, &muatan).unwrap();
        // Balikkan arahnya: paket yang kita bangun, dikirim balik ke kita.
        f[0..6].copy_from_slice(&MAC_KITA);
        f[6..12].copy_from_slice(&MAC_PEER);
        f[ETH_HDR + 12..ETH_HDR + 16].copy_from_slice(&PEER.octets());
        f[ETH_HDR + 16..ETH_HDR + 20].copy_from_slice(&KITA.octets());
        f[ETH_HDR + 20..ETH_HDR + 22].copy_from_slice(&9003u16.to_be_bytes());
        f[ETH_HDR + 22..ETH_HDR + 24].copy_from_slice(&40000u16.to_be_bytes());
        match t.telan(&f) {
            Masuk::Udp { dari, awal, panjang } => {
                assert_eq!(dari, 9003);
                assert_eq!(&f[awal..awal + panjang], &muatan[..]);
            }
            lain => panic!("{lain:?}"),
        }
    }

    /// Padding Ethernet (frame minimum 60 byte) tidak boleh ikut jadi muatan.
    /// Paket kontrol DJI panjangnya 34-46 byte, jadi ini bukan kasus teoretis:
    /// muatan yang membawa ekor nol akan gagal diurai di `pipeline`.
    #[test]
    fn padding_ethernet_tidak_ikut_jadi_muatan() {
        let mut t = tumpukan();
        let muatan = [7u8; 4];
        let mut f = t.bungkus_udp(9003, &muatan).unwrap();
        f[0..6].copy_from_slice(&MAC_KITA);
        f[ETH_HDR + 12..ETH_HDR + 16].copy_from_slice(&PEER.octets());
        f[ETH_HDR + 16..ETH_HDR + 20].copy_from_slice(&KITA.octets());
        f[ETH_HDR + 22..ETH_HDR + 24].copy_from_slice(&40000u16.to_be_bytes());
        while f.len() < 60 {
            f.push(0); // padding, seperti yang dilakukan hardware Ethernet
        }
        match t.telan(&f) {
            Masuk::Udp { panjang, .. } => assert_eq!(panjang, 4),
            lain => panic!("{lain:?}"),
        }
    }

    /// Yang bukan milik percakapan ini harus jatuh ke `Lain` - bukan diteruskan
    /// ke decoder sebagai video.
    #[test]
    fn paket_asing_ditolak() {
        let mut t = tumpukan();
        let bikin = |ip_sumber: Ipv4Addr, port: u16| {
            let mut u = Tumpukan::baru(ip_sumber, KITA, 9003);
            u.mac_peer = Some(MAC_KITA);
            let mut f = u.bungkus_udp(port, &[1, 2, 3]).unwrap();
            f[0..6].copy_from_slice(&MAC_KITA);
            f
        };
        // Port tujuan salah.
        assert_eq!(t.telan(&bikin(PEER, 40001)), Masuk::Lain);
        // Alamat sumber bukan goggles.
        assert_eq!(t.telan(&bikin(Ipv4Addr::new(192, 168, 60, 9), 40000)), Masuk::Lain);
        // Bukan IPv4 sama sekali.
        let mut lain = vec![0u8; 60];
        lain[0..6].copy_from_slice(&MAC_KITA);
        lain[12..14].copy_from_slice(&0x86ddu16.to_be_bytes()); // IPv6
        assert_eq!(t.telan(&lain), Masuk::Lain);
        // Terlalu pendek.
        assert_eq!(t.telan(&[0; 8]), Masuk::Lain);
    }

    /// ARP request dari goggles WAJIB dijawab: tanpa itu goggles tidak punya
    /// MAC tujuan, dan seluruh jalur diam walau kita rajin mengirim.
    #[test]
    fn arp_request_dijawab_dan_mac_peer_dicatat() {
        let mut t = Tumpukan::baru(KITA, PEER, 40000);
        assert!(t.mac_peer.is_none());
        assert!(t.bungkus_udp(9003, &[1]).is_none(), "tanpa MAC peer jangan mengarang tujuan");

        let mut minta = Tumpukan::baru(PEER, KITA, 0).arp_minta();
        // arp_minta memakai MAC_KITA sebagai pengirim; ganti supaya terlihat
        // datang dari goggles.
        minta[6..12].copy_from_slice(&MAC_PEER);
        minta[22..28].copy_from_slice(&MAC_PEER);

        match t.telan(&minta) {
            Masuk::Balas(j) => {
                assert_eq!(&j[0..6], &MAC_PEER, "jawaban harus ke penanya");
                assert_eq!(&j[6..12], &MAC_KITA);
                assert_eq!(be16(&j, 12), ETH_ARP);
                assert_eq!(be16(&j, ETH_HDR + 6), 2, "opcode reply");
                assert_eq!(&j[ETH_HDR + 8..ETH_HDR + 14], &MAC_KITA);
                assert_eq!(&j[ETH_HDR + 14..ETH_HDR + 18], &KITA.octets());
            }
            lain => panic!("{lain:?}"),
        }
        assert_eq!(t.mac_peer, Some(MAC_PEER));
        assert!(t.bungkus_udp(9003, &[1]).is_some());
    }

    #[test]
    fn arp_reply_mengisi_mac_peer() {
        let mut t = Tumpukan::baru(KITA, PEER, 40000);
        let mut balas = t.arp_minta();
        balas[0..6].copy_from_slice(&MAC_KITA);
        balas[6..12].copy_from_slice(&MAC_PEER);
        balas[ETH_HDR + 6..ETH_HDR + 8].copy_from_slice(&2u16.to_be_bytes());
        balas[ETH_HDR + 8..ETH_HDR + 14].copy_from_slice(&MAC_PEER);
        balas[ETH_HDR + 14..ETH_HDR + 18].copy_from_slice(&PEER.octets());
        assert_eq!(t.telan(&balas), Masuk::MacPeer);
        assert_eq!(t.mac_peer, Some(MAC_PEER));
    }

    /// ARP request kami sendiri harus berbentuk benar: broadcast, opcode 1,
    /// dan target IP goggles. Kalau salah satunya keliru, goggles tidak akan
    /// pernah menjawab dan jalur ini berhenti sebelum mulai.
    #[test]
    fn arp_minta_berbentuk_benar() {
        let f = Tumpukan::baru(KITA, PEER, 0).arp_minta();
        assert_eq!(f.len(), ETH_HDR + 28);
        assert_eq!(&f[0..6], &[0xff; 6]);
        assert_eq!(&f[6..12], &MAC_KITA);
        assert_eq!(be16(&f, 12), ETH_ARP);
        assert_eq!(be16(&f, ETH_HDR), 1, "hardware Ethernet");
        assert_eq!(be16(&f, ETH_HDR + 2), ETH_IPV4);
        assert_eq!(f[ETH_HDR + 4], 6);
        assert_eq!(f[ETH_HDR + 5], 4);
        assert_eq!(be16(&f, ETH_HDR + 6), 1, "opcode request");
        assert_eq!(&f[ETH_HDR + 14..ETH_HDR + 18], &KITA.octets());
        assert_eq!(&f[ETH_HDR + 24..ETH_HDR + 28], &PEER.octets());
    }

    /// MAC dari RNDIS hanya dugaan awal, dan ARP harus bisa menimpanya.
    #[test]
    fn dugaan_mac_ditimpa_jawaban_arp() {
        let mut t = Tumpukan::baru(KITA, PEER, 40000);
        t.duga_mac_peer([0; 6]);
        assert!(t.mac_peer.is_none(), "MAC nol bukan dugaan yang sah");
        t.duga_mac_peer([1, 2, 3, 4, 5, 6]);
        assert_eq!(t.mac_peer, Some([1, 2, 3, 4, 5, 6]));
        t.duga_mac_peer(MAC_PEER);
        assert_eq!(t.mac_peer, Some([1, 2, 3, 4, 5, 6]), "dugaan kedua tidak menimpa");

        let mut balas = t.arp_minta();
        balas[0..6].copy_from_slice(&MAC_KITA);
        balas[ETH_HDR + 6..ETH_HDR + 8].copy_from_slice(&2u16.to_be_bytes());
        balas[ETH_HDR + 8..ETH_HDR + 14].copy_from_slice(&MAC_PEER);
        balas[ETH_HDR + 14..ETH_HDR + 18].copy_from_slice(&PEER.octets());
        t.telan(&balas);
        assert_eq!(t.mac_peer, Some(MAC_PEER), "jawaban ARP harus menimpa dugaan");
    }

    /// Fragmen IP dibuang, bukan diteruskan setengah - muatan setengah paket
    /// akan terbaca sebagai video rusak, bukan sebagai galat.
    #[test]
    fn fragmen_ip_dibuang() {
        let mut t = tumpukan();
        let mut f = t.bungkus_udp(9003, &[1; 40]).unwrap();
        f[0..6].copy_from_slice(&MAC_KITA);
        f[ETH_HDR + 12..ETH_HDR + 16].copy_from_slice(&PEER.octets());
        f[ETH_HDR + 16..ETH_HDR + 20].copy_from_slice(&KITA.octets());
        f[ETH_HDR + 22..ETH_HDR + 24].copy_from_slice(&40000u16.to_be_bytes());
        f[ETH_HDR + 6] = 0x20; // fragment offset bukan nol
        assert_eq!(t.telan(&f), Masuk::Lain);
    }
}
