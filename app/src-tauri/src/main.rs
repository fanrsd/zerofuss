// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// `--uji-rndis` menjalankan jalur Goggles 3 / N3 user space langkah demi
/// langkah dan mencetak hasil tiap tahap, lalu keluar.
///
/// Ada di biner yang sama, bukan di crate uji terpisah seperti `uji-usb/`, dan
/// itu keputusan: jalur ini butuh SELURUH tumpukan (`rndis` + `net` + protokol
/// DJI), jadi menyalinnya ke crate lain berarti dua salinan yang harus dijaga
/// sinkron - dan yang disalin justru bagian yang paling belum terbukti.
///
/// Di macOS ia bisa dijalankan langsung dari bundle yang sudah dipasang:
///
///   /Applications/Zerofuss.app/Contents/MacOS/zerofuss --uji-rndis
fn main() {
    if std::env::args().any(|a| a == "--uji-rndis") {
        std::process::exit(zerofuss_lib::uji_rndis());
    }
    zerofuss_lib::run()
}
