// Zerofuss - sisi tampilan.
//
// Video TIDAK lewat IPC Tauri: 3 MB/frame sebagai JSON tidak masuk akal.
// Rust menyajikan frame NV12 lewat satu respons HTTP chunked di loopback, dan
// di sini dibaca sebagai aliran biner lalu diunggah ke tekstur WebGL. Konversi
// YUV->RGB dan koreksi lensa dikerjakan shader, jadi CPU tidak menyentuh piksel.

const invoke = window.__TAURI__.core.invoke;
const $ = (id) => document.getElementById(id);

// ------------------------------------------------------------------ penggambar

const VERT = `
attribute vec2 p;
varying vec2 v;
void main() {
  v = vec2(p.x * 0.5 + 0.5, 0.5 - p.y * 0.5);
  gl_Position = vec4(p, 0.0, 1.0);
}`;

// BT.709 rentang terbatas (16-235), sesuai keluaran encoder goggles.
// Koreksi lensa: model barrel satu suku, r' = r(1 + k r^2), dihitung di ruang
// yang sudah dikoreksi aspek supaya lengkungannya tidak jadi elips. Gambar
// dikecilkan 1/(1+k) lebih dulu supaya sudutnya tidak pernah keluar bingkai.
const FRAG = `
precision mediump float;
varying vec2 v;
uniform sampler2D ty;
uniform sampler2D tuv;
uniform float k;
uniform float asp;

vec3 yuv(vec2 t) {
  float y = (texture2D(ty, t).r - 0.0627) * 1.1644;
  vec2 c = texture2D(tuv, t).ra - 0.5;
  return vec3(
    y + 1.7927 * c.y,
    y - 0.2132 * c.x - 0.5329 * c.y,
    y + 2.1124 * c.x);
}

void main() {
  vec2 t = v;
  if (k > 0.0001) {
    vec2 scale = vec2(asp, 1.0);
    float norm = 0.5 * length(scale);
    vec2 c = (v - 0.5) * scale / norm / (1.0 + k);
    c *= 1.0 + k * dot(c, c);
    t = c * norm / scale + 0.5;
  }
  gl_FragColor = vec4(yuv(t), 1.0);
}`;

/// Pengaturan tampilan murni - disimpan di webview, tidak perlu bolak-balik ke
/// Rust untuk hal yang tidak menyentuh jaringan.
const view = {
  lens: false,
  lensStr: 35,
  // Bawaannya SELALU ikut sumber: kotak tampil dibuat sama dengan bingkai yang
  // datang, jadi tidak pernah ada peregangan maupun bilah tambahan. `wide`
  // hanya menambahkan satu tindakan opsional - mengisi layar 16:9 dari sumber
  // 4:3 dengan MEMOTONG atas-bawah.
  wide: false,
  clean: false,
  pin: false,
};

function loadView() {
  try {
    const lama = JSON.parse(localStorage.getItem("zerofuss.view") || "{}");
    // Dua bentuk lama pernah ada: boolean `stretch`, lalu pemilih tiga arah
    // `aspect`. Keduanya dipetakan ke `wide`, bukan dibuang - dan keduanya
    // dibuang dari objeknya supaya tidak ikut tersimpan lagi. Pilihan "4:3"
    // yang lama TIDAK dipetakan ke wide: artinya memaksa 4:3, dan itu justru
    // keadaan yang membuat gambar tampil sempit berbilah - bukan yang diminta.
    if (lama.wide === undefined) {
      lama.wide = lama.aspect === "16:9" || lama.stretch === true;
    }
    delete lama.stretch;
    delete lama.aspect;
    Object.assign(view, lama);
  } catch {
    /* nilai bawaan saja */
  }
}
const saveView = () => localStorage.setItem("zerofuss.view", JSON.stringify(view));

function initRenderer(canvas) {
  const gl = canvas.getContext("webgl", {
    antialias: false,
    depth: false,
    stencil: false,
    preserveDrawingBuffer: false,
    // Lepas kanvas dari sinkronisasi compositor: satu sumber tundaan hilang.
    desynchronized: true,
    powerPreference: "high-performance",
  });
  if (!gl) throw new Error("WebGL tidak tersedia di webview ini");

  const sh = (type, src) => {
    const s = gl.createShader(type);
    gl.shaderSource(s, src);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s));
    return s;
  };
  const prog = gl.createProgram();
  gl.attachShader(prog, sh(gl.VERTEX_SHADER, VERT));
  gl.attachShader(prog, sh(gl.FRAGMENT_SHADER, FRAG));
  gl.linkProgram(prog);
  if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(prog));
  gl.useProgram(prog);

  const buf = gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER, buf);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);
  const loc = gl.getAttribLocation(prog, "p");
  gl.enableVertexAttribArray(loc);
  gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);

  const tex = (unit, name) => {
    const t = gl.createTexture();
    gl.activeTexture(gl.TEXTURE0 + unit);
    gl.bindTexture(gl.TEXTURE_2D, t);
    for (const p of [gl.TEXTURE_WRAP_S, gl.TEXTURE_WRAP_T]) gl.texParameteri(gl.TEXTURE_2D, p, gl.CLAMP_TO_EDGE);
    for (const p of [gl.TEXTURE_MIN_FILTER, gl.TEXTURE_MAG_FILTER]) gl.texParameteri(gl.TEXTURE_2D, p, gl.LINEAR);
    gl.uniform1i(gl.getUniformLocation(prog, name), unit);
    return t;
  };
  const texY = tex(0, "ty");
  const texUV = tex(1, "tuv");
  const uK = gl.getUniformLocation(prog, "k");
  const uAsp = gl.getUniformLocation(prog, "asp");
  // Baris NV12 tidak berpadding 4 byte.
  gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);

  let cw = 0, ch = 0;
  return function draw(w, h, data) {
    if (w !== cw || h !== ch) {
      canvas.width = w;
      canvas.height = h;
      gl.viewport(0, 0, w, h);
      gl.uniform1f(uAsp, w / h);
      cw = w;
      ch = h;
      applyAspect();
    }
    // Nilai 0,6 di ujung slider cukup untuk lensa FPV paling melengkung yang
    // ada; di atas itu gambarnya mulai terasa dipaksa.
    gl.uniform1f(uK, view.lens ? (view.lensStr / 100) * 0.6 : 0);
    const L = gl.LUMINANCE, LA = gl.LUMINANCE_ALPHA, U8 = gl.UNSIGNED_BYTE;
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, texY);
    gl.texImage2D(gl.TEXTURE_2D, 0, L, w, h, 0, L, U8, data.subarray(0, w * h));
    gl.activeTexture(gl.TEXTURE1);
    gl.bindTexture(gl.TEXTURE_2D, texUV);
    gl.texImage2D(gl.TEXTURE_2D, 0, LA, w >> 1, h >> 1, 0, LA, U8, data.subarray(w * h));
    gl.drawArrays(gl.TRIANGLES, 0, 3);
  };
}

// -------------------------------------------------------------- baca aliran

/// Bingkai di dalam aliran: [u16 lebar][u16 tinggi][u32 panjang][data NV12].
/// Potongan dari fetch tidak sejajar dengan batas frame, jadi header dan isi
/// diisi bertahap ke buffer tetap - nol realokasi setelah frame pertama.
async function readStream(port, signal, onFrame) {
  const res = await fetch(`http://127.0.0.1:${port}/frames`, { signal, cache: "no-store" });
  const rd = res.body.getReader();
  const hdr = new Uint8Array(8);
  const hv = new DataView(hdr.buffer);
  let hgot = 0, body = null, bgot = 0, need = 0, w = 0, h = 0;

  for (;;) {
    const { done, value } = await rd.read();
    if (done) return;
    let i = 0;
    while (i < value.length) {
      if (hgot < 8) {
        const k = Math.min(8 - hgot, value.length - i);
        hdr.set(value.subarray(i, i + k), hgot);
        hgot += k;
        i += k;
        if (hgot === 8) {
          w = hv.getUint16(0, true);
          h = hv.getUint16(2, true);
          need = hv.getUint32(4, true);
          if (!body || body.length !== need) body = new Uint8Array(need);
          bgot = 0;
        }
        continue;
      }
      const k = Math.min(need - bgot, value.length - i);
      body.set(value.subarray(i, i + k), bgot);
      bgot += k;
      i += k;
      if (bgot === need) {
        onFrame(w, h, body);
        hgot = 0;
      }
    }
  }
}

// --------------------------------------------------------------------- UI

const el = {
  stage: $("stage"), view: $("view"), idle: $("idle"), toast: $("toast"),
  pill: $("pill"), pillText: $("pill-text"),
  fps: $("s-fps"), mbps: $("s-mbps"), loss: $("s-loss"), res: $("s-res"),
  connect: $("btn-connect"), connect2: $("btn-connect2"),
  rec: $("btn-rec"), recLabel: $("rec-label"), full: $("btn-full"), pin: $("btn-pin"),
  set: $("btn-set"), close: $("btn-close"), drawer: $("drawer"),
  help: $("btn-help"), help2: $("btn-help2"), guide: $("guide"), guideClose: $("btn-guide-close"),
  setup: $("btn-setup"), setup2: $("btn-setup2"), cleanup: $("btn-cleanup"),
  openRec: $("btn-open-rec"),
  idleTitle: $("idle-title"), idleHint: $("idle-hint"), idleNote: $("idle-note"),
  idleFfmpeg: $("idle-ffmpeg"),
  adapter: $("adapter-state"), identBox: $("adapter-ident"),
  aName: $("a-name"), aDesc: $("a-desc"), aMac: $("a-mac"), aIp: $("a-ip"),
  pkts: $("d-pkts"), lost: $("d-lost"), drop: $("d-drop"), dec: $("d-dec"), log: $("d-log"),
  corrupt: $("d-corrupt"), restarts: $("d-restarts"),
  licenseDoc: $("btn-license-doc"), notices: $("btn-notices"),
  ob: $("ob"), obHitung: $("ob-hitung"), obTutup: $("ob-tutup"),
  obGambar: $("ob-gambar"), obJudul: $("ob-judul"), obIsi: $("ob-isi"),
  obTitik: $("ob-titik"), obMundur: $("ob-mundur"), obMaju: $("ob-maju"),
  obPanduan: $("ob-panduan"),
  f: { recdir: $("f-recdir") },
  v: {
    lens: $("f-lens"), lensStr: $("f-lens-str"), lensOut: $("o-lens-str"),
    wide: $("f-wide"), clean: $("f-clean"), pin: $("f-pin"),
  },
};

let draw = null;
let abort = null;
// Apa yang dilaporkan Rust pada tick terakhir. Tombolnya harus memakai sumber
// yang SAMA dengan labelnya: kalau aksinya dari `abort` (sisi JS) sementara
// labelnya dari `s.running` (sisi Rust), keduanya bisa berbeda pendapat selama
// satu tick - tombol bertuliskan "Putuskan" yang justru menyambung.
let berjalan = false;
let lastFrameAt = 0;
let toastTimer = 0;
let revealed = false;
// Keadaan adapter pada tick terakhir, apa adanya dari probe Rust. Menentukan
// dokumen mana yang benar untuk dibuka: keempat langkah Goggles 3, atau
// rujukan V1/V2. Disimpan sebagai string dan bukan sebagai bendera turunan -
// satu sumber, jadi tidak ada dua bendera yang bisa berbeda pendapat selama
// satu tick.
let adapterKini = "";
// Cuplikan korupsi sebelumnya - yang menentukan bukan nilainya, tapi arahnya.

function say(msg, bad = false) {
  el.toast.textContent = msg;
  el.toast.toggleAttribute("data-bad", bad);
  el.toast.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (el.toast.hidden = true), bad ? 9000 : 5000);
}

const bytes = (n) =>
  n >= 1e9 ? `${(n / 1e9).toFixed(2)} GB` : n >= 1e6 ? `${(n / 1e6).toFixed(1)} MB` : `${(n / 1e3).toFixed(0)} KB`;

/// Toleransi rasio: 1440/1080 dan 1280/960 sama-sama 1,3333, tapi pembulatan
/// bingkai bisa menggeser sedikit. 0,02 cukup lebar untuk itu dan masih jauh
/// dari 16:9 (1,778).
const SUMBER_4_3 = (w, h) => !!w && !!h && Math.abs(w / h - 4 / 3) < 0.02;

/// Menyiapkan kotak tampil kanvas. Backing store tetap ukuran asli, jadi nol
/// biaya dan nol kehilangan detail - yang diatur cuma CSS.
///
/// Bawaannya kotaknya dibuat SAMA dengan bingkai yang datang. Itu selalu benar
/// dan tidak pernah menambah bilah: kalau goggles sudah mem-pillarbox isinya
/// sendiri (Camera View Recording ON), bilah itu memang milik gambarnya.
///
/// Satu tindakan opsional: saat sumbernya 4:3, `wide` mengisi layar 16:9
/// dengan MEMOTONG atas-bawah - `object-fit: cover`, bukan `fill`. Terukur di
/// browser dengan persegi 300x300 di sumber 1440x1080 pada kotak 1280x720:
///   fill  -> 212x159  (bentuk diperas 33%)
///   cover -> 212x213  (bentuk utuh, atas-bawah terpotong)
/// Meregangkan tidak pernah diminta siapa pun; memotong iya.
function applyAspect() {
  const w = el.view.width, h = el.view.height;
  if (!w || !h) return;
  const isi = view.wide && SUMBER_4_3(w, h);
  el.view.style.aspectRatio = isi ? "16 / 9" : `${w} / ${h}`;
  el.view.style.objectFit = isi ? "cover" : "";
  // Saklarnya hanya berarti kalau sumbernya 4:3. Dimatikan, bukan disembunyikan:
  // pengguna perlu tahu fiturnya ada dan kenapa ia sedang tidak berlaku.
  const bisa = SUMBER_4_3(w, h);
  el.v.wide.disabled = !bisa;
  el.v.wide.closest(".switch").title = bisa
    ? ""
    : `Sumbernya ${w}×${h}, bukan 4:3 - tidak ada yang perlu diisi.`;
}

async function connect() {
  if (!draw) {
    try {
      draw = initRenderer(el.view);
    } catch (e) {
      say(String(e.message ?? e), true);
      return;
    }
  }
  el.connect.disabled = el.connect2.disabled = true;
  try {
    const port = await invoke("start");
    // Umur `abort` terikat pada umur ALIRAN, bukan pada niat pengguna. Aliran
    // bisa mati sendiri - kabel tercabut, ffmpeg mati - dan kalau `abort`
    // dibiarkan terisi sesudahnya, tombol di bar bertuliskan
    // "Sambungkan" tapi menjalankan `disconnect()`. Klik pertama tidak
    // melakukan apa pun; itu terbaca sebagai tombol rusak, tepat saat pengguna
    // sedang diminta menyambung lagi.
    const ini = (abort = new AbortController());
    lastFrameAt = 0;
    readStream(port, ini.signal, (w, h, d) => {
      draw(w, h, d);
      lastFrameAt = performance.now();
      el.stage.setAttribute("data-live", "");
    })
      .catch((e) => {
        if (e.name === "AbortError") return;
        say(`aliran video berhenti: ${e.message ?? e}`, true);
      })
      // Perbandingan identitas wajib: tanpa itu aliran lama yang baru selesai
      // bisa mengosongkan controller milik aliran baru.
      .finally(() => {
        if (abort === ini) abort = null;
      });
  } catch (e) {
    say(String(e), true);
  } finally {
    el.connect.disabled = el.connect2.disabled = false;
  }
}

async function disconnect() {
  abort?.abort();
  abort = null;
  await invoke("stop").catch(() => {});
  el.stage.removeAttribute("data-live");
  el.rec.disabled = true;
  el.rec.removeAttribute("data-on");
  el.recLabel.textContent = "Rekam";
}

el.connect.onclick = () => (abort || berjalan ? disconnect() : connect());
el.connect2.onclick = connect;

el.rec.onclick = async () => {
  try {
    const path = await invoke("toggle_record");
    say(path ? `merekam ke ${path}` : "rekaman disimpan");
  } catch (e) {
    say(String(e), true);
  }
};

/// Layar penuh memakai API JENDELA Tauri, bukan Fullscreen API HTML.
///
/// Ini bukan pilihan gaya. `document.documentElement.requestFullscreen()` MATI
/// di WKWebView macOS: WebKit menyembunyikan Fullscreen API di belakang
/// preferensi yang tidak dinyalakan untuk webview tertanam, jadi di Mac tombol
/// ini tidak melakukan apa pun - terukur di MacBook Air. Di Windows (WebView2)
/// API itu jalan, tapi punya dua jalur untuk satu tombol berarti satu di
/// antaranya selalu jadi jalur yang tidak pernah diuji.
///
/// Jendela Tauri sebaliknya sama di semua OS, dan hasilnya juga lebih benar:
/// yang jadi layar penuh adalah JENDELANYA (di macOS lewat fullscreen native,
/// lengkap dengan Esc dan tombol hijau), bukan sebuah elemen di dalam halaman.
const jendela = window.__TAURI__.window.getCurrentWindow();
let layarPenuh = false;

el.full.onclick = async () => {
  try {
    layarPenuh = !(await jendela.isFullscreen());
    await jendela.setFullscreen(layarPenuh);
    applyOverlay(true);
  } catch (e) {
    say(String(e.message ?? e), true);
  }
};

/// Bar disembunyikan saat layar penuh atau mode bersih. Keduanya memakai satu
/// mekanisme yang sama supaya tidak ada dua perilaku yang harus dihafal.
function applyOverlay(hint = false) {
  const on = layarPenuh || view.clean;
  document.body.toggleAttribute("data-overlay", on);
  document.body.toggleAttribute("data-pin", view.pin);
  el.full.toggleAttribute("data-on", layarPenuh);
  el.pin.toggleAttribute("data-on", view.pin);
  if (hint && on && !view.pin) {
    say(
      "Arahkan kursor ke tepi atas untuk memunculkan bar" +
        (layarPenuh ? " · Esc keluar layar penuh" : ""),
    );
  }
}

// Keluar layar penuh bisa terjadi tanpa lewat tombol kita - Esc, tombol hijau
// di macOS, atau Mission Control. Tanpa menyinkron ulang di sini, `layarPenuh`
// jadi basi dan bar tetap tersembunyi di jendela yang sudah normal.
jendela.onResized(async () => {
  const kini = await jendela.isFullscreen().catch(() => layarPenuh);
  if (kini !== layarPenuh) {
    layarPenuh = kini;
    applyOverlay(true);
  }
});

// Satu panel saja yang terbuka: dua panel berebut sisi kanan.
function panel(which) {
  const openGuide = which === "guide" && el.guide.hidden;
  const openSet = which === "set" && el.drawer.hidden;
  el.guide.hidden = !openGuide;
  el.drawer.hidden = !openSet;
  if (openSet) refreshIdent();
}
el.set.onclick = () => panel("set");
el.close.onclick = () => (el.drawer.hidden = true);
// Satu pintu untuk pertanyaan "bagaimana caranya": tombol "?" di bar, tuts H,
// dan "Cara pasang" di layar kosong semuanya membuka langkah yang sama.
// Panduan lengkap itu RUJUKAN, bukan instruksi kedua - pintu masuknya ada di
// langkah terakhir, tempat orang yang sudah mencoba semuanya berakhir.
el.help.onclick = el.help2.onclick = () => {
  // Kecuali kalau yang tercolok FPV Goggles V1/V2. Keempat langkah itu isinya
  // menu Goggles 3 (OTG, LiveView Sharing) yang TIDAK ADA di goggles lama -
  // mengarahkan mereka ke sana bukan sekadar tidak membantu, ia menyuruh
  // mencari menu yang tidak pernah ada. Untuk mereka rujukannya yang benar.
  if (adapterKini.startsWith("bulk-")) {
    bukaPanduan("guide-bulk");
    return;
  }
  bukaOb();
};
el.obPanduan.onclick = () => {
  tutupOb();
  bukaPanduan();
};

/// Buka panel rujukan, opsional langsung ke satu bagian.
function bukaPanduan(bagian) {
  panel("guide");
  if (!bagian) return;
  // Setelah panel terlihat, bukan sebelumnya: elemen tersembunyi tidak punya
  // posisi, jadi scroll ke sana tidak melakukan apa pun.
  requestAnimationFrame(() =>
    document.getElementById(bagian)?.scrollIntoView({ block: "start" })
  );
}
el.guideClose.onclick = () => (el.guide.hidden = true);
el.openRec.onclick = () => invoke("open_rec_dir").catch((e) => say(String(e), true));

const doSetup = async () => {
  try {
    say(await invoke("setup_adapter"));
  } catch (e) {
    say(String(e), true);
  }
  refreshIdent(true);
};
el.setup.onclick = el.setup2.onclick = doSetup;
el.cleanup.onclick = async () => {
  try {
    say(await invoke("cleanup_adapter"));
  } catch (e) {
    say(String(e), true);
  }
  refreshIdent(true);
};

/// Identitas adapter memanggil PowerShell, jadi hasilnya di-cache di Rust.
/// Tombol "Deteksi ulang" dibuang: membuka panel sudah menyegarkannya, dan
/// `paksa` dipakai tepat sesudah Setel/Bersihkan supaya keadaannya tidak basi.
async function refreshIdent(paksa = false) {
  const info = await invoke("adapter_info", { paksa }).catch(() => []);
  if (!info.length) {
    el.identBox.hidden = true;
    return;
  }
  const [name, desc, mac, ip] = info;
  el.aName.textContent = name;
  el.aDesc.textContent = desc;
  el.aMac.textContent = mac;
  // Bukan cuma menampilkan alamatnya - menyebut apakah alamatnya YANG BENAR.
  // Itu satu-satunya hal yang perlu diketahui pengguna dari baris ini.
  el.aIp.textContent = !ip
    ? "belum ada"
    : ip === "192.168.60.1" ? `${ip} (sesuai)` : `${ip} (tidak sesuai)`;
  el.identBox.hidden = false;
}

// -------------------------------------------------------- pengaturan tampilan

function applyView() {
  el.v.lens.checked = view.lens;
  el.v.lensStr.value = view.lensStr;
  el.v.lensOut.textContent = `${view.lensStr}%`;
  el.v.lensStr.parentElement.toggleAttribute("data-off", !view.lens);
  el.v.wide.checked = view.wide;
  el.v.clean.checked = view.clean;
  el.v.pin.checked = view.pin;
  applyAspect();
}

el.v.lens.onchange = () => {
  view.lens = el.v.lens.checked;
  saveView();
  applyView();
};
el.v.lensStr.oninput = () => {
  view.lensStr = +el.v.lensStr.value;
  el.v.lensOut.textContent = `${view.lensStr}%`;
  saveView();
};
const setWide = (on) => {
  view.wide = on;
  saveView();
  applyView();
};
el.v.wide.onchange = () => setWide(el.v.wide.checked);
const setClean = (on) => {
  view.clean = on;
  saveView();
  applyView();
  applyOverlay(on);
};
el.v.clean.onchange = () => setClean(el.v.clean.checked);

const setPin = (on) => {
  view.pin = on;
  saveView();
  applyView();
  applyOverlay();
};
el.pin.onclick = () => setPin(!view.pin);
el.v.pin.onchange = () => setPin(el.v.pin.checked);

document.addEventListener("keydown", (e) => {
  // TEXTAREA ikut dikecualikan, bukan cuma INPUT: mengetik di kolom isian
  // tidak boleh memicu pintasan ("s" menutup panel, "h" membuka modal langkah).
  if (e.target.tagName === "INPUT" || e.target.tagName === "TEXTAREA") return;
  // Modal langkah menahan seluruh pintasan aplikasi: menekan "f" saat membaca
  // langkah tidak boleh melompat ke layar penuh.
  if (!el.ob.hidden) {
    if (e.key === "Escape") tutupOb();
    else if (e.key === "ArrowRight" || e.key === "Enter") obLanjut();
    else if (e.key === "ArrowLeft") obKembali();
    return;
  }
  const k = e.key.toLowerCase();
  if (k === "f") el.full.click();
  else if (k === "r" && !el.rec.disabled) el.rec.click();
  else if (k === "s") panel("set");
  // Lewat tombolnya, bukan langsung ke bukaOb(): pemilihan dokumen yang benar
  // ada di satu tempat, dan tuts H tidak boleh punya pendapat sendiri.
  else if (k === "h") el.help.click();
  else if (k === "b") setClean(!view.clean);
  else if (k === "p") setPin(!view.pin);
  // Mode 4:3 vs 16:9 berganti antar penerbangan, jadi mengisi layar pantas
  // punya pintasan - bukan cuma terkubur di panel.
  else if (k === "a") {
    if (el.v.wide.disabled) {
      say(`Sumbernya ${el.view.width}×${el.view.height}, bukan 4:3 - tidak ada yang perlu diisi.`);
    } else {
      setWide(!view.wide);
      say(view.wide ? "Isi layar 16:9 - atas-bawah dipotong" : "Ikut sumber");
    }
  }
  else if (e.key === "Escape") {
    if (!el.drawer.hidden) el.drawer.hidden = true;
    else if (!el.guide.hidden) el.guide.hidden = true;
    else if (view.clean) setClean(false);
  }
});

// ------------------------------------------------------------- pengaturan pipa

/// Alamat jalur goggles tidak lagi ada di UI - lihat komentar `Settings` di
/// lib.rs. Yang tersisa untuk diubah pengguna cuma folder rekaman, jadi
/// muat/simpan hanya mengurus itu; nilai lainnya dibawa apa adanya supaya
/// jalan keluar darurat lewat settings.json tidak ikut terhapus.
let setelan = null;

async function loadSettings() {
  setelan = await invoke("get_settings");
  el.f.recdir.value = setelan.rec_dir;
  return setelan;
}

async function saveSettings() {
  if (!setelan) return;
  setelan = { ...setelan, rec_dir: el.f.recdir.value.trim() };
  await invoke("set_settings", { s: setelan }).catch((e) => say(String(e), true));
}
for (const i of Object.values(el.f)) i.onchange = saveSettings;

/// Ditandai sesudah onboarding dituntaskan, supaya modalnya tidak memaksa diri
/// lagi di jalan berikutnya.
async function tandaiOnboarded() {
  if (!setelan || setelan.onboarded) return;
  setelan = { ...setelan, onboarded: true };
  await invoke("set_settings", { s: setelan }).catch(() => {});
}

// ---------------------------------------------------------------- onboarding

/* Empat langkah, tiga di antaranya bergambar tangkapan asli dari goggles.
   Urutannya sama dengan yang harus dikerjakan pengguna, dan tidak ada langkah
   yang bisa dilewati tanpa video gagal muncul - itu sebabnya ini modal. */
const LANGKAH = [
  {
    svg: "i-usb",
    judul: "Colok kabel USB-C",
    isi: "Sambungkan goggles ke PC. Harus kabel <b>data</b> - kabel charge-only " +
         "tidak akan memunculkan perangkat apa pun.",
  },
  {
    img: "langkah-about.webp",
    judul: "Di goggles: Settings › About",
    isi: "Buka menu <b>Settings</b>, lalu masuk ke <b>About</b> di paling bawah.",
  },
  {
    img: "langkah-otg.webp",
    judul: "Nyalakan OTG Wired Connection",
    isi: "Aktifkan <b>OTG Wired Connection to Computer</b>. Inilah yang membuka " +
         "jalur video lewat kabel - tanpa ini goggles cuma mengisi daya.",
  },
  {
    img: "langkah-liveview.webp",
    judul: "Nyalakan LiveView Sharing",
    isi: "Tekan <b>5D button</b> ke bawah (ke arahmu) untuk membuka " +
         "<b>Shortcut Menu</b>, lalu nyalakan <b>LiveView Sharing</b>. Meski " +
         "namanya menyebut Wi-Fi di goggles, ini gerbang yang sama untuk jalur kabel.",
  },
];

let obIdx = 0;
let obFokusLama = null;

function obGambarLangkah(arah) {
  const s = LANGKAH[obIdx];
  el.obHitung.textContent = `${obIdx + 1} / ${LANGKAH.length}`;
  el.obJudul.textContent = s.judul;
  el.obIsi.innerHTML = s.isi;
  el.obGambar.innerHTML = s.img
    ? `<img src="${s.img}" alt="" />`
    : `<svg aria-hidden="true"><use href="#${s.svg}"/></svg>`;

  el.obTitik.innerHTML = LANGKAH.map((_, i) =>
    `<i${i === obIdx ? " data-kini" : ""}></i>`).join("");

  const akhir = obIdx === LANGKAH.length - 1;
  el.obMundur.disabled = obIdx === 0;
  el.obMaju.textContent = akhir ? "Selesai" : "Lanjut";
  el.obPanduan.hidden = !akhir;

  // Isi diganti lebih dulu, baru transisinya dilepas: dua rAF karena satu saja
  // kadang digabung browser dengan perubahan atribut, dan transisinya tak jalan.
  if (arah) {
    el.ob.dataset.masuk = arah;
    requestAnimationFrame(() =>
      requestAnimationFrame(() => delete el.ob.dataset.masuk));
  }
}

function bukaOb() {
  obFokusLama = document.activeElement;
  obIdx = 0;
  obGambarLangkah(null);
  // Gambar dipramuat saat modal dibuka, bukan saat aplikasi mulai: tiga berkas
  // kecil, tapi tidak ada gunanya memuatnya untuk orang yang tidak membukanya.
  for (const s of LANGKAH) if (s.img) new Image().src = s.img;
  el.ob.hidden = false;
  requestAnimationFrame(() => {
    el.ob.dataset.open = "";
    el.obMaju.focus();
  });
}

function tutupOb() {
  delete el.ob.dataset.open;
  // Tunggu transisi keluar selesai sebelum menyembunyikan, kalau tidak modalnya
  // hilang seketika dan animasinya tidak pernah terlihat.
  setTimeout(() => (el.ob.hidden = true), 200);
  obFokusLama?.focus?.();
  tandaiOnboarded();
}

const obLanjut = () => {
  if (obIdx === LANGKAH.length - 1) return tutupOb();
  obIdx++;
  obGambarLangkah("maju");
};
const obKembali = () => {
  if (obIdx === 0) return;
  obIdx--;
  obGambarLangkah("mundur");
};

el.obMaju.onclick = obLanjut;
el.obMundur.onclick = obKembali;
el.obTutup.onclick = tutupOb;
// Klik latar menutup - perilaku yang diharapkan dari modal, dan tetap aman
// karena langkahnya bisa dibuka lagi lewat "Cara pasang".
el.ob.onclick = (e) => {
  if (e.target === el.ob) tutupOb();
};

// ------------------------------------------------------------------- polling

const bukaDoc = (name) => () =>
  invoke("open_doc", { name }).catch((e) => say(String(e), true));
el.licenseDoc.onclick = bukaDoc("LICENSE.txt");
el.notices.onclick = bukaDoc("THIRD-PARTY-NOTICES.txt");

function setPill(state, text) {
  el.pill.dataset.state = state;
  el.pillText.textContent = text;
}

async function tick() {
  let s;
  try {
    s = await invoke("stats");
  } catch {
    return;
  }

  if (!revealed) {
    revealed = true;
    invoke("app_ready").catch(() => {});
  }

  adapterKini = s.adapter;
  const bulk = s.adapter.startsWith("bulk-");
  // Labelnya ikut berubah supaya tombolnya tidak menjanjikan hal yang salah:
  // di jalur V1/V2 yang dibuka rujukan, bukan keempat langkah Goggles 3.
  const dok = bulk ? "Panduan V1/V2" : "Cara pasang";
  el.help2.textContent = dok;
  el.help.title = `${dok} (H)`;

  const ADAPTER = {
    siap:           { teks: "terpasang", ok: true },
    "belum-ip":     { teks: "belum punya alamat IP", ok: false },
    "tidak-ada":    { teks: "tidak terdeteksi", ok: false },
    "bulk-siap":    { teks: "FPV Goggles V1/V2 (USB bulk)", ok: true },
    "bulk-driver":  { teks: "FPV Goggles V1/V2 - driver belum ada", ok: false },
    "rndis-usb-siap": { teks: "Goggles 3 / N3 (USB langsung)", ok: true },
  }[s.adapter] || { teks: "memeriksa…", ok: false };
  el.adapter.textContent = ADAPTER.teks;
  el.adapter.toggleAttribute("data-ok", ADAPTER.ok);
  // Setel adapter hanya berarti di jalur adapter jaringan, dan hanya kalau
  // adapternya ada. Di jalur USB bulk tidak ada adapter jaringan sama sekali;
  // di macOS jalur Goggles 3 lewat USB langsung, jadi tidak ada adapter yang
  // bisa disetel - `sistem.setel_adapter` menutup yang kedua, dan tombolnya
  // sendiri sudah disembunyikan saat boot.
  el.setup2.disabled =
    !sistem.setel_adapter || !(s.adapter === "belum-ip" || s.adapter === "siap");

  berjalan = s.running;
  if (!s.running) {
    el.connect.textContent = "Sambungkan";
    setPill("idle", "Terputus");
    el.stage.removeAttribute("data-live");
    el.fps.textContent = el.mbps.textContent = el.loss.textContent = el.res.textContent = "—";

    /* Enam keadaan, dan di masing-masing HANYA SATU tindakan yang berarti -
       tindakan itu yang jadi primer, sisanya mati atau disembunyikan.
       Yang salah sebelumnya: "Setel adapter" selalu bisa diklik walau tanpa
       adapter RNDIS ia hanya memunculkan prompt admin lalu gagal, dan
       "Sambungkan" di bar tetap hidup walau kembarannya di layar kosong mati.

       Dua keadaan `bulk-` milik FPV Goggles V1/V2. Jalurnya USB bulk dan
       tidak punya adapter jaringan apa pun, jadi di sana "Setel adapter"
       bukan cuma tak berguna - ia menyesatkan, dan disembunyikan.

       `rndis-usb-siap` milik Goggles 3 / N3 di macOS: interface RNDIS-nya
       dipegang Zerofuss sendiri dari user space, jadi di sana tidak ada
       adapter jaringan dan tidak ada alamat IP yang perlu disetel - yang
       tersisa cuma menekan Sambungkan. */
    const IDLE = {
      "tidak-ada": {
        judul: "Goggles belum terdeteksi",
        petunjuk: "Colok kabel USB-C, lalu nyalakan OTG Wired Connection di goggles.",
        catatan: "Belum ada goggles di PC ini.",
        setel: "mati",
        primer: "cara",
      },
      "belum-ip": {
        judul: "Adapter goggles perlu alamat IP",
        petunjuk: "Goggles tidak melayani DHCP, jadi alamatnya dipasang manual - sekali saja.",
        catatan: "Tekan Setel adapter (butuh izin admin).",
        setel: "hidup",
        primer: "setel",
      },
      siap: {
        judul: "Siap disambungkan",
        petunjuk: "Adapter sudah bersetel. Pastikan LiveView Sharing menyala di goggles.",
        catatan: "",
        setel: "sembunyi",
        primer: "sambung",
      },
      "rndis-usb-siap": {
        judul: "Goggles 3 / N3 siap disambungkan",
        petunjuk: "Satu langkah lagi: tekan Sambungkan. Pastikan LiveView Sharing menyala di goggles.",
        catatan: "Di Mac tidak ada adapter jaringan atau alamat IP yang perlu disetel.",
        setel: "sembunyi",
        primer: "sambung",
      },
      "bulk-driver": {
        // Di OS yang tidak punya pemasangan driver untuk jalur ini, satu-satunya
        // sebab interface-nya tidak bisa diklaim adalah program lain yang
        // sedang memegangnya. Menyuruh mereka mencari Zadig di situ bukan cuma
        // salah - ia mengirim orang berjam-jam ke alat yang tidak ada.
        judul: sistem.perlu_driver
          ? "FPV Goggles V1/V2 perlu driver"
          : "Interface video goggles sedang dipakai",
        petunjuk: sistem.perlu_driver
          ? "Interface videonya belum dipasangi WinUSB. Sekali saja, lewat Zadig."
          : "Tidak ada driver yang perlu dipasang di sini - yang menahan program lain. Tutup DigiView atau tab browser yang membuka goggles, lalu coba lagi.",
        catatan: "",
        setel: "sembunyi",
        primer: "cara",
      },
      "bulk-siap": {
        judul: "FPV Goggles V1/V2 siap",
        petunjuk: "Nyalakan drone / air unit supaya ada gambar yang dikirim.",
        catatan: "",
        setel: "sembunyi",
        primer: "sambung",
      },
    }[s.adapter] || {
      judul: "Memeriksa perangkat…",
      petunjuk: "",
      catatan: "",
      setel: "mati",
      primer: "cara",
    };

    el.idleTitle.textContent = IDLE.judul;
    el.idleHint.textContent = IDLE.petunjuk;
    el.idleNote.textContent = IDLE.catatan;

    // Setel adapter disembunyikan saat sudah siap: menjalankannya ulang tidak
    // merusak apa pun, tapi ia jadi kebisingan di layar yang seharusnya bersih.
    // Panel Pengaturan tetap menyediakannya untuk yang benar-benar butuh.
    // Di OS yang tidak punya langkah setel adapter ia tidak boleh muncul lagi,
    // apa pun keadaan adapternya: tick() jalan empat kali sedetik, dan satu
    // kali memunculkan tombol yang pasti gagal sudah cukup untuk menyesatkan.
    el.setup.hidden = !sistem.setel_adapter || IDLE.setel === "sembunyi";
    el.setup.disabled = IDLE.setel === "mati";
    el.setup.toggleAttribute("data-primary", IDLE.primer === "setel");

    const bisaSambung = IDLE.primer === "sambung";
    el.connect2.disabled = !bisaSambung;
    el.connect2.toggleAttribute("data-primary", bisaSambung);
    // Kembaran di bar harus ikut: tombol hidup yang pasti gagal itu jebakan.
    el.connect.disabled = !bisaSambung;

    // Saat tidak ada yang bisa dikerjakan aplikasi, membaca langkahnya ADALAH
    // tindakan utamanya.
    el.help2.toggleAttribute("data-primary", IDLE.primer === "cara");
    el.help2.classList.toggle("btn-ghost", IDLE.primer !== "cara");
    return;
  }

  el.connect.textContent = "Putuskan";
  // Pipa hidup: Putuskan harus selalu bisa ditekan, apa pun keadaan adapter.
  el.connect.disabled = false;
  // Kembaran di layar kosong harus mati selagi pipa hidup. Saat tersambung
  // tapi belum ada gambar, overlay idle sengaja tampil kembali - dan tanpa
  // baris ini ia membawa tombol "Sambungkan" yang masih primer dari tick idle
  // terakhir. Menekannya menjalankan aliran KEDUA dan menganakyatimkan yang
  // pertama, yang lalu tidak punya jalan untuk dihentikan.
  el.connect2.disabled = true;
  el.connect2.removeAttribute("data-primary");

  const fresh = lastFrameAt && performance.now() - lastFrameAt < 1500;
  if (fresh) {
    setPill("live", "Live");
  } else {
    // Aliran berhenti: kembalikan tampilan awal. Menahan frame terakhir di
    // layar membuatnya terlihat seperti gambar hidup yang macet - lebih buruk
    // daripada jujur mengatakan tidak ada gambar.
    setPill("wait", "Menunggu feed");
    el.stage.removeAttribute("data-live");
    el.idleTitle.textContent = "Tersambung, menunggu gambar";
    el.idleHint.innerHTML =
      "Nyalakan <b>LiveView Sharing</b> di goggles - 5D button ke bawah, lalu Shortcut Menu. Pastikan juga ada sumber gambar - air unit ter-link, atau <b>Camera View Recording</b> untuk uji tanpa drone.";
  }

  el.fps.textContent = fresh ? s.fps.toFixed(0) : "0";
  el.mbps.textContent = `${s.mbps.toFixed(1)} Mbps`;
  el.loss.textContent = `${s.loss.toFixed(1)}%`;
  el.loss.toggleAttribute("data-warn", s.loss >= 0.5 && s.loss < 3);
  // Rasio yang DIPAKSA harus terlihat. Tanpa ini gambar bisa tampil terperas
  // dan berbilah di kiri-kanan tanpa satu pun petunjuk kenapa - satu-satunya
  // penyebabnya adalah setelan yang tersimpan dari sesi lalu, dan pengguna
  // tidak punya cara menduganya.
  const diisi = view.wide && SUMBER_4_3(s.width, s.height);
  el.res.textContent = s.width ? `${s.width}×${s.height}` + (diisi ? " → 16:9" : "") : "—";
  el.res.toggleAttribute("data-warn", diisi);

  el.rec.disabled = !s.running;
  el.rec.toggleAttribute("data-on", s.recording);
  el.recLabel.textContent = s.recording ? bytes(s.rec_bytes) : "Rekam";

  el.pkts.textContent = s.pkts.toLocaleString("id");
  el.lost.textContent = s.lost.toLocaleString("id");
  el.drop.textContent = s.dropped.toLocaleString("id");
  el.dec.textContent = s.decoded.toLocaleString("id");
  el.corrupt.textContent = s.corrupt.toLocaleString("id");
  el.restarts.textContent = s.restarts.toLocaleString("id");
  // TIDAK diwarnai. Dulu ditandai kuning saat korupsi bertambah sementara
  // Hilang nol - dan itu terbalik: terukur di hardware, keadaan SEHAT justru
  // seperti itu. Tiga cuplikan berjarak ~90 detik pada sesi yang sama:
  // 295/7.553, 376/9.617, 496/12.711 frame - laju konstan 3,9% dengan Hilang 0
  // dan Dibuang 0. Jadi peringatan itu menyala sepanjang pemakaian normal dan
  // membuat pengguna mengejar kerusakan yang tidak ada. Yang benar-benar
  // menandakan masalah adalah HILANG, dan itu sudah punya warnanya sendiri.

  if (!el.drawer.hidden) {
    const log = await invoke("log_tail").catch(() => []);
    el.log.textContent = log.slice(-14).join("\n") || "—";
  }
}

loadView();
applyView();

/// Satu-satunya sumber pengetahuan OS di sisi tampilan. Sengaja dari Rust dan
/// bukan dari `navigator`: yang menentukan di sini bukan mesin render webview
/// tapi OS tempat probe USB dan pencarian ffmpeg benar-benar berjalan - dan
/// keduanya cuma diketahui Rust.
const sistem = await invoke("sistem").catch(() => {
  // Perintah ini cuma membaca konstanta, jadi ia tidak punya alasan gagal.
  // Tapi seluruh berkas ini satu modul dengan `await` di tingkat atas: kalau
  // baris ini melempar, TIDAK ADA sisa skrip yang terpasang - jendelanya
  // muncul kosong, tanpa satu pun pesan, dan pengaman splash 6 detik justru
  // memastikan kekosongan itu terlihat. Cadangan yang membuka semua jalur
  // lebih baik: petunjuknya bisa salah OS, tapi aplikasinya tetap terpakai.
  return { os: "windows", rndis: true, rndis_usb: false, setel_adapter: true, perlu_driver: true, ffmpeg: true };
});

// Blok panduan dan catatan yang hanya berlaku di OS tertentu. `data-os` berisi
// daftar OS tempat blok itu berlaku, dikueri SEKALI di sini - bukan cabang JS
// per blok, yang selalu berakhir dengan satu blok yang lupa diikutkan.
for (const n of document.querySelectorAll("[data-os]"))
  n.hidden = !n.dataset.os.split(" ").includes(sistem.os);

// Setel adapter dan Bersihkan DISEMBUNYIKAN, bukan dimatikan, di OS yang tidak
// punya langkah itu: di macOS jalur Goggles 3 / N3 berjalan lewat USB langsung
// dari user space, jadi tidak ada adapter jaringan yang bisa disetel dan
// keduanya tidak akan pernah berhasil. Tombol mati masih mengundang pertanyaan
// "kenapa dan bagaimana supaya hidup"; tombol yang tidak ada tidak. tick() ikut
// memeriksa `sistem.setel_adapter` supaya ia tidak memunculkannya lagi di tick
// berikutnya.
if (!sistem.setel_adapter) el.setup.hidden = el.setup2.hidden = el.cleanup.hidden = true;

// Tanpa decoder tidak akan ada gambar sama sekali, dan itu harus terbaca terus
// - bukan lewat sekali sebagai toast lalu hilang. Tidak memblokir apa pun:
// sisa aplikasi tetap bisa dipakai, cuma videonya yang tidak akan datang.
el.idleFfmpeg.hidden = sistem.ffmpeg;

applyOverlay();
const s0 = await loadSettings();
setInterval(tick, 250);
tick();

/* Jalan pertama: langkahnya dibuka sendiri, karena tanpa mengerjakannya tidak
   akan ada gambar sama sekali - dan pengguna tidak punya cara menduganya.
   Sesudah dituntaskan sekali, ia hanya muncul kalau diminta. Auto-connect
   dilewati di jalan pertama supaya galat sambungan tidak menimpa langkahnya. */
if (!s0.onboarded) {
  bukaOb();
} else if (s0.auto_start) {
  connect();
}
