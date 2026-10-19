//! The main window (the .NET `MainForm`): borderless, a drawn title bar, the hardware header,
//! one compact line per setting grouped into sections, Apply / Reset and the social footer.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, Id, IconData, Margin, Pos2, Rect, ResizeDirection, Sense, Stroke,
    StrokeKind, TextureHandle, Ui, UiBuilder, Vec2, ViewportCommand, pos2, vec2,
};

use super::curve_optimizer::CurveOptimizerDialog;
use super::log_window::{LogBuffer, LogWindow};
use super::model::UiModel;
use super::per_core::PerCoreDialog;
use super::preview::PreviewOptions;
use super::text::{self, APP_NAME, APP_VERSION};
use super::theme::{self, Caption, Tone};
use super::win::{self, Prompt};
use crate::model::memory_voltage_link::{Drafts, MemoryVoltageLink};
use crate::model::setting::{Setting, SettingGroup};

const RESIZE_BORDER: f32 = 6.0;
const TITLE_HEIGHT: f32 = 30.0;
const FOOTER_HEIGHT: f32 = 24.0;
/// Low rate on purpose: the BCLK row needs a measurement and the Vcore annotation a sample,
/// and nothing on this window justifies hammering the hardware.
const SLOW_REFRESH: Duration = Duration::from_millis(3000);
/// An apply result stays readable this long before the live reading takes the label back.
const RESULT_HOLD: Duration = Duration::from_secs(10);
const RAILS: [&str; 3] = ["VDD", "VDDQ", "VPP"];

/// Messages from worker threads back to the window.
enum Event {
    Initialized,
    RowApplied { id: String, ok: bool, ignored: bool },
    RestoreReadback,
    HardwareDone { status: String, error: bool },
}

/// Which text box has focus: a setting row or a shared DIMM rail editor.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum BoxKey {
    Row(String),
    Shared(String),
}

/// The right-hand column of a row: live reading, idle text or the last apply result.
#[derive(Clone, Debug)]
struct RowLabel {
    text: String,
    tone: Tone,
    tip: Option<String>,
}

#[derive(Default)]
struct Header {
    cpu: String,
    cores: String,
    microcode: String,
    board: String,
    agesa: Option<String>,
    bios: String,
    live: Option<String>,
    warn: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Starting,
    Initializing,
    Ready,
}

/// One section card: a group heading and the rows under it, in model order.
struct Section {
    group: SettingGroup,
    ids: Vec<String>,
}

pub struct MainWindow {
    model: Arc<dyn UiModel>,
    ctx: egui::Context,
    preview: Option<PreviewOptions>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    log: Arc<LogBuffer>,
    log_window: LogWindow,
    icon: Arc<IconData>,
    mark: Option<TextureHandle>,
    phase: Phase,
    title: String,
    header: Header,

    // rows
    settings: Vec<Setting>,
    /// The rows that have a box, in build order (the .NET `_boxes` keys).
    rows: Vec<String>,
    sections: Vec<Section>,
    /// What each box shows; survives a rebuild (the .NET `_settingDrafts`).
    drafts: HashMap<String, String>,
    labels: HashMap<String, RowLabel>,
    /// When each row last showed an apply result in its range label, so the live reading leaves it up for a while.
    row_results: HashMap<String, Instant>,
    /// Hover text for the name and box of each row.
    row_tips: HashMap<String, String>,
    /// Why the core target is unavailable, when it is.
    core_detail: Option<String>,
    pawnio_missing: bool,
    link: Option<MemoryVoltageLink>,
    shared_drafts: HashMap<String, String>,
    pending_shared: HashSet<String>,
    pending_memory: HashSet<String>,
    has_memory_section: bool,

    status: (String, bool),
    applying: bool,
    controls_enabled: bool,
    apply_enabled: bool,
    reset_enabled: bool,
    per_core_enabled: bool,
    per_core_text: String,
    bclk_busy: Arc<AtomicBool>,
    /// When the slow refresh last ran; `None` while it is stopped.
    last_tick: Option<Instant>,
    focused: Option<BoxKey>,
    focused_now: Option<BoxKey>,
    box_ids: HashSet<Id>,

    per_core: Option<PerCoreDialog>,
    curve: Option<CurveOptimizerDialog>,
    visuals_dark: Option<bool>,
    fit_pending: u8,
    scroll_pending: u8,
}

impl MainWindow {
    pub fn new(cc: &eframe::CreationContext<'_>, model: Arc<dyn UiModel>, preview: Option<PreviewOptions>, icon: Arc<IconData>) -> Self {
        let ctx = cc.egui_ctx.clone();
        theme::install_fonts(&ctx);
        ctx.set_theme(egui::ThemePreference::Light);
        theme::apply_visuals(&ctx);
        let mark = {
            let image = egui::ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba);
            let options = egui::TextureOptions { mipmap_mode: Some(egui::TextureFilter::Linear), ..egui::TextureOptions::LINEAR };
            Some(ctx.load_texture("mark", image, options))
        };
        let (tx, rx) = channel();
        let log = Arc::new(LogBuffer::default());
        if preview.is_none() {
            let (log, repaint) = (log.clone(), ctx.clone());
            model.subscribe_log(Arc::new(move |m: &str| {
                log.append(m);
                repaint.request_repaint();
            }));
        }
        Self {
            model,
            ctx,
            preview,
            tx,
            rx,
            log,
            log_window: LogWindow::default(),
            icon,
            mark,
            phase: Phase::Starting,
            title: APP_NAME.into(),
            header: Header::default(),
            settings: Vec::new(),
            rows: Vec::new(),
            sections: Vec::new(),
            drafts: HashMap::new(),
            labels: HashMap::new(),
            row_results: HashMap::new(),
            row_tips: HashMap::new(),
            core_detail: None,
            pawnio_missing: false,
            link: None,
            shared_drafts: HashMap::new(),
            pending_shared: HashSet::new(),
            pending_memory: HashSet::new(),
            has_memory_section: false,
            status: (String::new(), false),
            applying: false,
            controls_enabled: true,
            apply_enabled: false,
            reset_enabled: false,
            per_core_enabled: false,
            per_core_text: "Per-Core Ratio Table".into(),
            bclk_busy: Arc::new(AtomicBool::new(false)),
            last_tick: None,
            focused: None,
            focused_now: None,
            box_ids: HashSet::new(),
            per_core: None,
            curve: None,
            visuals_dark: Some(theme::is_dark()),
            fit_pending: 0,
            scroll_pending: 0,
        }
    }

    fn is_preview(&self) -> bool {
        self.preview.is_some()
    }

    fn append_log(&self, msg: &str) {
        self.log.append(msg);
    }

    fn set_status(&mut self, text: impl Into<String>, error: bool) {
        self.status = (text.into(), error);
    }

    fn setting(&self, id: &str) -> Option<&Setting> {
        self.settings.iter().find(|s| s.id == id)
    }

    fn core_read_failed(&self) -> bool {
        self.model.core_voltage_target_read_failed()
    }

    fn core_display_text(&self, s: &Setting) -> String {
        text::core_display_text(s, self.core_read_failed())
    }

    fn is_untouched_core_placeholder(&self, s: &Setting, t: &str) -> bool {
        text::is_untouched_core_placeholder(s, t, self.core_read_failed())
    }

    fn synced(&self) -> bool {
        self.link.as_ref().is_some_and(|l| l.is_synced())
    }

    // ------------------------------------------------------------------ lifecycle
    fn on_preview_load(&mut self) {
        let opts = self.preview.clone().unwrap_or_default();
        self.title = format!("{APP_NAME} - UI preview");
        self.ctx.send_viewport_cmd(ViewportCommand::Title(self.title.clone()));
        self.header = Header {
            cpu: "CPU: Fixture - Intel Core i5-14600KF".into(),
            cores: "Cores / Threads: 6 / 12".into(),
            microcode: "Microcode: 0x11F (fixture)".into(),
            board: "Motherboard: Z790MPOWER (fixture)".into(),
            agesa: None,
            bios: "BIOS: P.A0 (fixture)".into(),
            live: Some(format!(
                "Fixture: P x54 E x? Ring x50 | VID 1.270 V | {}",
                if opts.voltage_readings.as_deref() == Some("error") { "Vcore unavailable" } else { "Vcore 1.276 V" }
            )),
            warn: Some("UI PREVIEW - fixed fixture values; no hardware access or changes.".into()),
        };
        self.apply_enabled = false;
        self.reset_enabled = false;
        self.per_core_enabled = false;
        self.build_rows();
        self.set_status("UI preview - no hardware access", false);
        self.append_log("UI preview: hardware model, drivers and refresh timers are not initialized.");
        self.phase = Phase::Ready;
        self.fit_pending = 2;
        // Fixture-only layout aid: expose the final memory row without changing values,
        // starting refresh timers, invoking hardware callbacks or saving preferences.
        if opts.scroll_bottom {
            self.scroll_pending = 4;
        }
    }

    fn on_load_begin(&mut self) {
        let dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| format!("{}\\", d.display())))
            .unwrap_or_default();
        self.append_log(&format!("{APP_NAME} {APP_VERSION} starting from {dir}"));
        self.phase = Phase::Initializing;
        let (model, tx, ctx) = (self.model.clone(), self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            model.initialize();
            let _ = tx.send(Event::Initialized);
            ctx.request_repaint();
        });
    }

    fn on_load_finish(&mut self) {
        let m = self.model.clone();
        let h = m.header();
        let intel = m.intel();
        let amd = m.amd();
        if intel.is_some() || amd.is_some() {
            // Vendor-neutral summary from the model (Intel and AMD both feed it). The generation
            // string is deliberately not shown: it is still detected and still drives the
            // platform warning, it just does not earn a line in the header.
            self.header.cpu = format!("CPU: {}", h.cpu_name);
            self.header.cores = format!("Cores / Threads: {}", h.core_summary);
            self.header.microcode =
                if h.microcode_revision > 0 { format!("Microcode: 0x{:X}", h.microcode_revision) } else { "Microcode: Unknown".into() };
        } else {
            self.header.cpu = "CPU: No CPU access".into();
            self.header.cores = format!("Driver: {}", m.driver_status());
            self.header.microcode = "Microcode: Unknown".into();
        }
        self.header.board = format!("Motherboard: {}", h.board);
        self.header.agesa = (!h.agesa.is_empty()).then(|| format!("AGESA: {}", h.agesa));
        self.header.bios = format!("BIOS: {}", h.bios_version);

        let warns = text::warnings(&text::WarningInputs {
            driver_loaded: m.driver_loaded(),
            oc_locked: m.oc_locked(),
            voltage_access_restricted: m.voltage_access_restriction().is_some(),
            mailbox_available: m.mailbox_available(),
            intel,
            amd,
            smu_available: m.smu_available(),
            pbo_allowed: m.pbo_allowed(),
        });
        self.header.warn = (!warns.is_empty()).then(|| text::join_warnings(&warns));

        self.build_rows();
        if let Some(a) = amd {
            self.per_core_text = "Curve Optimizer (per core)".into();
            self.per_core_enabled = m.smu_available() && a.has_curve_optimizer;
            // The live line reports Intel P/E/ring ratios and VID; on AMD it would only hold an empty row.
            self.header.live = None;
        } else {
            self.per_core_enabled = intel.is_some();
            self.header.live = Some(String::new());
        }
        self.apply_enabled = true;
        self.reset_enabled = true;
        self.set_status(format!("Ready  \u{00B7}  {}", m.driver_status()), false);
        self.phase = Phase::Ready;

        self.last_tick = Some(Instant::now());
        self.slow_tick();
        self.fit_pending = 2;
    }

    /// One compact line per available setting; every row remains accessible through the viewport.
    fn build_rows(&mut self) {
        let synced = self.link.as_ref().map(|l| l.is_synced()).unwrap_or(!self.preview.as_ref().is_some_and(|p| p.split_dimms));
        let pending_ids: HashSet<String> = self.pending_memory.iter().map(|s| s.to_lowercase()).collect();
        self.rows.clear();
        self.sections.clear();
        self.labels.clear();
        self.row_results.clear();
        self.row_tips.clear();
        self.pending_memory.clear();
        self.has_memory_section = false;
        self.core_detail = None;
        self.settings = self.model.settings();
        let mut link = MemoryVoltageLink::new(&self.settings, &self.model.memory_grids());
        link.set_synced(synced);
        self.link = Some(link);
        self.pawnio_missing = !self.is_preview() && self.model.is_amd() && !self.model.pawnio_installed();

        let settings = self.settings.clone();
        let mut last: Option<SettingGroup> = None;
        for s in &settings {
            // Keep the core target visible when its existing verification gate refuses access.
            // Its target is not the measured Vcore or VID, and no value is read through that gate.
            let unavailable_core = s.id == "core_v" && !s.available;
            if !s.available && !unavailable_core {
                continue;
            }
            if last != Some(s.group) {
                last = Some(s.group);
                self.sections.push(Section { group: s.group, ids: Vec::new() });
                if s.group == SettingGroup::Memory {
                    self.has_memory_section = true;
                }
            }
            self.sections.last_mut().unwrap().ids.push(s.id.clone());
            self.rows.push(s.id.clone());

            let tip = text::row_tip(s);
            let mut label = RowLabel {
                text: if unavailable_core { "unavailable".into() } else { text::idle_text(s).into() },
                tone: if s.read_only() || unavailable_core { Tone::Muted } else { Tone::Text },
                tip: tip.clone(),
            };
            if let Some(t) = tip {
                self.row_tips.insert(s.id.clone(), t);
            }
            if unavailable_core {
                let detail = if self.is_preview() {
                    "Fixture: automatic startup core-page check failed; no voltage or mode target changed.".to_string()
                } else {
                    self.model.voltage_access_restriction().unwrap_or_else(|| {
                        let status = self.model.core_voltage_status();
                        if status != "not probed" {
                            status
                        } else {
                            s.note.clone().unwrap_or_else(|| "Core target is unavailable; see Log for the existing access checks.".into())
                        }
                    })
                };
                label.tip = Some(detail.clone());
                self.row_tips.insert(s.id.clone(), detail.clone());
                self.core_detail = Some(detail);
                self.drafts.insert(s.id.clone(), "Not read".into());
            } else {
                let shown = self.drafts.get(&s.id).cloned().unwrap_or_else(|| self.core_display_text(s));
                self.drafts.insert(s.id.clone(), shown);
            }
            self.labels.insert(s.id.clone(), label);
            if MemoryVoltageLink::try_get_rail(s).is_some() && pending_ids.contains(&s.id.to_lowercase()) {
                self.pending_memory.insert(s.id.clone());
            }
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::Initialized => self.on_load_finish(),
                Event::RowApplied { id, ok, ignored } => {
                    let fresh = self.model.setting(&id);
                    if let Some(f) = fresh.clone() {
                        if let Some(row) = self.settings.iter_mut().find(|s| s.id == id) {
                            *row = f;
                        }
                    }
                    let Some(s) = fresh.or_else(|| self.setting(&id).cloned()) else { continue };
                    let status = if ignored {
                        "board ignored it".to_string()
                    } else if ok {
                        s.last_result.clone().unwrap_or_else(|| "applied".into())
                    } else {
                        "failed - see Log".into()
                    };
                    self.set_row_status(&id, status, if ok && !ignored { Tone::Ok } else { Tone::Danger });
                    let shown = self.core_display_text(&s);
                    self.drafts.insert(id.clone(), shown);
                    self.pending_memory.remove(&id);
                }
                Event::RestoreReadback => {
                    self.row_results.clear();
                    self.settings = self.model.settings();
                    self.show_readback_texts();
                    self.reset_labels();
                }
                Event::HardwareDone { status, error } => {
                    self.set_status(status, error);
                    self.set_controls_enabled(true);
                    self.applying = false;
                    self.last_tick = Some(Instant::now());
                }
            }
        }
    }

    fn reset_labels(&mut self) {
        for id in self.rows.clone() {
            let Some(s) = self.setting(&id).cloned() else { continue };
            if let Some(l) = self.labels.get_mut(&id) {
                l.text = if !s.available { "unavailable".into() } else { text::idle_text(&s).into() };
                l.tone = if s.read_only() || !s.available { Tone::Muted } else { Tone::Text };
            }
        }
    }

    // ------------------------------------------------------------------ slow refresh
    fn slow_tick(&mut self) {
        if self.is_preview() || self.applying {
            return;
        }
        if self.model.is_amd() {
            self.amd_tick();
            return;
        }
        if self.model.intel().is_none() {
            return;
        }
        let live = self.model.read_live();
        self.header.live = Some(text::live_line(&live));
        self.refresh_bclk();
        self.settings = self.model.settings();

        // Existing rail reads continue at the same rate; a staged writable request stays intact.
        let board: Vec<Setting> = self.settings.iter().filter(|s| s.group == SettingGroup::Board && s.available).cloned().collect();
        for s in board {
            let Some(shown) = self.drafts.get(&s.id).cloned() else { continue };
            if !self.rows.contains(&s.id) {
                continue;
            }
            let untouched = self.focused != Some(BoxKey::Row(s.id.clone())) && shown == s.current_text();
            self.model.refresh(&s.id);
            let fresh = self.model.setting(&s.id).unwrap_or(s);
            if fresh.read_only() || untouched {
                self.drafts.insert(fresh.id.clone(), fresh.current_text());
            }
            self.store_snapshot(fresh);
        }

        // The mailbox voltage rows were read once at start-up and then never again, so a single
        // read that came back wrong - one was seen reporting a domain as Auto that was in fact
        // holding an override - stayed on screen for the life of the process. Re-read them, but
        // only where the box still shows what was last read: anything the user has typed is
        // theirs until they apply or revert it.
        let voltages: Vec<Setting> = self.settings.iter().filter(|s| s.group == SettingGroup::Voltages && s.available).cloned().collect();
        for s in voltages {
            if !self.rows.contains(&s.id) || self.focused == Some(BoxKey::Row(s.id.clone())) {
                continue;
            }
            let shown = self.drafts.get(&s.id).cloned().unwrap_or_default();
            let was = s.current_text();
            if shown != was && !self.is_untouched_core_placeholder(&s, &shown) {
                continue;
            }
            self.model.refresh(&s.id);
            let fresh = self.model.setting(&s.id).unwrap_or(s);
            let display = self.core_display_text(&fresh);
            if shown != display {
                self.drafts.insert(fresh.id.clone(), display);
            }
            self.store_snapshot(fresh);
        }
    }

    fn store_snapshot(&mut self, fresh: Setting) {
        if let Some(row) = self.settings.iter_mut().find(|s| s.id == fresh.id) {
            *row = fresh;
        }
    }

    /// Starts a base-clock measurement on a worker and shows the last finished one.
    fn refresh_bclk(&mut self) {
        if self.is_preview() {
            return;
        }
        if !self.bclk_busy.swap(true, Ordering::SeqCst) {
            let (model, busy, ctx) = (self.model.clone(), self.bclk_busy.clone(), self.ctx.clone());
            std::thread::spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| model.measure_bclk()));
                busy.store(false, Ordering::SeqCst);
                ctx.request_repaint();
            });
        }
        if let Some(lb) = self.model.last_bclk() {
            if self.rows.iter().any(|r| r == "bclk") && self.focused != Some(BoxKey::Row("bclk".into())) {
                self.model.set_current("bclk", Some(lb));
                if let Some(row) = self.settings.iter_mut().find(|s| s.id == "bclk") {
                    row.current = Some(lb);
                    let shown = row.format(lb);
                    self.drafts.insert("bclk".into(), shown);
                }
            }
        }
    }

    /// AMD: measured base clock, and what the CPU is drawing against each power limit.
    fn amd_tick(&mut self) {
        if self.is_preview() {
            return;
        }
        self.refresh_bclk();
        for id in self.rows.clone() {
            let Some(s) = self.setting(&id).cloned() else { continue };
            let Some(live) = s.live.clone() else { continue };
            if self.row_results.get(&id).is_some_and(|shown| shown.elapsed() < RESULT_HOLD) {
                continue;
            }
            let now = live();
            if let Some(l) = self.labels.get_mut(&id) {
                l.text = text::live_text(&s, now);
                l.tone = if s.read_only() { Tone::Muted } else { Tone::Text };
            }
        }
    }

    // ------------------------------------------------------------------ apply
    fn refresh_rows(&mut self) {
        if self.is_preview() {
            return;
        }
        self.row_results.clear();
        self.model.refresh_all();
        self.settings = self.model.settings();
        self.show_readback_texts();
        self.reset_labels();
    }

    fn show_readback_texts(&mut self) {
        for id in self.rows.clone() {
            if let Some(s) = self.setting(&id).cloned() {
                let shown = self.core_display_text(&s);
                self.drafts.insert(id, shown);
            }
        }
        self.pending_memory.clear();
        self.pending_shared.clear();
        self.shared_drafts.clear();
    }

    /// Runs hardware work on a worker thread so the window stays live, and puts the controls back
    /// when it finishes. Anything that writes hardware from the buttons goes through here.
    ///
    /// A base-clock or VDD2 change measures the result after every step, so it takes a second or
    /// more; doing that inline froze the window for the whole operation. The slow refresh is
    /// stopped for the duration, and not only so its repaint does not fight the row updates: it
    /// measures the base clock on its own worker, and two overlapping measurements share the same
    /// fixed counters, which would make a step look like it had missed and trigger a restore that
    /// was never needed.
    fn run_on_hardware<F>(&mut self, busy_text: &str, work: F)
    where
        F: FnOnce(&dyn UiModel, &Sender<Event>, &LogBuffer, &egui::Context) -> (String, bool) + Send + 'static,
    {
        if self.is_preview() || self.applying {
            return;
        }
        self.applying = true;
        self.last_tick = None;
        self.set_controls_enabled(false);
        self.set_status(busy_text, false);
        let (model, tx, log, ctx, busy) = (self.model.clone(), self.tx.clone(), self.log.clone(), self.ctx.clone(), self.bclk_busy.clone());
        std::thread::spawn(move || {
            // A base-clock measurement started by the last refresh may still be in flight, and it
            // owns the same fixed counters this is about to measure with. Let it finish.
            for _ in 0..40 {
                if !busy.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&*model, &tx, &log, &ctx)))
                .unwrap_or_else(|panic| {
                    let msg = panic
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "Unexpected error.".into());
                    (msg, true)
                });
            let _ = tx.send(Event::HardwareDone { status: result.0, error: result.1 });
            ctx.request_repaint();
        });
    }

    fn draft_text(&self, s: &Setting) -> String {
        self.drafts.get(&s.id).cloned().unwrap_or_else(|| s.current_text())
    }

    fn draft_targets(&self) -> Drafts {
        self.rows
            .iter()
            .filter_map(|id| self.setting(id))
            .map(|s| (s.id.clone(), self.draft_text(s)))
            .collect()
    }

    fn apply_all(&mut self) {
        if self.is_preview() || self.applying {
            return;
        }
        self.settings = self.model.settings();
        if let Err(memory_error) = self.prepare_memory_for_apply() {
            let shown = memory_error.clone().unwrap_or_else(|| "DIMM targets are invalid.".into());
            self.set_status(shown, true);
            self.append_log(&format!("Apply refused: {}", memory_error.unwrap_or_default()));
            return;
        }

        // Parsing and the confirmation dialog stay on the UI thread, where they belong.
        let mut work: Vec<(String, f64)> = Vec::new();
        let mut rejected = 0;
        let mut linked_rails: HashSet<String> = HashSet::new();
        let synced = self.synced();
        for id in self.rows.clone() {
            let Some(s) = self.setting(&id).cloned() else { continue };
            if !s.available || s.read_only() {
                continue;
            }
            if synced && let Some(rail) = MemoryVoltageLink::try_get_rail(&s) {
                if !linked_rails.insert(rail) {
                    continue;
                }
                let Some(group) = self.link.as_ref().and_then(|l| l.get_group(&self.settings, &s.id)) else { continue };
                if !group.members.iter().any(|m| text::is_memory_target_edited(m, &self.draft_text(m))) {
                    continue;
                }
                if !group.can_sync {
                    rejected += 1; // Defensive: preflight must already have rejected this group.
                    continue;
                }
                let mut linked_work = Vec::new();
                let mut confirmed = true;
                for member in &group.members {
                    if !self.rows.contains(&member.id) {
                        continue;
                    }
                    let target = self.draft_text(member).trim().to_string();
                    if !text::is_memory_target_edited(member, &target) {
                        continue;
                    }
                    // The complete group was validated before any work was queued.
                    let Some(linked_value) = member.try_parse(&target) else {
                        confirmed = false;
                        rejected += 1;
                        break;
                    };
                    if !self.confirm_dangerous(member, linked_value) {
                        confirmed = false;
                        break;
                    }
                    linked_work.push((member.id.clone(), linked_value));
                }
                if confirmed {
                    work.extend(linked_work);
                } else {
                    for member in &group.members {
                        self.set_row_status(&member.id, "group skipped", Tone::Muted);
                    }
                }
                continue;
            }
            let t = self.draft_text(&s).trim().to_string();
            if t.is_empty() {
                continue;
            }
            if self.is_untouched_core_placeholder(&s, &t) {
                continue;
            }
            if t.eq_ignore_ascii_case(&s.current_text()) && s.last_error.is_none() {
                continue;
            }
            let Some(value) = s.try_parse(&t) else {
                self.append_log(&format!("{}: '{t}' is not a number.", s.name));
                rejected += 1;
                self.set_row_status(&s.id, "invalid", Tone::Danger);
                continue;
            };
            if value != 0.0 && !self.confirm_dangerous(&s, value) {
                self.set_row_status(&s.id, "skipped", Tone::Muted);
                continue;
            }
            work.push((s.id.clone(), value));
        }

        if work.is_empty() {
            if rejected == 0 {
                self.set_status("No edits to apply; displayed values are register readings.", false);
            } else {
                self.set_status(format!("Applied at {}  \u{00B7}  0 ok, {rejected} failed", text::clock()), true);
            }
            return;
        }

        let pre_failed = rejected;
        self.run_on_hardware("Applying...", move |model, tx, log, ctx| {
            let (mut applied, mut failed) = (0, pre_failed);
            for (id, value) in work {
                let ok = model.apply(&id, value);
                let ignored = ok && (id == "core_v" || id == "core_off") && model.core_voltage_ignored();
                if ok && !ignored {
                    applied += 1;
                } else {
                    failed += 1;
                }
                let _ = tx.send(Event::RowApplied { id, ok, ignored });
                ctx.request_repaint();
            }
            log.append(&format!("Apply: {applied} applied, {failed} failed."));
            (format!("Applied at {}  \u{00B7}  {applied} ok, {failed} failed", text::clock()), failed > 0)
        });
    }

    fn set_controls_enabled(&mut self, on: bool) {
        self.controls_enabled = on;
        self.apply_enabled = on;
        self.reset_enabled = on;
    }

    fn confirm_dangerous(&self, s: &Setting, value: f64) -> bool {
        if !text::is_risky(&s.id, value) {
            return true;
        }
        win::confirm(&text::danger_prompt(s, value), APP_NAME, Prompt::Warning)
    }

    fn set_row_status(&mut self, id: &str, status: impl Into<String>, tone: Tone) {
        let Some(s) = self.setting(id).cloned() else { return };
        if let Some(l) = self.labels.get_mut(id) {
            l.text = status.into();
            l.tone = tone;
            l.tip = s.last_error.clone().or_else(|| text::row_tip(&s));
            self.row_results.insert(id.to_string(), Instant::now());
        }
    }

    fn restore_defaults(&mut self) {
        if self.is_preview() || self.applying {
            return;
        }
        if !win::confirm(text::RESET_PROMPT, APP_NAME, Prompt::Question) {
            return;
        }
        // On a worker for the same reason as Apply: putting the base clock and VDD2 back walks
        // them a step at a time, measuring each, which is seconds of work.
        self.run_on_hardware("Restoring...", |model, tx, log, ctx| {
            let failures = model.restore_all_defaults();
            model.refresh_all();
            let _ = tx.send(Event::RestoreReadback);
            ctx.request_repaint();
            let result = if failures == 0 {
                "Reset to Default: defaults/Auto restored.".to_string()
            } else {
                format!("Reset to Default: {failures} setting(s) failed; see Log.")
            };
            log.append(&result);
            (result, failures > 0)
        });
    }

    // ------------------------------------------------------------------ DIMM linking
    fn rail_source(&self, rail: &str) -> Option<String> {
        self.rows
            .iter()
            .filter_map(|id| self.setting(id))
            .find(|s| MemoryVoltageLink::try_get_rail(s).as_deref() == Some(rail))
            .map(|s| s.id.clone())
    }

    /// What the shared editor of a rail shows and whether it can be edited.
    fn shared_projection(&self, rail: &str) -> SharedProjection {
        let group = self.rail_source(rail).and_then(|id| self.link.as_ref()?.get_group(&self.settings, &id));
        let Some(group) = group else {
            return SharedProjection {
                text: self.shared_drafts.get(rail).cloned().unwrap_or_default(),
                enabled: false,
                tone: Tone::Muted,
                label: Some(("No detected rail".into(), Tone::Muted, None)),
                box_tip: None,
            };
        };
        let display = self.link.as_ref().unwrap().get_display(&group, &self.draft_targets());
        let drafted = self.shared_drafts.contains_key(rail);
        let enabled = display.can_edit;
        let tone = if display.is_mixed && !drafted {
            Tone::Muted
        } else if enabled {
            Tone::Accent
        } else {
            Tone::Muted
        };
        let text = self.shared_drafts.get(rail).cloned().unwrap_or(display.text);
        if !group.can_sync {
            return SharedProjection {
                text,
                enabled,
                tone,
                label: Some(("Sync unavailable - use Split DIMMs".into(), Tone::Warn, group.reason.clone())),
                box_tip: group.reason.clone(),
            };
        }
        // Hide the allowed range/step hint and its layout space; validation still uses
        // the exact same capability intersection and PMIC grids before staging/Apply.
        SharedProjection { text, enabled, tone, label: None, box_tip: Some(text::SHARED_BOX_TIP.into()) }
    }

    fn stage_shared_memory_draft(&mut self, rail: &str) -> bool {
        if !self.synced() || !self.has_memory_section {
            return true;
        }
        let Some(source) = self.rail_source(rail) else { return false };
        let shown = self.shared_projection(rail).text;
        let staged = self.link.as_ref().unwrap().try_stage(&self.settings, &source, &shown);
        match staged {
            Err(error) => {
                self.shared_drafts.insert(rail.into(), shown);
                self.pending_shared.insert(rail.into());
                self.set_status(error.unwrap_or_else(|| "DIMM targets could not be linked.".into()), true);
                false
            }
            Ok(updates) => {
                for (id, t) in updates {
                    self.drafts.insert(id.clone(), t);
                    self.pending_memory.remove(&id);
                }
                self.shared_drafts.remove(rail);
                self.pending_shared.remove(rail);
                true
            }
        }
    }

    fn stage_memory_draft(&mut self, source: &str) -> bool {
        if !self.synced() || !self.rows.iter().any(|r| r == source) {
            return true;
        }
        let shown = self.drafts.get(source).cloned().unwrap_or_default();
        let staged = self.link.as_ref().unwrap().try_stage(&self.settings, source, &shown);
        match staged {
            Err(error) => {
                self.set_row_status(source, "invalid Sync target", Tone::Danger);
                self.set_status(error.unwrap_or_else(|| "DIMM targets could not be linked.".into()), true);
                false
            }
            Ok(updates) => {
                for (id, t) in updates {
                    if self.rows.contains(&id) {
                        self.drafts.insert(id.clone(), t);
                    }
                    self.pending_memory.remove(&id);
                    self.set_row_status(&id, "staged", Tone::Text);
                }
                true
            }
        }
    }

    fn prepare_memory_for_apply(&mut self) -> Result<(), Option<String>> {
        if !self.synced() {
            return Ok(());
        }
        let mut pending: Vec<String> = self.pending_shared.iter().cloned().collect();
        pending.sort_by_key(|r| RAILS.iter().position(|x| x == r));
        for rail in pending {
            if !self.stage_shared_memory_draft(&rail) {
                return Err(Some(self.status.0.clone()));
            }
        }
        // Enter can request Apply while the edited textbox still has focus.
        if let Some(BoxKey::Row(id)) = self.focused.clone() {
            if self.pending_memory.contains(&id) && !self.stage_memory_draft(&id) {
                return Err(Some(self.status.0.clone()));
            }
        }
        let targets = self.draft_targets();
        self.link.as_ref().unwrap().validate_for_apply(&self.settings, &targets)
    }

    // ------------------------------------------------------------------ dialogs
    fn open_per_core(&mut self) {
        if self.is_preview() {
            return;
        }
        let parent = self.ctx.input(|i| i.viewport().outer_rect);
        let centre = |size: Vec2| parent.map(|r| r.center() - size / 2.0);
        if self.model.is_amd() {
            let mut d = CurveOptimizerDialog::new(self.model.clone());
            d.position = centre(d.size());
            self.curve = Some(d);
            return;
        }
        if let Some(intel) = self.model.intel() {
            let mut d = PerCoreDialog::new(self.model.clone(), intel);
            d.position = centre(super::per_core::SIZE);
            self.per_core = Some(d);
        }
    }

    fn toggle_log(&mut self) {
        if self.log_window.open {
            self.log_window.open = false;
        } else {
            let outer = self.ctx.input(|i| i.viewport().outer_rect);
            self.log_window.position = outer.map(|r| pos2(r.max.x + 6.0, r.min.y));
            self.log_window.open = true;
        }
    }

    fn toggle_theme(&mut self) {
        if let Err(e) = theme::toggle() {
            self.append_log(&format!("Theme preference could not be saved: {e}"));
        }
    }

    // ------------------------------------------------------------------ drawing
    fn window_ui(&mut self, ui: &mut Ui) {
        let full = ui.max_rect();
        ui.painter().rect_filled(full, CornerRadius::ZERO, theme::bg());
        let inner = full.shrink(1.0);

        let title_rect = Rect::from_min_size(inner.min, vec2(inner.width(), TITLE_HEIGHT));
        self.title_bar_ui(ui, title_rect);

        let footer_rect = Rect::from_min_max(pos2(inner.min.x, inner.max.y - FOOTER_HEIGHT), inner.max);
        let body_rect = Rect::from_min_max(pos2(inner.min.x + 10.0, title_rect.max.y + 2.0), pos2(inner.max.x - 10.0, footer_rect.min.y - 6.0));
        ui.scope_builder(UiBuilder::new().max_rect(body_rect).layout(egui::Layout::top_down(Align::Min)), |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            self.body_ui(ui);
        });
        self.footer_ui(ui, footer_rect);

        // The 1 px window outline the borderless form drew in OnPaint.
        ui.painter().rect_stroke(full, CornerRadius::ZERO, Stroke::new(1.0, theme::border()), StrokeKind::Inside);
    }

    fn title_bar_ui(&mut self, ui: &mut Ui, rect: Rect) {
        let drag = ui.interact(rect, Id::unique("title-drag"), Sense::click_and_drag());
        if drag.drag_started() {
            ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
        }
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::ZERO, theme::title_bar());
        let mut x = rect.min.x + 8.0;
        if let Some(mark) = &self.mark {
            let r = Rect::from_min_size(pos2(x, rect.min.y + 3.0), vec2(24.0, 24.0));
            painter.image(mark.id(), r, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
            x += 30.0;
        }
        theme::paint_text(
            ui,
            pos2(x, rect.min.y + 7.0),
            Align2::LEFT_TOP,
            &format!("{APP_NAME} {APP_VERSION}"),
            theme::bold(),
            theme::text(),
            None,
        );

        let close_rect = Rect::from_min_size(pos2(rect.max.x - 44.0, rect.min.y), vec2(44.0, TITLE_HEIGHT));
        let min_rect = close_rect.translate(vec2(-44.0, 0.0));
        let theme_rect = Rect::from_min_size(pos2(min_rect.min.x - 36.0, rect.min.y), vec2(36.0, TITLE_HEIGHT));
        if theme::title_button(ui, close_rect, Caption::Close).clicked() {
            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
        }
        if theme::title_button(ui, min_rect, Caption::Minimise).clicked() {
            ui.ctx().send_viewport_cmd(ViewportCommand::Minimized(true));
        }
        if theme::title_button(ui, theme_rect, Caption::Theme).on_hover_text(text::theme_tip(theme::is_dark())).clicked() {
            self.toggle_theme();
        }
    }

    fn body_ui(&mut self, ui: &mut Ui) {
        let width = ui.available_width();
        self.header_ui(ui, width);

        // Manual per-core controls remain; automatic ratio stepping has been removed.
        ui.add_space(4.0);
        let (row, _) = ui.allocate_exact_size(vec2(width, 36.0), Sense::hover());
        let button_rect = Rect::from_min_size(row.min, vec2(width, 30.0));
        let label = self.per_core_text.clone();
        if theme::button_at(ui, button_rect, &label, false, self.per_core_enabled, theme::bold()).clicked() {
            self.open_per_core();
        }
        ui.add_space(4.0);

        // Only the setting rows scroll; header, actions and footer remain reachable on smaller displays.
        let apply_height = 36.0;
        let rows_height = (ui.available_height() - apply_height).max(40.0);
        let output = egui::ScrollArea::vertical()
            .id_salt("rows")
            .auto_shrink([false, false])
            .max_height(rows_height)
            .min_scrolled_height(rows_height)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                ui.set_width(ui.available_width());
                self.rows_ui(ui);
            });
        let content_height = output.content_size.y;
        let viewport_height = output.inner_rect.height();

        // Apply / reset. Refresh-from-hardware remains automatic for live read-only rows.
        ui.add_space(4.0);
        let (row, _) = ui.allocate_exact_size(vec2(width, 32.0), Sense::hover());
        let half = (width / 2.0).floor();
        let apply_rect = Rect::from_min_size(row.min, vec2(half - 6.0, 32.0));
        let reset_rect = Rect::from_min_max(pos2(row.min.x + half, row.min.y), row.max);
        let apply_label = if self.controls_enabled || self.is_preview() { "Apply" } else { "Working..." };
        if theme::button_at(ui, apply_rect, apply_label, true, self.apply_enabled, theme::bold()).clicked() {
            self.apply_all();
        }
        if theme::button_at(ui, reset_rect, "Reset to Default", false, self.reset_enabled, theme::base())
            .on_hover_text(text::RESET_TIP)
            .clicked()
        {
            self.restore_defaults();
        }

        // Use natural content height within the work area; overflow stays in the settings viewport.
        if self.fit_pending > 0 && self.phase == Phase::Ready && !self.rows.is_empty() {
            self.fit_pending -= 1;
            if self.fit_pending == 0 {
                self.fit_to_content(content_height, viewport_height);
            }
        }
    }

    fn fit_to_content(&mut self, content_height: f32, viewport_height: f32) {
        let (inner, outer, monitor) =
            self.ctx.input(|i| (i.viewport().inner_rect, i.viewport().outer_rect, i.viewport().monitor_size));
        let Some(inner) = inner else { return };
        let needed = inner.height() - viewport_height + content_height + 2.0;
        let mut height = needed;
        if let Some(m) = monitor {
            height = height.min(m.y - 40.0);
        }
        let height = height.max(400.0).round();
        self.ctx.send_viewport_cmd(ViewportCommand::InnerSize(vec2(inner.width(), height)));
        if let (Some(outer), Some(m)) = (outer, monitor) {
            let bottom_limit = m.y - 40.0;
            if outer.min.y + height > bottom_limit {
                let top = (bottom_limit - height - 10.0).max(0.0);
                self.ctx.send_viewport_cmd(ViewportCommand::OuterPosition(pos2(outer.min.x, top)));
            }
        }
    }

    fn header_ui(&mut self, ui: &mut Ui, width: f32) {
        let wrap = (width - 60.0).max(100.0);
        let top = ui.cursor().min;
        // Log button top right.
        let log_rect = Rect::from_min_size(pos2(top.x + width - 64.0, top.y + 2.0), vec2(64.0, 24.0));
        let cpu = self.header.cpu.clone();
        let h = theme::text_height(ui, &cpu, theme::bold(), wrap);
        theme::paint_text(ui, top, Align2::LEFT_TOP, &cpu, theme::bold(), theme::text(), Some(wrap));
        ui.allocate_exact_size(vec2(width - 70.0, h.max(14.0)), Sense::hover());
        if theme::button_at(ui, log_rect, "Log", false, true, theme::base()).clicked() {
            self.toggle_log();
        }
        let mut lines: Vec<String> = vec![self.header.cores.clone(), self.header.microcode.clone(), self.header.board.clone()];
        if let Some(a) = &self.header.agesa {
            lines.push(a.clone());
        }
        lines.push(self.header.bios.clone());
        if let Some(l) = &self.header.live {
            lines.push(l.clone());
        }
        for line in lines {
            ui.add_space(1.0);
            theme::wrapped_label(ui, &line, theme::base(), theme::text(), wrap, Margin::ZERO);
        }
        if let Some(w) = self.header.warn.clone() {
            ui.add_space(4.0);
            theme::wrapped_label(ui, &w, theme::base(), theme::warn(), wrap.max(width - 20.0).min(width), Margin::ZERO);
        }
        ui.add_space(2.0);
    }

    fn rows_ui(&mut self, ui: &mut Ui) {
        ui.add_space(2.0);
        let sections: Vec<(SettingGroup, Vec<String>)> = self.sections.iter().map(|s| (s.group, s.ids.clone())).collect();
        let synced = self.synced();
        for (group, ids) in sections {
            theme::section_frame().show(ui, |ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                ui.set_width(ui.available_width());
                theme::paint_text(ui, ui.cursor().min, Align2::LEFT_TOP, &text::section_title(group), theme::bold(), theme::text(), None);
                ui.allocate_exact_size(vec2(ui.available_width(), 16.0), Sense::hover());
                theme::rule(ui);
                if group == SettingGroup::Memory {
                    self.memory_link_ui(ui);
                }
                if group == SettingGroup::Power && self.pawnio_missing {
                    pawnio_note_ui(ui);
                }
                for id in &ids {
                    let is_dimm = self.setting(id).is_some_and(|s| MemoryVoltageLink::try_get_rail(s).is_some());
                    if is_dimm && synced {
                        continue;
                    }
                    self.row_ui(ui, id);
                }
            });
            ui.add_space(6.0);
        }
    }

    fn row_ui(&mut self, ui: &mut Ui, id: &str) {
        let Some(s) = self.setting(id).cloned() else { return };
        let unavailable_core = s.id == "core_v" && !s.available;
        let width = ui.available_width();
        let name = text::display_name(&s);
        let name_h = theme::text_height(ui, &name, theme::base(), 175.0);
        let height = (name_h + 7.0).max(26.0);
        let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
        let x = rect.min.x;

        // name
        let name_rect = Rect::from_min_size(rect.min, vec2(180.0, height));
        theme::paint_text(ui, pos2(x + 6.0, rect.min.y + 4.0), Align2::LEFT_TOP, &name, theme::base(), theme::text(), Some(175.0));
        let tip = self.row_tips.get(id).cloned();
        let name_resp = ui.interact(name_rect, Id::unique(("row-name", id)), Sense::hover());
        if let Some(t) = &tip {
            name_resp.on_hover_text(t.as_str());
        }

        // box
        let box_rect = Rect::from_min_size(pos2(x + 180.0, rect.min.y + 1.0), vec2(76.0, 24.0));
        let enabled = s.available && !s.read_only();
        let color = if enabled { theme::accent() } else { theme::muted() };
        let box_id = Id::unique(("row-box", id));
        self.box_ids.insert(box_id);
        let mut shown = self.drafts.get(id).cloned().unwrap_or_default();
        let before = shown.clone();
        let response = theme::value_box(ui, box_rect, box_id, &mut shown, enabled, self.is_preview(), color);
        let changed = shown != before;
        if changed {
            self.drafts.insert(id.to_string(), shown);
        }
        if response.has_focus() {
            self.focused_now = Some(BoxKey::Row(id.to_string()));
        }
        let lost = response.lost_focus();
        if let Some(t) = &tip {
            response.on_hover_text(t.as_str());
        }
        if MemoryVoltageLink::try_get_rail(&s).is_some() {
            if changed && !self.applying && self.synced() {
                self.pending_memory.insert(id.to_string());
            }
            if lost && !self.applying && self.synced() && self.pending_memory.contains(id) {
                self.stage_memory_draft(id);
            }
        }

        // unit
        theme::paint_text(ui, pos2(x + 180.0 + 84.0 + 4.0, rect.min.y + 5.0), Align2::LEFT_TOP, &s.unit, theme::base(), theme::text(), None);

        // range / live / result
        let range_rect = Rect::from_min_max(pos2(x + 180.0 + 84.0 + 40.0 + 8.0, rect.min.y), pos2(rect.max.x - 8.0, rect.min.y + 24.0));
        if range_rect.width() > 4.0 {
            if let Some(label) = self.labels.get(id).cloned() {
                theme::paint_truncated(ui, range_rect.translate(vec2(0.0, 2.0)), true, &label.text, theme::base(), label.tone.color());
                let resp = ui.interact(range_rect, Id::unique(("row-range", id)), Sense::hover());
                if let Some(t) = label.tip.as_ref().or(tip.as_ref()) {
                    resp.on_hover_text(t.as_str());
                }
            }
        }

        if unavailable_core {
            let reason = theme::wrapped_label(
                ui,
                "Core voltage unavailable - see Log",
                theme::base(),
                theme::warn(),
                420.0,
                Margin { left: 6, right: 6, top: 0, bottom: 4 },
            );
            if let Some(d) = &self.core_detail {
                reason.on_hover_text(d.as_str());
            }
        }
        if self.scroll_pending > 0 {
            let target_last = self
                .preview
                .as_ref()
                .is_some_and(|_| !self.synced() && self.settings.last().is_some_and(|l| l.id == id));
            if target_last {
                ui.scroll_to_rect(rect, Some(Align::BOTTOM));
            }
        }
    }

    fn memory_link_ui(&mut self, ui: &mut Ui) {
        let synced = self.synced();
        let enabled = self.controls_enabled;
        ui.add_space(2.0);
        let mut set_synced: Option<bool> = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            ui.add_space(6.0);
            if theme::radio(ui, "Split DIMMs", !synced, enabled).clicked() {
                set_synced = Some(false);
            }
            ui.add_space(18.0);
            if theme::radio(ui, "Sync DIMMs", synced, enabled).clicked() {
                set_synced = Some(true);
            }
        });
        if let Some(on) = set_synced {
            // Changing mode only changes the projection; drafts and pending shared text survive.
            if let Some(link) = self.link.as_mut() {
                link.set_synced(on);
            }
        }
        ui.add_space(2.0);
        theme::wrapped_label(ui, text::MEMORY_HINT, theme::base(), theme::text(), 420.0, Margin { left: 6, right: 0, top: 0, bottom: 3 });
        if !self.synced() {
            return;
        }
        for rail in RAILS {
            self.shared_row_ui(ui, rail);
        }
    }

    fn shared_row_ui(&mut self, ui: &mut Ui, rail: &str) {
        let projection = self.shared_projection(rail);
        let width = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(width, 26.0), Sense::hover());
        let x = rect.min.x;
        theme::paint_text(ui, pos2(x + 6.0, rect.min.y + 4.0), Align2::LEFT_TOP, rail, theme::base(), theme::text(), None);
        let box_rect = Rect::from_min_size(pos2(x + 180.0, rect.min.y + 1.0), vec2(76.0, 24.0));
        let box_id = Id::unique(("shared-box", rail));
        self.box_ids.insert(box_id);
        let mut shown = projection.text.clone();
        let response = theme::value_box(ui, box_rect, box_id, &mut shown, projection.enabled, self.is_preview(), projection.tone.color());
        if shown != projection.text && !self.applying && response.has_focus() {
            self.shared_drafts.insert(rail.into(), shown);
            self.pending_shared.insert(rail.into());
        }
        if response.has_focus() {
            self.focused_now = Some(BoxKey::Shared(rail.into()));
        }
        let lost = response.lost_focus();
        if let Some(t) = &projection.box_tip {
            response.on_hover_text(t.as_str());
        }
        theme::paint_text(ui, pos2(x + 180.0 + 84.0 + 4.0, rect.min.y + 5.0), Align2::LEFT_TOP, "V", theme::base(), theme::text(), None);
        if let Some((label, tone, tip)) = &projection.label {
            let r = theme::wrapped_label(ui, label, theme::base(), tone.color(), 420.0, Margin { left: 6, right: 0, top: 0, bottom: 2 });
            if let Some(t) = tip {
                r.on_hover_text(t.as_str());
            }
        }
        if lost && !self.applying && self.synced() && self.pending_shared.contains(rail) {
            self.stage_shared_memory_draft(rail);
        }
        if self.scroll_pending > 0 && rail == "VPP" && self.preview.is_some() {
            ui.scroll_to_rect(rect, Some(Align::BOTTOM));
        }
    }

    fn footer_ui(&mut self, ui: &mut Ui, rect: Rect) {
        let inner = rect.shrink2(vec2(10.0, 0.0));
        // Roch Viewer's footer handle: brand red, bold, no underline, and colour is the whole
        // affordance - there is no button edge, so it lifts a shade under the pointer.
        let parts: [(&str, Option<&str>); 5] = [
            ("YouTube", Some(text::YOUTUBE_URL)),
            (" | ", None),
            ("X", Some(text::AUTHOR_URL)),
            (" | ", None),
            ("Discord", Some(text::DISCORD_URL)),
        ];
        let font = theme::bold();
        let y = inner.min.y + 5.0;
        let mut x = inner.min.x;
        let total = Rect::from_min_size(pos2(x, y), vec2(150.0, 16.0));
        let hovered_any = ui.rect_contains_pointer(total);
        let mut link_rects = Vec::new();
        for (part, url) in parts {
            let color = match url {
                Some(_) => {
                    if hovered_any { theme::HOVER } else { theme::SELECTED }
                }
                None => theme::text(),
            };
            let r = theme::paint_text(ui, pos2(x, y), Align2::LEFT_TOP, part, font.clone(), color, None);
            if let Some(u) = url {
                link_rects.push((r, u));
            }
            x = r.max.x;
        }
        for (r, url) in link_rects {
            let resp = ui.interact(r, Id::unique(("footer-link", url)), Sense::click());
            if self.is_preview() {
                continue;
            }
            let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
            if resp.clicked() {
                win::open_url(url);
            }
        }
        let status_rect = Rect::from_min_max(pos2(x + 6.0, inner.min.y), inner.max);
        let (status, error) = self.status.clone();
        if status_rect.width() > 4.0 {
            theme::paint_truncated(ui, status_rect, true, &status, theme::base(), if error { theme::danger() } else { theme::text() });
            if !status.is_empty() {
                ui.interact(status_rect, Id::unique("status"), Sense::hover()).on_hover_text(status);
            }
        }
    }

    /// Borderless window: resize from the 6 px edges (WM_NCHITTEST in .NET).
    fn resize_edges(&self, ctx: &egui::Context) {
        let (maximized, rect, pointer, pressed) = ctx.input(|i| {
            (
                i.viewport().maximized.unwrap_or(false),
                i.viewport_rect(),
                i.pointer.hover_pos(),
                i.pointer.primary_pressed(),
            )
        });
        if maximized {
            return;
        }
        let Some(p) = pointer else { return };
        let (l, r) = (p.x < rect.min.x + RESIZE_BORDER, p.x >= rect.max.x - RESIZE_BORDER);
        let (t, b) = (p.y < rect.min.y + RESIZE_BORDER, p.y >= rect.max.y - RESIZE_BORDER);
        let dir = match (t, b, l, r) {
            (true, _, true, _) => Some((ResizeDirection::NorthWest, CursorIcon::ResizeNorthWest)),
            (true, _, _, true) => Some((ResizeDirection::NorthEast, CursorIcon::ResizeNorthEast)),
            (_, true, true, _) => Some((ResizeDirection::SouthWest, CursorIcon::ResizeSouthWest)),
            (_, true, _, true) => Some((ResizeDirection::SouthEast, CursorIcon::ResizeSouthEast)),
            (_, _, true, _) => Some((ResizeDirection::West, CursorIcon::ResizeWest)),
            (_, _, _, true) => Some((ResizeDirection::East, CursorIcon::ResizeEast)),
            (true, _, _, _) => Some((ResizeDirection::North, CursorIcon::ResizeNorth)),
            (_, true, _, _) => Some((ResizeDirection::South, CursorIcon::ResizeSouth)),
            _ => None,
        };
        if let Some((direction, cursor)) = dir {
            ctx.set_cursor_icon(cursor);
            if pressed {
                ctx.send_viewport_cmd(ViewportCommand::BeginResize(direction));
            }
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let mut closed = false;
        if let Some(d) = self.per_core.as_mut() {
            if !d.show(ctx, &self.icon) {
                closed = true;
                self.per_core = None;
            }
        }
        if let Some(d) = self.curve.as_mut() {
            if !d.show(ctx, &self.icon) {
                closed = true;
                self.curve = None;
            }
        }
        if closed {
            self.refresh_rows();
        }
    }

    /// While a dialog is open the main window takes no input, as with `ShowDialog`.
    fn block_main_input(&self, ctx: &egui::Context) {
        let target = if self.per_core.is_some() {
            Some(super::per_core::VIEWPORT)
        } else if self.curve.is_some() {
            Some(super::curve_optimizer::VIEWPORT)
        } else {
            None
        };
        let Some(viewport) = target else { return };
        let rect = ctx.content_rect();
        egui::Area::new(Id::unique("modal-block")).order(egui::Order::Foreground).fixed_pos(rect.min).show(ctx, |ui| {
            let r = ui.allocate_rect(rect, Sense::click_and_drag());
            if r.clicked() || r.drag_started() {
                ctx.send_viewport_cmd_to(egui::ViewportId::from_hash_of(viewport), ViewportCommand::Focus);
            }
        });
    }
}

struct SharedProjection {
    text: String,
    enabled: bool,
    tone: Tone,
    label: Option<(String, Tone, Option<String>)>,
    box_tip: Option<String>,
}

/// Without PawnIO the SMU power table cannot be read, so the AMD limit rows start as Auto.
fn pawnio_note_ui(ui: &mut Ui) {
    let note = text::pawnio_note();
    let at = note.find(text::PAWNIO_SITE).unwrap_or(note.len());
    let (before, rest) = note.split_at(at);
    let after = &rest[text::PAWNIO_SITE.len().min(rest.len())..];
    egui::Frame::new().inner_margin(Margin { left: 6, right: 6, top: 2, bottom: 6 }).show(ui, |ui| {
        ui.set_max_width(440.0f32.min(ui.available_width()));
        ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        ui.label(egui::RichText::new(before).font(theme::base()).color(theme::warn()));
        let link = ui
            .add(egui::Label::new(egui::RichText::new(text::PAWNIO_SITE).font(theme::base()).color(theme::accent())).sense(Sense::click()))
            .on_hover_cursor(CursorIcon::PointingHand);
        if link.clicked() {
            win::open_url(text::PAWNIO_URL);
        }
        ui.label(egui::RichText::new(after).font(theme::base()).color(theme::warn()));
        });
    });
}

impl eframe::App for MainWindow {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        match self.phase {
            Phase::Starting if self.is_preview() => self.on_preview_load(),
            Phase::Starting => self.on_load_begin(),
            _ => {}
        }
        self.drain_events();

        if self.visuals_dark != Some(theme::is_dark()) {
            self.visuals_dark = Some(theme::is_dark());
            theme::apply_visuals(&ctx);
        }

        // Disposing the driver mid-apply could leave the base clock on an intermediate value with
        // nothing left to put it back. An apply is short; ask again in a moment.
        if ctx.input(|i| i.viewport().close_requested()) && self.applying {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.set_status("Still applying - closing once it finishes.", false);
        }

        if let Some(at) = self.last_tick {
            let elapsed = at.elapsed();
            if elapsed >= SLOW_REFRESH {
                self.last_tick = Some(Instant::now());
                self.slow_tick();
                ctx.request_repaint_after(SLOW_REFRESH);
            } else {
                ctx.request_repaint_after(SLOW_REFRESH - elapsed);
            }
        }
        if self.phase == Phase::Initializing {
            ctx.set_cursor_icon(CursorIcon::Wait);
        }

        self.focused_now = None;
        self.box_ids.clear();
        self.window_ui(ui);
        self.focused = self.focused_now.clone();
        if self.scroll_pending > 0 && self.fit_pending == 0 {
            self.scroll_pending -= 1;
            ctx.request_repaint();
        }
        if self.fit_pending > 0 {
            ctx.request_repaint();
        }

        let dialog_open = self.per_core.is_some() || self.curve.is_some();
        if !self.is_preview() && !dialog_open && self.phase == Phase::Ready {
            let enter = ctx.input(|i| i.key_pressed(egui::Key::Enter));
            let focus = ctx.memory(|m| m.focused());
            if enter && focus.is_none_or(|f| self.box_ids.contains(&f)) {
                self.apply_all();
            }
        }

        self.log_window.show(&ctx, &self.log, &self.icon);
        self.dialogs(&ctx);
        self.block_main_input(&ctx);
        if !dialog_open {
            self.resize_edges(&ctx);
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::bg().to_normalized_gamma_f32()
    }

    fn persist_egui_memory(&self) -> bool {
        false
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Let a base-clock measurement in flight finish before the model (and its driver) goes.
        for _ in 0..40 {
            if !self.bclk_busy.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
