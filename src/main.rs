// hevctui v3 — app desktop (egui) per ricomprimere la libreria in HEVC con NVENC.
// `hevctui.exe scan` = aggiorna l'indice e stampa il riepilogo, senza GUI.
#![windows_subsystem = "windows"]

mod core;
mod update;

const MAX_WORKERS: usize = 4;

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};

use crate::core::*;
use crate::update::{UpdateState, CURRENT};

#[cfg(windows)]
fn attach_console() {
    extern "system" { fn AttachConsole(pid: u32) -> i32; }
    unsafe { AttachConsole(u32::MAX); } // ATTACH_PARENT_PROCESS
}

/// Mutex con nome tenuto per tutta la vita del processo: il setup lo usa per aspettare la chiusura.
#[cfg(windows)]
fn hold_running_mutex() {
    extern "system" { fn CreateMutexW(attrs: *const std::ffi::c_void, owner: i32, name: *const u16) -> *mut std::ffi::c_void; }
    let name: Vec<u16> = update::MUTEX_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()); }
}

fn main() -> eframe::Result {
    #[cfg(windows)]
    hold_running_mutex();
    let cfg = load_config();
    if std::env::args().any(|a| a == "scan") {
        #[cfg(windows)]
        attach_console();
        run_scan_cli(&cfg);
        return Ok(());
    }

    let mut st = State {
        cfg: cfg.clone(), files: load_library(&cfg), running: false, quit: false, jobs: vec![],
        skip: Default::default(), scanning: true, scan_done: 0, scan_total: 0, first_scan_done: false,
        rescan_now: false, update: UpdateState::Idle,
        log: std::fs::read_to_string(log_path()).unwrap_or_default().lines().rev().take(100).collect::<Vec<_>>()
            .into_iter().rev().map(String::from).collect(),
    };
    st.reclassify();
    let shared: Shared = Arc::new(Mutex::new(st));
    { let s = shared.clone(); thread::spawn(move || refresh_loop(s)); }
    for slot in 0..MAX_WORKERS { let s = shared.clone(); thread::spawn(move || encode_worker(s, slot)); }
    if cfg.auto_update_check { update::spawn_check(shared.clone()); }

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("hevctui").with_inner_size([1280.0, 800.0]).with_min_inner_size([900.0, 500.0]),
        ..Default::default()
    };
    let s2 = shared.clone();
    let r = eframe::run_native("hevctui", opts, Box::new(move |_cc| Ok(Box::new(Gui::new(s2)))));
    {
        let mut a = shared.lock().unwrap();
        a.quit = true;
        save_config(&a.cfg);
    }
    thread::sleep(Duration::from_millis(600)); // i worker chiudono ffmpeg e ripuliscono i temp
    save_library(&shared);
    r
}

// ---------------- GUI ----------------
#[derive(Clone, Copy, PartialEq)]
enum Filter { All, Candidates, Done, Skipped, Errors }
impl Filter {
    const ALL: [Filter; 5] = [Filter::All, Filter::Candidates, Filter::Done, Filter::Skipped, Filter::Errors];
    fn label(&self) -> &'static str {
        match self { Filter::All => "Tutti", Filter::Candidates => "In coda", Filter::Done => "Fatti / tenuti", Filter::Skipped => "Saltati", Filter::Errors => "Errori" }
    }
    fn matches(&self, s: Status) -> bool {
        match self {
            Filter::All => true,
            Filter::Candidates => matches!(s, Status::Pending | Status::Converting),
            Filter::Done => matches!(s, Status::Done | Status::Kept),
            Filter::Skipped => matches!(s, Status::SkipLow | Status::SkipCodec),
            Filter::Errors => matches!(s, Status::ErrProbe | Status::ErrEncode),
        }
    }
}
#[derive(Clone, Copy, PartialEq)]
enum SortCol { Status, Kind, Title, Codec, Mbps, Size, Result }

struct Gui {
    st: Shared,
    filter: Filter,
    search: String,
    sort: SortCol,
    asc: bool,
    selected: Option<std::path::PathBuf>,
    show_settings: bool,
    show_log: bool,
    last_cfg_save: std::time::Instant,
    // buffer dei campi testo: applicati alla config solo a fine modifica, per non riscansionare a ogni tasto
    folder_bufs: Vec<String>,
    exts_buf: String,
    codecs_buf: String,
    ffmpeg_buf: String,
    ffprobe_buf: String,
    ffmpeg_ok: bool,
    ffprobe_ok: bool,
    // azione richiesta dalla UI, eseguita a lock rilasciato (i thread di update bloccano `Shared`)
    update_action: Option<UpdateAction>,
    closing: bool,
}

enum UpdateAction { Check, Install(update::Release) }

impl Gui {
    fn new(st: Shared) -> Self {
        let cfg = st.lock().unwrap().cfg.clone();
        Gui { st, filter: Filter::Candidates, search: String::new(), sort: SortCol::Size, asc: false, selected: None,
              show_settings: cfg.folders.is_empty(), show_log: true, last_cfg_save: std::time::Instant::now(),
              folder_bufs: cfg.folders.iter().map(|d| d.path.clone()).collect(),
              exts_buf: cfg.video_exts.join(", "), codecs_buf: cfg.encode_codecs.join(", "),
              ffmpeg_ok: tool_ok(&cfg.ffmpeg), ffprobe_ok: tool_ok(&cfg.ffprobe),
              ffmpeg_buf: cfg.ffmpeg, ffprobe_buf: cfg.ffprobe, update_action: None, closing: false }
    }
}

fn status_color(s: Status) -> Color32 {
    match s {
        Status::Pending => Color32::from_rgb(170, 170, 170),
        Status::Converting => Color32::from_rgb(240, 190, 60),
        Status::Done => Color32::from_rgb(90, 200, 110),
        Status::Kept => Color32::from_rgb(90, 180, 220),
        Status::SkipLow | Status::SkipCodec => Color32::from_rgb(110, 110, 110),
        Status::ErrProbe | Status::ErrEncode => Color32::from_rgb(230, 90, 80),
    }
}
fn gb(b: u64) -> String { format!("{:.1} GB", b as f64 / 1e9) }
fn hms(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

fn stat_card(ui: &mut egui::Ui, label: &str, value: String, color: Color32) {
    egui::Frame::group(ui.style()).inner_margin(egui::Margin::symmetric(12, 6)).show(ui, |ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(label).small().weak());
            ui.label(RichText::new(value).size(20.0).strong().color(color));
        });
    });
}

impl eframe::App for Gui {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().request_repaint_after(Duration::from_millis(500));
        let st = self.st.clone();
        let mut a = st.lock().unwrap();
        let cfg_before = a.cfg.clone();

        // ---------- header ----------
        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading(RichText::new("hevctui").strong());
                let n = a.cfg.folders.len();
                ui.label(RichText::new(if n == 1 { "1 cartella".to_string() } else { format!("{n} cartelle") }).weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.selectable_label(self.show_settings, "⚙ Impostazioni").clicked() { self.show_settings = !self.show_settings; }
                    if ui.selectable_label(self.show_log, "Log").clicked() { self.show_log = !self.show_log; }
                    match &a.update {
                        UpdateState::Available(r) => {
                            let b = egui::Button::new(RichText::new(format!("⬆ Aggiorna a {}", r.version)).strong().color(Color32::WHITE)).fill(Color32::from_rgb(50, 110, 190));
                            let busy = !a.jobs.is_empty();
                            let resp = ui.add_enabled(!busy, b).on_disabled_hover_text("Metti in pausa la coda e attendi la fine dell'encode in corso");
                            if resp.clicked() { self.update_action = Some(UpdateAction::Install(r.clone())); }
                        }
                        UpdateState::Downloading { version, done, total } => {
                            ui.add(egui::ProgressBar::new(*done as f32 / (*total).max(1) as f32).desired_width(160.0).text(format!("download {version}")));
                        }
                        _ => {}
                    }
                    let (txt, col) = if a.running { ("⏸  Pausa", Color32::from_rgb(200, 150, 40)) } else { ("▶  Avvia coda", Color32::from_rgb(40, 140, 70)) };
                    if ui.add(egui::Button::new(RichText::new(txt).strong().color(Color32::WHITE)).fill(col).min_size([130.0, 28.0].into())).clicked() {
                        a.running = !a.running;
                    }
                });
            });
            ui.add_space(4.0);
            let count = |f: &dyn Fn(Status) -> bool| a.files.iter().filter(|x| f(x.status)).count();
            let queued: Vec<&MediaFile> = a.files.iter().filter(|x| matches!(x.status, Status::Pending | Status::Converting)).collect();
            let queued_gb: u64 = queued.iter().map(|x| x.size).sum();
            ui.horizontal(|ui| {
                stat_card(ui, "Risparmiato", gb(a.saved_total()), Color32::from_rgb(90, 200, 110));
                stat_card(ui, "Fatti", count(&|s| s == Status::Done).to_string(), Color32::from_rgb(90, 200, 110));
                stat_card(ui, "Tenuti", count(&|s| s == Status::Kept).to_string(), Color32::from_rgb(90, 180, 220));
                stat_card(ui, "In coda", format!("{}  ({})", queued.len(), gb(queued_gb)), Color32::from_rgb(220, 220, 220));
                stat_card(ui, "Errori", count(&|s| matches!(s, Status::ErrEncode | Status::ErrProbe)).to_string(), Color32::from_rgb(230, 90, 80));
                stat_card(ui, "Libreria", format!("{} file", a.files.len()), Color32::from_rgb(180, 180, 180));
                ui.vertical(|ui| {
                    ui.label(RichText::new(format!("NVENC {} · CQ {} · {} · soglia {:.0} Mbps", a.cfg.preset, a.cfg.cq, if a.cfg.ten_bit { "10-bit" } else { "8-bit" }, a.cfg.min_mbps)).small());
                    if a.scanning {
                        let p = if a.scan_total > 0 { a.scan_done as f32 / a.scan_total as f32 } else { 0.0 };
                        ui.add(egui::ProgressBar::new(p).desired_width(260.0).text(format!("scansione {}/{}", a.scan_done, a.scan_total)));
                    } else {
                        ui.label(RichText::new("indice aggiornato").small().weak());
                    }
                });
            });
            ui.add_space(6.0);
        });

        // ---------- job attivi ----------
        if !a.jobs.is_empty() {
            egui::Panel::top("jobs").show(ui, |ui| {
                ui.add_space(4.0);
                let jobs = a.jobs.clone();
                for j in &jobs {
                    let dur = a.files.iter().find(|f| f.path == j.path).map(|f| f.duration).unwrap_or(0.0);
                    let eta = if j.phase == "encode" && j.speed > 0.0 { format!(" · ETA {}", hms(dur * (1.0 - j.pct as f64 / 100.0) / j.speed as f64)) } else { String::new() };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&j.title).strong());
                        ui.label(RichText::new(format!("{} · {:.0} fps · {:.2}x · {}{}", j.phase, j.fps, j.speed, hms(j.started.elapsed().as_secs_f64()), eta)).weak());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Salta").on_hover_text("Interrompe e rimette il file in fondo alla coda").clicked() {
                                a.skip.insert(j.path.clone());
                            }
                        });
                    });
                    ui.add(egui::ProgressBar::new(j.pct / 100.0).show_percentage().fill(Color32::from_rgb(200, 150, 40)));
                }
                ui.add_space(4.0);
            });
        }

        // ---------- impostazioni ----------
        if self.show_settings {
            egui::Panel::right("settings").default_size(340.0).show(ui, |ui| {
                ui.heading("Impostazioni");
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    self.settings_ui(ui, &mut a);
                });
            });
        }

        // ---------- log ----------
        if self.show_log {
            egui::Panel::bottom("log").resizable(true).default_size(150.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Log").strong());
                    ui.label(RichText::new(log_path().display().to_string()).small().weak());
                });
                egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                    for l in &a.log {
                        let c = if l.contains("ERR") { Color32::from_rgb(230, 90, 80) } else if l.contains("DONE") { Color32::from_rgb(90, 200, 110) } else { ui.visuals().text_color() };
                        ui.label(RichText::new(l).monospace().size(11.5).color(c));
                    }
                });
            });
        }

        // ---------- dettaglio ----------
        if let Some(sel) = self.selected.clone() {
            if let Some(f) = a.files.iter().find(|f| f.path == sel).cloned() {
                egui::Panel::bottom("detail").show(ui, |ui| {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&f.title).strong().size(15.0));
                        ui.label(RichText::new(f.status.label()).color(status_color(f.status)));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("✕").clicked() { self.selected = None; }
                            if matches!(f.status, Status::Kept | Status::ErrEncode | Status::SkipLow) && ui.button("Metti in coda").clicked() {
                                a.requeue(&f.path);
                            }
                            if ui.button("Apri cartella").clicked() {
                                let _ = std::process::Command::new("explorer").arg("/select,").arg(&f.path).spawn();
                            }
                        });
                    });
                    ui.label(RichText::new(f.path.display().to_string()).small().weak());
                    ui.label(format!("{} {}p · {:.2} Mbps · {} · {}", f.codec, f.height, f.mbps, gb(f.size), hms(f.duration)));
                    if f.new_size > 0 { ui.label(format!("Nuova size: {}  (-{}%)", gb(f.new_size), f.saving)); }
                    if !f.note.is_empty() { ui.label(RichText::new(&f.note).small().color(Color32::from_rgb(230, 170, 90))); }
                    ui.add_space(4.0);
                });
            }
        }

        // ---------- tabella ----------
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                for f in Filter::ALL {
                    let n = a.files.iter().filter(|x| f.matches(x.status)).count();
                    if ui.selectable_label(self.filter == f, format!("{} ({})", f.label(), n)).clicked() { self.filter = f; }
                }
                ui.separator();
                ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("cerca titolo…").desired_width(220.0));
            });
            ui.add_space(4.0);
            if a.cfg.folders.is_empty() {
                ui.label(RichText::new("Nessuna cartella configurata: aggiungila da ⚙ Impostazioni → Cartelle.").color(Color32::from_rgb(230, 170, 90)));
            }

            let q = self.search.to_lowercase();
            let mut idx: Vec<usize> = a.files.iter().enumerate()
                .filter(|(_, f)| self.filter.matches(f.status) && (q.is_empty() || f.title.to_lowercase().contains(&q)))
                .map(|(i, _)| i).collect();
            let files = &a.files;
            idx.sort_by(|&x, &y| {
                let (fx, fy) = (&files[x], &files[y]);
                let o = match self.sort {
                    SortCol::Status => fx.status.label().cmp(fy.status.label()),
                    SortCol::Kind => fx.kind.label().cmp(fy.kind.label()),
                    SortCol::Title => fx.title.to_lowercase().cmp(&fy.title.to_lowercase()),
                    SortCol::Codec => fx.codec.cmp(&fy.codec).then(fx.height.cmp(&fy.height)),
                    SortCol::Mbps => fx.mbps.partial_cmp(&fy.mbps).unwrap_or(std::cmp::Ordering::Equal),
                    SortCol::Size => fx.size.cmp(&fy.size),
                    SortCol::Result => fx.saving.cmp(&fy.saving),
                };
                if self.asc { o } else { o.reverse() }
            });

            let mut clicked_sort: Option<SortCol> = None;
            let mut clicked_row: Option<std::path::PathBuf> = None;
            let header = |ui: &mut egui::Ui, label: &str, col: SortCol, cur: SortCol, asc: bool, out: &mut Option<SortCol>| {
                let arrow = if col == cur { if asc { " ⬆" } else { " ⬇" } } else { "" };
                if ui.add(egui::Button::new(RichText::new(format!("{label}{arrow}")).strong()).frame(false)).clicked() { *out = Some(col); }
            };
            let (cur, asc) = (self.sort, self.asc);
            let sel = self.selected.clone();
            TableBuilder::new(ui)
                .striped(true)
                .sense(egui::Sense::click())
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(110.0))
                .column(Column::exact(50.0))
                .column(Column::remainder().at_least(200.0).clip(true))
                .column(Column::exact(100.0))
                .column(Column::exact(70.0))
                .column(Column::exact(80.0))
                .column(Column::exact(80.0))
                .header(24.0, |mut h| {
                    h.col(|ui| header(ui, "Stato", SortCol::Status, cur, asc, &mut clicked_sort));
                    h.col(|ui| header(ui, "Tipo", SortCol::Kind, cur, asc, &mut clicked_sort));
                    h.col(|ui| header(ui, "Titolo", SortCol::Title, cur, asc, &mut clicked_sort));
                    h.col(|ui| header(ui, "Codec", SortCol::Codec, cur, asc, &mut clicked_sort));
                    h.col(|ui| header(ui, "Bitrate", SortCol::Mbps, cur, asc, &mut clicked_sort));
                    h.col(|ui| header(ui, "Size", SortCol::Size, cur, asc, &mut clicked_sort));
                    h.col(|ui| header(ui, "Esito", SortCol::Result, cur, asc, &mut clicked_sort));
                })
                .body(|body| {
                    body.rows(22.0, idx.len(), |mut row| {
                        let f = &files[idx[row.index()]];
                        row.set_selected(sel.as_ref() == Some(&f.path));
                        row.col(|ui| { ui.label(RichText::new(f.status.label()).color(status_color(f.status))); });
                        row.col(|ui| { ui.label(f.kind.label()); });
                        row.col(|ui| { ui.label(&f.title).on_hover_text(f.path.display().to_string()); });
                        row.col(|ui| { ui.label(if f.codec.is_empty() { "-".into() } else { format!("{} {}p", f.codec, f.height) }); });
                        row.col(|ui| { ui.label(format!("{:.1} M", f.mbps)); });
                        row.col(|ui| { ui.label(gb(f.size)); });
                        row.col(|ui| {
                            let t = match f.status {
                                Status::Done => format!("-{}%", f.saving),
                                Status::Kept => format!("{:+}%", -f.saving),
                                _ => String::new(),
                            };
                            ui.label(RichText::new(t).color(status_color(f.status)));
                        });
                        if row.response().clicked() { clicked_row = Some(f.path.clone()); }
                    });
                });
            if let Some(c) = clicked_sort {
                if c == self.sort { self.asc = !self.asc; } else { self.sort = c; self.asc = matches!(c, SortCol::Title | SortCol::Kind | SortCol::Status); }
            }
            if let Some(p) = clicked_row { self.selected = if self.selected.as_ref() == Some(&p) { None } else { Some(p) }; }
        });

        // ---------- effetti delle impostazioni ----------
        if a.cfg.min_mbps != cfg_before.min_mbps { a.reclassify(); }
        if a.cfg != cfg_before && self.last_cfg_save.elapsed() > Duration::from_secs(1) {
            save_config(&a.cfg);
            self.last_cfg_save = std::time::Instant::now();
        }

        // ---------- aggiornamenti ----------
        if matches!(a.update, UpdateState::Installing) && !self.closing {
            self.closing = true; // il setup attende la chiusura (mutex) e poi rilancia l'app
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        drop(a);
        match self.update_action.take() {
            Some(UpdateAction::Check) => update::spawn_check(self.st.clone()),
            Some(UpdateAction::Install(r)) => update::spawn_install(self.st.clone(), r),
            None => {}
        }
    }
}

impl Gui {
    fn settings_ui(&mut self, ui: &mut egui::Ui, a: &mut State) {
        let mut rescan = false;

        egui::CollapsingHeader::new("Cartelle").default_open(true).show(ui, |ui| {
            if self.folder_bufs.len() != a.cfg.folders.len() {
                self.folder_bufs = a.cfg.folders.iter().map(|d| d.path.clone()).collect();
            }
            if a.cfg.folders.is_empty() {
                ui.label(RichText::new("Nessuna cartella: aggiungine una da scansionare.").color(Color32::from_rgb(230, 170, 90)));
            }
            let mut remove = None;
            for i in 0..a.cfg.folders.len() {
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt(("kind", i)).width(60.0).selected_text(a.cfg.folders[i].kind.label()).show_ui(ui, |ui| {
                        for k in Kind::ALL {
                            if ui.selectable_value(&mut a.cfg.folders[i].kind, k, k.label()).changed() { rescan = true; }
                        }
                    });
                    if ui.button("✕").on_hover_text("Rimuovi (i file della cartella escono dall'indice)").clicked() { remove = Some(i); }
                    if ui.button("📁").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            self.folder_bufs[i] = p.display().to_string();
                            a.cfg.folders[i].path = self.folder_bufs[i].clone();
                            rescan = true;
                        }
                    }
                    let r = ui.add(egui::TextEdit::singleline(&mut self.folder_bufs[i]).hint_text(r"percorso o \\server\share").desired_width(f32::INFINITY));
                    if r.lost_focus() && self.folder_bufs[i].trim() != a.cfg.folders[i].path {
                        a.cfg.folders[i].path = self.folder_bufs[i].trim().to_string();
                        rescan = true;
                    }
                });
            }
            if let Some(i) = remove {
                a.cfg.folders.remove(i);
                self.folder_bufs.remove(i);
                rescan = true;
            }
            ui.horizontal(|ui| {
                for k in Kind::ALL {
                    if ui.button(format!("+ {}", k.label())).clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            a.cfg.folders.push(ScanDir { path: p.display().to_string(), kind: k });
                            self.folder_bufs.push(p.display().to_string());
                            rescan = true;
                        }
                    }
                }
            });
            ui.label(RichText::new("Film: titolo = cartella del file. Serie: titolo = prima sottocartella + SxxEyy.").small().weak());
        });

        egui::CollapsingHeader::new("Encode").default_open(true).show(ui, |ui| {
            egui::Grid::new("cfg_enc").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("CQ").on_hover_text("Qualità: più basso = migliore e più grande. 25 in 10-bit ≈ 22 in 8-bit");
                ui.add(egui::Slider::new(&mut a.cfg.cq, 16..=34));
                ui.end_row();
                ui.label("Preset NVENC").on_hover_text("p5 = stessa qualità di p7 misurata (VMAF), ~70% più veloce");
                egui::ComboBox::from_id_salt("preset").selected_text(a.cfg.preset.clone()).show_ui(ui, |ui| {
                    for p in ["p1", "p2", "p3", "p4", "p5", "p6", "p7"] { ui.selectable_value(&mut a.cfg.preset, p.to_string(), p); }
                });
                ui.end_row();
                ui.label("10-bit");
                ui.checkbox(&mut a.cfg.ten_bit, "Main10 (file più piccoli)");
                ui.end_row();
                ui.label("Encode paralleli").on_hover_text("Una GPU consumer ha in genere un solo chip NVENC: più encode paralleli non aumentano il throughput");
                ui.add(egui::Slider::new(&mut a.cfg.workers, 1..=MAX_WORKERS));
                ui.end_row();
            });
        });

        egui::CollapsingHeader::new("Soglie").default_open(true).show(ui, |ui| {
            egui::Grid::new("cfg_thr").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Soglia bitrate").on_hover_text("Sotto questo bitrate il file non viene ricodificato");
                ui.add(egui::Slider::new(&mut a.cfg.min_mbps, 0.5..=30.0).step_by(0.5).suffix(" Mbps"));
                ui.end_row();
                ui.label("Risparmio minimo").on_hover_text("Se il risparmio (stimato o reale) è inferiore, l'originale viene tenuto");
                ui.add(egui::Slider::new(&mut a.cfg.min_saving_pct, 0..=60).suffix(" %"));
                ui.end_row();
                ui.label("Tentativi su errore").on_hover_text("All'avvio gli errori di encode vengono ritentati fino a questo numero");
                ui.add(egui::Slider::new(&mut a.cfg.max_retries, 0..=10));
                ui.end_row();
                ui.label("Tolleranza durata").on_hover_text("Differenza massima di durata ammessa tra originale e ricodificato");
                ui.add(egui::Slider::new(&mut a.cfg.duration_tolerance, 0.5..=30.0).step_by(0.5).suffix(" s"));
                ui.end_row();
            });
        });

        egui::CollapsingHeader::new("Stima a campioni").default_open(false).show(ui, |ui| {
            egui::Grid::new("cfg_smp").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Attiva").on_hover_text("Spezzoni ricodificati prima dell'encode: scarta subito i file che non renderebbero");
                ui.checkbox(&mut a.cfg.sample_check, "");
                ui.end_row();
                ui.label("Campioni");
                ui.add(egui::Slider::new(&mut a.cfg.sample_count, 1..=6));
                ui.end_row();
                ui.label("Durata campione");
                ui.add(egui::Slider::new(&mut a.cfg.sample_secs, 5.0..=60.0).step_by(1.0).suffix(" s"));
                ui.end_row();
                ui.label("Solo file più lunghi di").on_hover_text("I file più corti vengono ricodificati direttamente");
                ui.add(egui::Slider::new(&mut a.cfg.sample_min_duration, 0.0..=1800.0).step_by(30.0).suffix(" s"));
                ui.end_row();
            });
        });

        egui::CollapsingHeader::new("Scansione").default_open(false).show(ui, |ui| {
            egui::Grid::new("cfg_scan").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Thread probe");
                ui.add(egui::Slider::new(&mut a.cfg.scan_threads, 1..=32));
                ui.end_row();
                ui.label("Intervallo refresh");
                ui.add(egui::Slider::new(&mut a.cfg.refresh_secs, 10..=3600).logarithmic(true).suffix(" s"));
                ui.end_row();
                ui.label("Estensioni video");
                if list_edit(ui, &mut self.exts_buf, &mut a.cfg.video_exts) { rescan = true; }
                ui.end_row();
                ui.label("Codec da ricodificare").on_hover_text("Nomi codec ffprobe; gli altri (hevc, av1, …) vengono saltati");
                if list_edit(ui, &mut self.codecs_buf, &mut a.cfg.encode_codecs) { rescan = true; }
                ui.end_row();
            });
        });

        egui::CollapsingHeader::new("Strumenti").default_open(!self.ffmpeg_ok || !self.ffprobe_ok).show(ui, |ui| {
            ui.label(RichText::new("Nome nel PATH o percorso completo.").small().weak());
            tool_edit(ui, "ffmpeg", &mut self.ffmpeg_buf, &mut a.cfg.ffmpeg, &mut self.ffmpeg_ok);
            if tool_edit(ui, "ffprobe", &mut self.ffprobe_buf, &mut a.cfg.ffprobe, &mut self.ffprobe_ok) { rescan = true; }
        });

        ui.separator();
        if ui.button("Riprova tutti gli errori").clicked() {
            let n = a.retry_errors();
            a.log.push_back(format!("rimessi in coda {n} errori"));
        }
        if ui.button("Riscansiona ora").clicked() { rescan = true; }
        ui.add_space(8.0);
        ui.label(RichText::new(format!("Config: {}", cfg_path().display())).small().weak());

        ui.separator();
        ui.label(RichText::new(format!("hevctui {CURRENT}")).strong());
        ui.checkbox(&mut a.cfg.auto_update_check, "Controlla aggiornamenti all'avvio");
        ui.horizontal(|ui| {
            let checking = matches!(a.update, UpdateState::Checking | UpdateState::Downloading { .. } | UpdateState::Installing);
            if ui.add_enabled(!checking, egui::Button::new("Controlla ora")).clicked() { self.update_action = Some(UpdateAction::Check); }
            let (txt, col) = match &a.update {
                UpdateState::Idle => (String::new(), ui.visuals().weak_text_color()),
                UpdateState::Checking => ("controllo…".into(), ui.visuals().weak_text_color()),
                UpdateState::UpToDate => ("aggiornato".into(), Color32::from_rgb(90, 200, 110)),
                UpdateState::Available(r) => (format!("disponibile {}", r.version), Color32::from_rgb(90, 160, 230)),
                UpdateState::Downloading { done, total, .. } => (format!("download {}%", done * 100 / (*total).max(1)), Color32::from_rgb(90, 160, 230)),
                UpdateState::Installing => ("avvio installazione…".into(), Color32::from_rgb(90, 160, 230)),
                UpdateState::Error(e) => (e.clone(), Color32::from_rgb(230, 90, 80)),
            };
            ui.label(RichText::new(txt).small().color(col));
        });
        if let UpdateState::Available(r) = &a.update {
            ui.hyperlink_to("note di rilascio", &r.page);
        }

        if rescan { a.rescan_now = true; }
    }
}

/// Lista separata da virgole; applicata a fine modifica. Ritorna true se la lista è cambiata.
fn list_edit(ui: &mut egui::Ui, buf: &mut String, list: &mut Vec<String>) -> bool {
    let r = ui.add(egui::TextEdit::singleline(buf).desired_width(180.0));
    if !r.lost_focus() { return false; }
    let parsed: Vec<String> = buf.split(',').map(|s| s.trim().trim_start_matches('.').to_lowercase()).filter(|s| !s.is_empty()).collect();
    *buf = parsed.join(", ");
    if parsed == *list { return false; }
    *list = parsed;
    true
}

/// Riga ffmpeg/ffprobe: testo + selettore file + esito di `-version`. Ritorna true se il percorso è cambiato.
fn tool_edit(ui: &mut egui::Ui, label: &str, buf: &mut String, value: &mut String, ok: &mut bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let (icon, col, tip) = if *ok { ("✔", Color32::from_rgb(90, 200, 110), "risponde a -version") } else { ("✖", Color32::from_rgb(230, 90, 80), "non eseguibile") };
        ui.label(RichText::new(icon).color(col)).on_hover_text(tip);
        ui.label(label);
        if ui.button("📁").clicked() {
            if let Some(p) = rfd::FileDialog::new().add_filter("eseguibile", &["exe"]).pick_file() {
                *buf = p.display().to_string();
                changed = true;
            }
        }
        if ui.add(egui::TextEdit::singleline(buf).desired_width(f32::INFINITY)).lost_focus() { changed = true; }
    });
    let new = buf.trim().to_string();
    if !changed || new == *value { return false; }
    *value = new;
    *ok = tool_ok(value);
    true
}
