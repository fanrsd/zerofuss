//! Uji jalur USB bulk terhadap goggles yang tercolok.
//!
//! Menjalankan langkah yang sama persis dengan `app/src-tauri/src/usb.rs`:
//! enumerasi -> open -> claim interface 3 -> kirim RMVT -> baca 0x84 -> pancing
//! ulang saat sunyi. Gunanya menjawab satu pertanyaan tanpa harus membuka
//! seluruh aplikasi: apakah goggles ini melayani jalur bulk.

use nusb::transfer::{Bulk, In, Out};
use nusb::MaybeFuture;
use std::time::{Duration, Instant};

const IFACE: u8 = 3;
const EP_OUT: u8 = 0x03;
const EP_IN: u8 = 0x84;
const MAGIC: [u8; 4] = [0x52, 0x4d, 0x56, 0x54]; // "RMVT"

fn main() {
    let t0 = Instant::now();
    for _ in 0..20 {
        let _ = nusb::list_devices().wait().map(|l| l.count());
    }
    println!("biaya enumerasi : {:.2} ms per probe", t0.elapsed().as_secs_f64() * 50.0);

    let Ok(list) = nusb::list_devices().wait() else {
        println!("USB tidak bisa dibaca");
        return;
    };
    let Some(info) = list.into_iter().find(|d| d.vendor_id() == 0x2ca3) else {
        println!("tidak ada goggles DJI tercolok");
        return;
    };
    println!("device          : VID {:04x} PID {:04x}", info.vendor_id(), info.product_id());
    println!("produk          : {:?}", info.product_string());
    println!(
        "keluarga        : {}",
        match info.product_id() {
            0x001f => "FPV Goggles V1/V2 -> jalur bulk (RMVT harus JALAN)",
            0x0020 => "Goggles 3 / N3 -> jalur RNDIS (IF3 diam, lihat README)",
            _ => "tidak dikenal",
        }
    );

    let dev = match info.open().wait() {
        Ok(d) => {
            println!("open            : OK");
            d
        }
        Err(e) => {
            println!("open            : GAGAL kind={:?}", e.kind());
            return;
        }
    };
    let iface = match dev.claim_interface(IFACE).wait() {
        Ok(i) => {
            println!("claim           : OK (interface {IFACE})");
            i
        }
        Err(e) => {
            let tanpa_driver =
                matches!(e.kind(), nusb::ErrorKind::Unsupported | nusb::ErrorKind::NotFound);
            println!("claim           : GAGAL kind={:?}", e.kind());
            println!(
                "                  {}",
                if tanpa_driver {
                    "WinUSB belum di-bind ke interface 3 - pasang lewat Zadig"
                } else {
                    "interface sedang dipakai program lain, bukan soal driver"
                }
            );
            return;
        }
    };

    let mut keluar = iface.endpoint::<Bulk, Out>(EP_OUT).expect("endpoint OUT");
    let mut masuk = iface.endpoint::<Bulk, In>(EP_IN).expect("endpoint IN");
    println!(
        "endpoint        : OUT {EP_OUT:#04x} max {} | IN {EP_IN:#04x} max {}",
        keluar.max_packet_size(),
        masuk.max_packet_size()
    );

    match keluar.transfer_blocking(MAGIC.to_vec().into(), Duration::from_secs(3)).into_result() {
        Ok(_) => println!("RMVT            : 4 byte terkirim"),
        Err(e) => println!("RMVT            : GAGAL {e} <- endpoint OUT tidak menerima tulisan"),
    }

    while masuk.pending() < 8 {
        let b = masuk.allocate(64 * 1024);
        masuk.submit(b);
    }
    println!("antrean         : {} transfer menggantung", masuk.pending());

    let (mut total, mut potongan, mut sunyi) = (0usize, 0usize, 0usize);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(5) {
        match masuk.wait_next_complete(Duration::from_millis(1000)) {
            None => {
                sunyi += 1;
                let _ = keluar
                    .transfer_blocking(MAGIC.to_vec().into(), Duration::from_millis(500));
            }
            Some(c) => {
                let n = c.buffer.len();
                if potongan == 0 && n > 0 {
                    let awal: Vec<String> =
                        c.buffer[..n.min(8)].iter().map(|b| format!("{b:02x}")).collect();
                    println!("data            : potongan pertama {n} byte, awal {}", awal.join(" "));
                }
                let st = c.status;
                masuk.submit(c.buffer);
                if let Err(e) = st {
                    println!("baca            : galat {e}");
                    break;
                }
                if n == 0 {
                    sunyi += 1;
                } else {
                    total += n;
                    potongan += 1;
                }
            }
        }
    }
    println!("hasil           : {potongan} potongan, {total} byte, {sunyi}x sunyi");
    println!(
        "\nVERDICT: {}",
        if total > 0 {
            "jalur bulk JALAN di goggles ini"
        } else {
            "IF3 diam - goggles ini tidak melayani jalur bulk"
        }
    );
}
