// Motore di hevctui: config, indice, scan parallelo, coda encode NVENC.
// Nessuna dipendenza dalla UI: la GUI legge/scrive solo `Shared`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Versione dei parametri di encode: i "tenuti" con versione inferiore vengono ritentati.
pub const ENC_VERSION: u32 = 3;

// ---------------- Config ----------------
/// Cartella scansionata e tipo di contenuto (determina come si costruisce il titolo).
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ScanDir {
    pub path: String,
    pub kind: Kind,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub folders: Vec<ScanDir>,
    pub cq: u32,
    pub preset: String,
    pub ten_bit: bool,
    pub sample_check: bool,
    pub sample_count: usize,
    pub sample_secs: f64,
    pub sample_min_duration: f64,
    pub min_mbps: f64,
    pub min_saving_pct: i32,
    pub max_retries: u32,
    pub duration_tolerance: f64,
    pub workers: usize,
    pub scan_threads: usize,
    pub refresh_secs: u64,
    pub video_exts: Vec<String>,
    pub encode_codecs: Vec<String>,
    pub ffmpeg: String,
    pub ffprobe: String,
    pub auto_update_check: bool,
    // formato v3 originale (root + sottocartelle): letto solo per migrare in `folders`
    #[serde(skip_serializing)]
    pub root: String,
    #[serde(skip_serializing)]
    pub movie_dirs: Vec<String>,
    #[serde(skip_serializing)]
    pub series_dirs: Vec<String>,
}
impl Default for Config {
    fn default() -> Self {
        let strs = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
        Config {
            folders: vec![],
            cq: 25,
            preset: "p5".into(),
            ten_bit: true,
            sample_check: true,
            sample_count: 3,
            sample_secs: 20.0,
            sample_min_duration: 180.0,
            min_mbps: 4.0,
            min_saving_pct: 8,
            max_retries: 2,
            duration_tolerance: 2.0,
            workers: 1,
            scan_threads: 8,
            refresh_secs: 60,
            video_exts: strs(&["mkv", "mp4", "avi", "ts", "m4v", "mpg", "mov", "wmv"]),
            encode_codecs: strs(&["h264", "mpeg4", "vc1", "mpeg2video", "msmpeg4v3", "wmv3"]),
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            auto_update_check: true,
            root: String::new(),
            movie_dirs: vec![],
            series_dirs: vec![],
        }
    }
}
impl Config {
    /// Punti di campionamento distribuiti uniformemente tra il 25% e il 75% della durata.
    pub fn sample_points(&self) -> Vec<f64> {
        let n = self.sample_count.max(1);
        if n == 1 { return vec![0.5]; }
        (0..n).map(|i| 0.25 + 0.5 * i as f64 / (n - 1) as f64).collect()
    }
    fn migrate_legacy(&mut self) -> bool {
        if !self.folders.is_empty() || self.root.is_empty() { return false; }
        let root = Path::new(&self.root);
        let dirs = self.movie_dirs.iter().map(|d| (d, Kind::Film)).chain(self.series_dirs.iter().map(|d| (d, Kind::Serie)));
        self.folders = dirs.map(|(d, kind)| ScanDir { path: root.join(d).to_string_lossy().to_string(), kind }).collect();
        self.root.clear();
        self.movie_dirs.clear();
        self.series_dirs.clear();
        true
    }
}

/// Verifica che l'eseguibile risponda a `-version`.
pub fn tool_ok(exe: &str) -> bool {
    cmd(exe).arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let d = base.join("hevctui");
    let _ = fs::create_dir_all(&d);
    d
}
pub fn cfg_path() -> PathBuf { data_dir().join("config.toml") }
fn lib_path() -> PathBuf { data_dir().join("library.json") }
pub fn log_path() -> PathBuf { data_dir().join("hevctui.log") }
fn sample_dir() -> PathBuf {
    let d = data_dir().join("samples");
    let _ = fs::create_dir_all(&d);
    d
}

pub fn load_config() -> Config {
    if let Ok(s) = fs::read_to_string(cfg_path()) {
        if let Ok(mut c) = toml::from_str::<Config>(&s) {
            if c.migrate_legacy() { save_config(&c); }
            return c;
        }
    }
    let c = Config::default();
    save_config(&c);
    c
}
pub fn save_config(c: &Config) {
    if let Ok(s) = toml::to_string_pretty(c) {
        let _ = fs::write(cfg_path(), s);
    }
}

// ---------------- Model ----------------
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind { Film, Serie }
impl Kind {
    pub const ALL: [Kind; 2] = [Kind::Film, Kind::Serie];
    pub fn label(&self) -> &'static str { match self { Kind::Film => "Film", Kind::Serie => "Serie" } }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status { Pending, Converting, Done, Kept, SkipLow, SkipCodec, ErrProbe, ErrEncode }
impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Status::Pending => "in coda",
            Status::Converting => "convertendo",
            Status::Done => "fatto",
            Status::Kept => "tenuto",
            Status::SkipLow => "sotto soglia",
            Status::SkipCodec => "già hevc",
            Status::ErrProbe => "errore probe",
            Status::ErrEncode => "errore encode",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MediaFile {
    pub path: PathBuf,
    pub kind: Kind,
    pub title: String,
    pub codec: String,
    pub height: u32,
    pub size: u64,
    pub duration: f64,
    pub mbps: f64,
    pub status: Status,
    #[serde(default)]
    pub new_size: u64,
    #[serde(default)]
    pub saving: i32,
    #[serde(default)]
    pub sig: String,
    #[serde(default)]
    pub retries: u32,
    #[serde(default)]
    pub enc_ver: u32,
    #[serde(default)]
    pub note: String,
}

/// Encode in corso (uno per worker).
#[derive(Clone)]
pub struct Job {
    pub path: PathBuf,
    pub title: String,
    pub phase: String,
    pub pct: f32,
    pub fps: f32,
    pub speed: f32,
    pub started: Instant,
}

pub struct State {
    pub cfg: Config,
    pub files: Vec<MediaFile>,
    pub running: bool,
    pub quit: bool,
    pub jobs: Vec<Job>,
    pub skip: HashSet<PathBuf>,
    pub scanning: bool,
    pub scan_done: usize,
    pub scan_total: usize,
    pub first_scan_done: bool,
    pub rescan_now: bool,
    pub log: VecDeque<String>,
    pub update: crate::update::UpdateState,
}
pub type Shared = Arc<Mutex<State>>;

impl State {
    pub fn saved_total(&self) -> u64 {
        self.files.iter().filter(|f| f.status == Status::Done).map(|f| f.size.saturating_sub(f.new_size)).sum()
    }
    /// Riallinea Pending/SkipLow alla soglia corrente (chiamato quando cambia `min_mbps`).
    pub fn reclassify(&mut self) {
        let t = self.cfg.min_mbps;
        for f in &mut self.files {
            match f.status {
                Status::Pending if f.mbps < t => f.status = Status::SkipLow,
                Status::SkipLow if f.mbps >= t => f.status = Status::Pending,
                _ => {}
            }
        }
    }
    pub fn retry_errors(&mut self) -> usize {
        let mut n = 0;
        for f in &mut self.files {
            if f.status == Status::ErrEncode { f.status = Status::Pending; f.retries = 0; n += 1; }
        }
        n
    }
    pub fn requeue(&mut self, path: &Path) {
        if let Some(f) = self.files.iter_mut().find(|f| f.path == path) {
            if matches!(f.status, Status::Kept | Status::ErrEncode | Status::SkipLow) {
                f.status = Status::Pending;
                f.retries = 0;
            }
        }
    }
}

// ---------------- log ----------------
pub fn logline(st: Option<&Shared>, msg: &str) {
    let line = format!("{} {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), msg);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = writeln!(f, "{}", line);
    }
    if let Some(s) = st {
        let mut a = s.lock().unwrap();
        a.log.push_back(line);
        while a.log.len() > 300 { a.log.pop_front(); }
    }
}

// ---------------- helpers ----------------
fn is_tmp(p: &Path) -> bool {
    p.file_name().map(|n| n.to_string_lossy().contains(".hevctmp.")).unwrap_or(false)
}
fn sig_of(md: &fs::Metadata) -> String {
    let mt = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
    format!("{}-{}", md.len(), mt)
}
fn cmd(exe: &str) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(exe);
    #[cfg(windows)]
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

fn probe(cfg: &Config, path: &Path) -> Option<(String, u32, f64)> {
    let out = cmd(&cfg.ffprobe)
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_name,height", "-show_entries", "format=duration", "-of", "json"])
        .arg(path).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() { return None; }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let st = v.get("streams")?.as_array()?.first()?;
    let codec = st.get("codec_name")?.as_str()?.to_string();
    let height = st.get("height").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    let dur: f64 = v.get("format")?.get("duration")?.as_str()?.parse().ok()?;
    if dur <= 0.0 { return None; }
    Some((codec, height, dur))
}

// estrae "SxxEyy" dal nome file
fn extract_ep(name: &str) -> Option<String> {
    let b = name.as_bytes();
    let lb = name.to_lowercase().into_bytes();
    let mut i = 0;
    while i < lb.len() {
        if lb[i] == b's' {
            let mut j = i + 1;
            while j < lb.len() && lb[j].is_ascii_digit() { j += 1; }
            if j > i + 1 && j < lb.len() && lb[j] == b'e' {
                let mut k = j + 1;
                while k < lb.len() && lb[k].is_ascii_digit() { k += 1; }
                if k > j + 1 && k <= b.len() {
                    return Some(String::from_utf8_lossy(&b[i..k]).to_uppercase());
                }
            }
        }
        i += 1;
    }
    None
}

fn make_title(path: &Path, base: &Path, kind: Kind) -> String {
    match kind {
        Kind::Film => path.parent().and_then(|p| p.file_name()).map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()),
        Kind::Serie => {
            let series = path.strip_prefix(base).ok()
                .and_then(|rel| rel.components().next())
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .unwrap_or_else(|| "?".into());
            let fname = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let ep = extract_ep(&fname).unwrap_or_else(|| {
                path.parent().and_then(|p| p.file_name()).map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
            });
            format!("{} · {}", series, ep)
        }
    }
}

fn build_mediafile(cfg: &Config, path: &Path, base: &Path, kind: Kind, md: &fs::Metadata) -> MediaFile {
    let size = md.len();
    let mut mf = MediaFile {
        path: path.to_path_buf(), kind, title: make_title(path, base, kind), codec: String::new(), height: 0, size,
        duration: 0.0, mbps: 0.0, status: Status::Pending, new_size: 0, saving: 0, sig: sig_of(md),
        retries: 0, enc_ver: 0, note: String::new(),
    };
    match probe(cfg, path) {
        None => mf.status = Status::ErrProbe,
        Some((codec, height, dur)) => {
            mf.mbps = size as f64 * 8.0 / dur / 1_000_000.0;
            mf.status = if !cfg.encode_codecs.iter().any(|c| c.eq_ignore_ascii_case(&codec)) {
                Status::SkipCodec
            } else if mf.mbps < cfg.min_mbps {
                Status::SkipLow
            } else {
                Status::Pending
            };
            mf.codec = codec;
            mf.height = height;
            mf.duration = dur;
        }
    }
    mf
}

fn walk_all(cfg: &Config) -> Vec<(PathBuf, PathBuf, Kind)> {
    let mut out = vec![];
    for d in cfg.folders.iter().filter(|d| !d.path.trim().is_empty()) {
        let base = PathBuf::from(d.path.trim());
        for e in WalkDir::new(&base).into_iter().filter_map(|x| x.ok()) {
            if !e.file_type().is_file() { continue; }
            let ext = e.path().extension().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
            if cfg.video_exts.iter().any(|x| x.eq_ignore_ascii_case(&ext)) && !is_tmp(e.path()) {
                out.push((e.path().to_path_buf(), base.clone(), d.kind));
            }
        }
    }
    out
}

/// Probe parallelo dei file nuovi o cambiati rispetto a `known` (path → firma).
fn scan_changed(cfg: &Config, known: &HashMap<PathBuf, String>, progress: Option<&Shared>) -> (Vec<MediaFile>, HashSet<PathBuf>) {
    let current = walk_all(cfg);
    let paths: HashSet<PathBuf> = current.iter().map(|(p, _, _)| p.clone()).collect();
    let todo: Vec<(PathBuf, PathBuf, Kind, fs::Metadata)> = current.into_iter().filter_map(|(p, b, k)| {
        let md = fs::metadata(&p).ok()?;
        if known.get(&p).map(|s| *s == sig_of(&md)).unwrap_or(false) { None } else { Some((p, b, k, md)) }
    }).collect();
    if let Some(s) = progress {
        let mut a = s.lock().unwrap();
        a.scan_total = todo.len();
        a.scan_done = 0;
    }
    let done = AtomicUsize::new(0);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(cfg.scan_threads.clamp(1, 32)).build().unwrap();
    let out: Vec<MediaFile> = pool.install(|| {
        todo.par_iter().map(|(p, b, k, md)| {
            let mf = build_mediafile(cfg, p, b, *k, md);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if let Some(s) = progress { s.lock().unwrap().scan_done = n; }
            mf
        }).collect()
    });
    (out, paths)
}

// ---------------- persistence ----------------
pub fn load_library(cfg: &Config) -> Vec<MediaFile> {
    let mut v: Vec<MediaFile> = fs::read_to_string(lib_path()).ok()
        .and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    for f in &mut v {
        match f.status {
            Status::Converting => f.status = Status::Pending,
            Status::ErrEncode if f.retries < cfg.max_retries => f.status = Status::Pending,
            // tenuti con parametri vecchi: coi nuovi (10-bit) possono rendere
            Status::Kept if f.enc_ver < ENC_VERSION => f.status = Status::Pending,
            _ => {}
        }
    }
    v
}
pub fn save_library(st: &Shared) {
    let snapshot: Vec<MediaFile> = {
        let a = st.lock().unwrap();
        a.files.iter().map(|f| {
            let mut c = f.clone();
            if c.status == Status::Converting { c.status = Status::Pending; }
            c
        }).collect()
    };
    if let Ok(s) = serde_json::to_string(&snapshot) {
        let tmp = lib_path().with_extension("json.tmp");
        if fs::write(&tmp, s).is_ok() { let _ = fs::rename(&tmp, lib_path()); }
    }
}

// ---------------- refresh thread ----------------
pub fn refresh_loop(st: Shared) {
    loop {
        let (cfg, known) = {
            let mut a = st.lock().unwrap();
            if a.quit { return; }
            a.scanning = true;
            a.rescan_now = false;
            (a.cfg.clone(), a.files.iter().map(|f| (f.path.clone(), f.sig.clone())).collect::<HashMap<_, _>>())
        };
        let (updates, current) = scan_changed(&cfg, &known, Some(&st));
        {
            let mut a = st.lock().unwrap();
            for mut mf in updates {
                if let Some(pos) = a.files.iter().position(|f| f.path == mf.path) {
                    if a.files[pos].status == Status::Converting { continue; }
                    mf.retries = 0;
                    a.files[pos] = mf;
                } else {
                    a.files.push(mf);
                }
            }
            a.files.retain(|f| current.contains(&f.path) || f.status == Status::Converting);
            a.reclassify();
            a.first_scan_done = true;
            a.scanning = false;
        }
        save_library(&st);
        let secs = cfg.refresh_secs.max(10);
        for _ in 0..secs {
            { let a = st.lock().unwrap(); if a.quit { return; } if a.rescan_now { break; } }
            thread::sleep(Duration::from_secs(1));
        }
    }
}

// ---------------- encode ----------------
enum Outcome { Done { new_size: u64, saving: i32 }, Kept { saving: i32, estimated: bool }, Skipped, Error(String) }

fn video_args(cfg: &Config) -> Vec<String> {
    let mut v: Vec<String> = ["-c:v:0", "hevc_nvenc", "-preset", &cfg.preset, "-tune", "hq", "-rc", "vbr", "-cq"]
        .iter().map(|s| s.to_string()).collect();
    v.push(cfg.cq.to_string());
    v.extend(["-b:v:0", "0"].iter().map(|s| s.to_string()));
    if cfg.ten_bit {
        v.extend(["-profile:v:0", "main10", "-pix_fmt:v:0", "p010le"].iter().map(|s| s.to_string()));
    }
    v
}

pub fn encode_worker(st: Shared, slot: usize) {
    loop {
        let next = {
            let mut a = st.lock().unwrap();
            if a.quit { return; }
            if !a.running || slot >= a.cfg.workers.max(1) {
                None
            } else if let Some(i) = a.files.iter().position(|f| f.status == Status::Pending) {
                a.files[i].status = Status::Converting;
                let f = a.files[i].clone();
                a.jobs.push(Job { path: f.path.clone(), title: f.title.clone(), phase: "avvio".into(), pct: 0.0, fps: 0.0, speed: 0.0, started: Instant::now() });
                a.skip.remove(&f.path);
                Some((f, a.cfg.clone()))
            } else {
                None
            }
        };
        let (mf, cfg) = match next {
            Some(x) => x,
            None => { thread::sleep(Duration::from_millis(300)); continue; }
        };

        let outcome = run_encode(&st, &cfg, &mf);
        {
            let mut a = st.lock().unwrap();
            a.jobs.retain(|j| j.path != mf.path);
            if let Some(f) = a.files.iter_mut().find(|f| f.path == mf.path) {
                match outcome {
                    Outcome::Done { new_size, saving } => {
                        f.status = Status::Done;
                        f.new_size = new_size;
                        f.saving = saving;
                        f.enc_ver = ENC_VERSION;
                        f.note.clear();
                        if let Ok(m) = fs::metadata(&f.path) { f.sig = sig_of(&m); }
                    }
                    Outcome::Kept { saving, estimated } => {
                        f.status = Status::Kept;
                        f.saving = saving;
                        f.enc_ver = ENC_VERSION;
                        f.note = if estimated { format!("stima campioni: {}%", -saving) } else { format!("encode completo: {}%", -saving) };
                    }
                    Outcome::Skipped => f.status = Status::Pending,
                    Outcome::Error(m) => {
                        f.status = Status::ErrEncode;
                        f.retries += 1;
                        f.note = m;
                    }
                }
            }
            if matches!(outcome_kind(&a, &mf.path), Some(Status::Pending)) && a.skip.contains(&mf.path) {
                // saltato dall'utente: in fondo alla coda per non riprenderlo subito
                if let Some(pos) = a.files.iter().position(|f| f.path == mf.path) {
                    let f = a.files.remove(pos);
                    a.files.push(f);
                }
            }
        }
        save_library(&st);
    }
}
fn outcome_kind(a: &State, p: &Path) -> Option<Status> { a.files.iter().find(|f| f.path == p).map(|f| f.status) }

fn set_job(st: &Shared, path: &Path, f: impl FnOnce(&mut Job)) {
    let mut a = st.lock().unwrap();
    if let Some(j) = a.jobs.iter_mut().find(|j| j.path == path) { f(j); }
}
fn should_stop(st: &Shared, path: &Path) -> bool {
    let a = st.lock().unwrap();
    a.quit || a.skip.contains(path)
}

/// Lancia ffmpeg con `-progress pipe:1`, aggiorna il job, raccoglie le ultime righe di stderr.
/// Ritorna Ok(()) se exit 0, Err(ultime righe) altrimenti, Err("stop") se interrotto.
fn run_ffmpeg(st: &Shared, path: &Path, mut c: Command, total_secs: f64, phase: &str) -> Result<(), String> {
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child: Child = c.spawn().map_err(|e| format!("spawn ffmpeg: {e}"))?;
    let stderr = child.stderr.take().unwrap();
    let err_tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
    let et = err_tail.clone();
    let err_thread = thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut q = et.lock().unwrap();
            q.push_back(line);
            if q.len() > 20 { q.pop_front(); }
        }
    });
    set_job(st, path, |j| { j.phase = phase.to_string(); j.pct = 0.0; });
    let mut stopped = false;
    if let Some(so) = child.stdout.take() {
        let (mut fps, mut speed) = (0.0f32, 0.0f32);
        for line in BufReader::new(so).lines().map_while(Result::ok) {
            if let Some(v) = line.strip_prefix("fps=") { fps = v.trim().parse().unwrap_or(fps); }
            else if let Some(v) = line.strip_prefix("speed=") { speed = v.trim().trim_end_matches('x').parse().unwrap_or(speed); }
            else if let Some(v) = line.strip_prefix("out_time_us=") {
                if let Ok(us) = v.trim().parse::<i64>() {
                    let pct = if total_secs > 0.0 { ((us as f64 / 1e6) / total_secs * 100.0).clamp(0.0, 100.0) as f32 } else { 0.0 };
                    set_job(st, path, |j| { j.pct = pct; j.fps = fps; j.speed = speed; });
                }
            }
            if should_stop(st, path) { let _ = child.kill(); stopped = true; break; }
        }
    }
    let status = child.wait().ok();
    let _ = err_thread.join();
    if stopped || should_stop(st, path) { return Err("stop".into()); }
    if status.map(|s| s.success()).unwrap_or(false) { return Ok(()); }
    let tail: Vec<String> = err_tail.lock().unwrap().iter().cloned().collect();
    Err(if tail.is_empty() { "ffmpeg exit != 0 (nessun output)".into() } else { tail.join(" | ") })
}

/// Stima del risparmio: 3 spezzoni, sorgente copiata vs ricodificata, in locale.
/// Ritorna la % di risparmio stimata sul file intero.
fn sample_estimate(st: &Shared, cfg: &Config, mf: &MediaFile) -> Result<i32, String> {
    let dir = sample_dir();
    let tag = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        mf.path.hash(&mut h);
        format!("{:016x}", h.finish())
    };
    let points = cfg.sample_points();
    let secs = cfg.sample_secs.max(1.0);
    let (mut src_b, mut enc_b) = (0u64, 0u64);
    for (n, p) in points.iter().enumerate() {
        let a = dir.join(format!("{tag}-src.mkv"));
        let b = dir.join(format!("{tag}-enc.mkv"));
        let mut c = cmd(&cfg.ffmpeg);
        c.args(["-y", "-hide_banner", "-loglevel", "error", "-nostdin"])
            .args(["-ss", &format!("{:.1}", mf.duration * p), "-t", &secs.to_string()]).arg("-i").arg(&mf.path)
            .args(["-map", "0:v:0", "-c", "copy"]).arg(&a)
            .args(["-map", "0:v:0"]).args(video_args(cfg)).args(["-progress", "pipe:1"]).arg(&b);
        let r = run_ffmpeg(st, &mf.path, c, secs, &format!("campione {}/{}", n + 1, points.len()));
        let sizes = (fs::metadata(&a).map(|m| m.len()).unwrap_or(0), fs::metadata(&b).map(|m| m.len()).unwrap_or(0));
        let _ = fs::remove_file(&a);
        let _ = fs::remove_file(&b);
        r?;
        src_b += sizes.0;
        enc_b += sizes.1;
    }
    if src_b == 0 { return Err("campioni vuoti".into()); }
    // bitrate video sorgente stimato sull'intera durata; il resto (audio/sub) resta invariato
    let secs = secs * points.len() as f64;
    let vid_src = (src_b as f64 / secs * mf.duration).min(mf.size as f64);
    let ratio = enc_b as f64 / src_b as f64;
    let saved = vid_src * (1.0 - ratio);
    Ok((saved / mf.size as f64 * 100.0).round() as i32)
}

fn run_encode(st: &Shared, cfg: &Config, mf: &MediaFile) -> Outcome {
    let input = &mf.path;
    let dir = input.parent().unwrap_or_else(|| Path::new("."));
    let stem = input.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = dir.join(format!("{}.hevctmp.mkv", stem));
    let _ = fs::remove_file(&tmp);

    if cfg.sample_check && mf.duration > cfg.sample_min_duration {
        match sample_estimate(st, cfg, mf) {
            Ok(est) if est < cfg.min_saving_pct => {
                logline(Some(st), &format!("KEPT (stima {}%) {}", -est, input.display()));
                return Outcome::Kept { saving: est, estimated: true };
            }
            Ok(est) => logline(Some(st), &format!("STIMA -{}% {}", est, input.display())),
            Err(e) if e == "stop" => return Outcome::Skipped,
            Err(e) => logline(Some(st), &format!("STIMA fallita ({}), procedo: {}", e, input.display())),
        }
    }

    logline(Some(st), &format!("START {} (cq {} {} {})", input.display(), cfg.cq, cfg.preset, if cfg.ten_bit { "10bit" } else { "8bit" }));
    let mut c = cmd(&cfg.ffmpeg);
    c.args(["-y", "-hide_banner", "-loglevel", "error", "-nostdin"]).arg("-i").arg(input)
        .args(["-map", "0", "-c", "copy"]).args(video_args(cfg))
        .args(["-map_metadata", "0", "-progress", "pipe:1"]).arg(&tmp);
    match run_ffmpeg(st, input, c, mf.duration, "encode") {
        Ok(()) => {}
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            if e == "stop" { return Outcome::Skipped; }
            logline(Some(st), &format!("ERR-ENCODE {}: {}", input.display(), e));
            return Outcome::Error(e);
        }
    }
    if !tmp.exists() { return Outcome::Error("output mancante".into()); }

    set_job(st, input, |j| j.phase = "verifica".into());
    let orig_size = fs::metadata(input).map(|m| m.len()).unwrap_or(0);
    let new_size = fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    let vp = probe(cfg, &tmp);
    let dur_ok = vp.as_ref().map(|(_, _, d)| (d - mf.duration).abs() <= cfg.duration_tolerance).unwrap_or(false);
    let codec_ok = vp.as_ref().map(|(c, _, _)| c == "hevc").unwrap_or(false);
    if !dur_ok || !codec_ok {
        let _ = fs::remove_file(&tmp);
        let m = format!("verifica fallita (durata ok={dur_ok}, hevc={codec_ok})");
        logline(Some(st), &format!("ERR-ENCODE {}: {}", input.display(), m));
        return Outcome::Error(m);
    }
    let saving = if orig_size > 0 { (((orig_size as i64 - new_size as i64) * 100) / orig_size as i64) as i32 } else { 0 };
    if saving < cfg.min_saving_pct {
        let _ = fs::remove_file(&tmp);
        logline(Some(st), &format!("KEPT {} ({}%)", input.display(), -saving));
        return Outcome::Kept { saving, estimated: false };
    }
    let ext = input.extension().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    let final_path = if ext == "mkv" { input.to_path_buf() } else { dir.join(format!("{}.mkv", stem)) };
    let _ = fs::remove_file(&final_path);
    if let Err(e) = fs::rename(&tmp, &final_path) {
        let _ = fs::remove_file(&tmp);
        return Outcome::Error(format!("rename: {e}"));
    }
    if final_path != *input { let _ = fs::remove_file(input); }
    logline(Some(st), &format!("DONE {}  {}MB -> {}MB (-{}%)", final_path.display(), orig_size / 1_000_000, new_size / 1_000_000, saving));
    Outcome::Done { new_size, saving }
}

// ---------------- CLI scan ----------------
pub fn run_scan_cli(cfg: &Config) {
    let cache: HashMap<PathBuf, MediaFile> = load_library(cfg).into_iter().map(|f| (f.path.clone(), f)).collect();
    let known: HashMap<PathBuf, String> = cache.iter().map(|(p, f)| (p.clone(), f.sig.clone())).collect();
    let t0 = Instant::now();
    let (updates, current) = scan_changed(cfg, &known, None);
    let mut by_path: HashMap<PathBuf, MediaFile> = cache.into_iter().filter(|(p, _)| current.contains(p)).collect();
    let n_new = updates.len();
    for u in updates { by_path.insert(u.path.clone(), u); }
    let mut files: Vec<MediaFile> = by_path.into_values().collect();
    for f in &mut files {
        match f.status {
            Status::Pending if f.mbps < cfg.min_mbps => f.status = Status::SkipLow,
            Status::SkipLow if f.mbps >= cfg.min_mbps => f.status = Status::Pending,
            _ => {}
        }
    }
    if let Ok(s) = serde_json::to_string(&files) { let _ = fs::write(lib_path(), s); }

    let mut counts: HashMap<&str, (usize, u64)> = HashMap::new();
    for f in &files {
        let e = counts.entry(f.status.label()).or_insert((0, 0));
        e.0 += 1;
        e.1 += f.size;
    }
    println!("\n=== hevctui — indice ({} file, {} riprobati in {:.1}s) ===", files.len(), n_new, t0.elapsed().as_secs_f64());
    for (k, (n, b)) in &counts { println!("  {:<14} {:>5}  {:>8.1} GB", k, n, *b as f64 / 1e9); }
    let mut cand: Vec<&MediaFile> = files.iter().filter(|f| f.status == Status::Pending).collect();
    cand.sort_by(|a, b| b.size.cmp(&a.size));
    println!("\nCandidati (>= {:.0} Mbps): {} file, {:.1} GB", cfg.min_mbps, cand.len(), cand.iter().map(|f| f.size).sum::<u64>() as f64 / 1e9);
    for f in cand.iter().take(10) { println!("  {:>6.1} Mbps  {:>6.2} GB  [{}] {}", f.mbps, f.size as f64 / 1e9, f.kind.label(), f.title); }
}
