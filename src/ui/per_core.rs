//! Per-core ratio editor: one ratio per physical P-core, each core's own ratio limit (OC mailbox
//! 0x1C/0x1D run on that core) - what MSI Dragon Power's "Per Core Ratio" sets. A core runs no
//! faster than its value. E-cores share one clock per cluster of four, so they get one ratio per
//! cluster. The all-core limits stay on the main window's CPU Ratio and E-Core Ratio rows.
//!
//! An earlier version also edited the turbo table by number of active cores here. It was removed:
//! "1 P-core active" read as "P-core 1", and a low first group silently capped every core.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

use eframe::egui::{self, Align2, Id, IconData, Margin, Rect, Ui, ViewportBuilder, ViewportId, pos2, vec2};

use super::model::{IntelInfo, UiModel};
use super::text;
use super::theme::{self, Tone};

pub const VIEWPORT: &str = "roch-cpu-per-core";
const ROW: f32 = 28.0;
pub const SIZE: egui::Vec2 = egui::Vec2::new(380.0, 500.0);

pub struct PerCoreDialog {
    model: Arc<dyn UiModel>,
    cores: Vec<String>,
    /// One box per E-core cluster, with its label ("E-cores 6-9").
    clusters: Vec<(String, String)>,
    status: String,
    status_tone: Tone,
    /// An Apply running on a worker thread: each core's limit is a mailbox write with a polled
    /// read-back, which on this firmware can take a moment per core, so it stays off the UI thread.
    pending: Option<Receiver<anyhow::Result<()>>>,
    theme_sent: theme::TitleTheme,
    /// Centred on the main window when opened (`CenterParent`).
    pub position: Option<egui::Pos2>,
}

impl PerCoreDialog {
    pub fn new(model: Arc<dyn UiModel>, _intel: IntelInfo) -> Self {
        let mut d = Self {
            model,
            cores: Vec::new(),
            clusters: Vec::new(),
            status: String::new(),
            status_tone: Tone::Text,
            pending: None,
            theme_sent: theme::TitleTheme::default(),
            position: None,
        };
        d.fill();
        d
    }

    fn fill(&mut self) {
        let p = self.model.read_p_core_ratio_ceilings();
        let e = self.model.read_e_cluster_ratios();
        self.cores = p.as_ref().map(|v| v.iter().map(|r| r.to_string()).collect()).unwrap_or_default();
        self.clusters = e.as_ref().map(|v| v.iter().map(|(l, r)| (l.clone(), r.to_string())).collect()).unwrap_or_default();
        match (p, e) {
            (Err(err), _) | (_, Err(err)) => {
                self.status = err.to_string();
                self.status_tone = Tone::Danger;
            }
            _ => {
                self.status = "Ratios read from the CPU.".into();
                self.status_tone = Tone::Text;
            }
        }
    }

    fn apply(&mut self) {
        let valid = |v: &Vec<i32>| v.iter().all(|r| (8..=120).contains(r));
        let p: Option<Vec<i32>> = self.cores.iter().map(|t| t.trim().parse::<i32>().ok()).collect();
        let e: Option<Vec<i32>> = self.clusters.iter().map(|(_, t)| t.trim().parse::<i32>().ok()).collect();
        let (Some(p), Some(e)) = (p, e) else {
            return self.finish(Err(anyhow::anyhow!("Every box needs a whole-number ratio between 8 and 120.")));
        };
        if !valid(&p) || !valid(&e) {
            return self.finish(Err(anyhow::anyhow!("Every box needs a whole-number ratio between 8 and 120.")));
        }
        let (tx, rx) = channel();
        let model = self.model.clone();
        std::thread::spawn(move || {
            let r = (|| {
                if !p.is_empty() {
                    model.write_p_core_ratio_ceilings(&p)?;
                }
                if !e.is_empty() {
                    model.write_e_cluster_ratios(&e)?;
                }
                Ok(())
            })();
            let _ = tx.send(r);
        });
        self.pending = Some(rx);
        self.status = "Applying...".into();
        self.status_tone = Tone::Text;
    }

    /// Shows an Apply's outcome; a success re-reads every limit from the CPU.
    fn finish(&mut self, r: anyhow::Result<()>) {
        match r {
            Ok(()) => {
                self.fill();
                self.status = format!("Read back at {}; live clocks depend on CPU limits.", text::clock());
                self.status_tone = Tone::Text;
            }
            Err(e) => {
                self.status = e.to_string();
                self.status_tone = Tone::Danger;
            }
        }
    }

    fn same_for_all(&mut self) {
        if let Some(c0) = self.cores.first().cloned() {
            self.cores.iter_mut().for_each(|t| *t = c0.clone());
        }
        if let Some((_, e0)) = self.clusters.first().cloned() {
            self.clusters.iter_mut().for_each(|(_, t)| *t = e0.clone());
        }
    }

    /// Shows the dialog; returns false once it has been closed.
    pub fn show(&mut self, ctx: &egui::Context, icon: &Arc<IconData>) -> bool {
        let mut builder = ViewportBuilder::default()
            .with_title("Per-core ratio")
            .with_inner_size(SIZE)
            .with_resizable(false)
            .with_minimize_button(false)
            .with_maximize_button(false)
            .with_taskbar(false)
            .with_icon(icon.clone());
        if let Some(p) = self.position {
            builder = builder.with_position(p);
        }
        ctx.show_viewport_immediate(ViewportId::from_hash_of(VIEWPORT), builder, |ui, _class| {
            self.theme_sent.sync(ui.ctx());
            let open = self.contents(ui);
            open && !ui.input(|i| i.viewport().close_requested())
        })
    }

    fn contents(&mut self, ui: &mut Ui) -> bool {
        let full = ui.max_rect();
        ui.painter().rect_filled(full, 0.0, theme::bg());
        let inner = full.shrink(14.0);
        let mut open = true;
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(r) => {
                    self.pending = None;
                    self.finish(r);
                }
                Err(TryRecvError::Empty) => ui.ctx().request_repaint_after(Duration::from_millis(100)),
                Err(TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.finish(Err(anyhow::anyhow!("Apply stopped unexpectedly.")));
                }
            }
        }
        let idle = self.pending.is_none();
        ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
            ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
            theme::wrapped_label(
                ui,
                "Each core's own ratio. E-cores share a clock in clusters of four, so they are set per cluster.",
                theme::base(),
                theme::text(),
                inner.width(),
                Margin::ZERO,
            );
            ui.add_space(12.0);
            // Rows are placed at explicit positions from here down. The value boxes advance the
            // layout cursor themselves, so reading it after a table (or allocating the table's
            // height on top) pushed the E-core section down onto the buttons.
            let mut y = ui.cursor().min.y;
            let header = |ui: &mut Ui, y: f32, title: &str| {
                theme::paint_text(ui, pos2(inner.min.x, y), Align2::LEFT_TOP, title, theme::bold(), theme::text(), None);
            };
            header(ui, y, "P-CORES");
            y += 20.0;
            for i in 0..self.cores.len() {
                theme::paint_text(ui, pos2(inner.min.x, y + 6.0), Align2::LEFT_TOP, &format!("P-core {i}"), theme::base(), theme::text(), None);
                let box_rect = Rect::from_min_size(pos2(inner.max.x - 64.0, y + 2.0), vec2(64.0, 24.0));
                theme::value_box(ui, box_rect, Id::unique(("per-core", i)), &mut self.cores[i], true, false, theme::accent());
                y += ROW;
            }
            if !self.clusters.is_empty() {
                y += 10.0;
                header(ui, y, "E-CORES");
                y += 20.0;
                for (i, (label, value)) in self.clusters.iter_mut().enumerate() {
                    theme::paint_text(ui, pos2(inner.min.x, y + 6.0), Align2::LEFT_TOP, label, theme::base(), theme::text(), None);
                    let box_rect = Rect::from_min_size(pos2(inner.max.x - 64.0, y + 2.0), vec2(64.0, 24.0));
                    theme::value_box(ui, box_rect, Id::unique(("per-cluster", i)), value, true, false, theme::accent());
                    y += ROW;
                }
            }
            let _ = y;

            // status and buttons at the bottom, right to left: Close, Apply, Same for all
            let buttons_top = inner.max.y - 30.0;
            theme::paint_text(
                ui,
                pos2(inner.min.x, buttons_top - 38.0),
                Align2::LEFT_TOP,
                &self.status,
                theme::base(),
                self.status_tone.color(),
                Some(inner.width()),
            );
            let close = Rect::from_min_size(pos2(inner.max.x - 80.0, buttons_top), vec2(80.0, 30.0));
            let apply = Rect::from_min_size(pos2(close.min.x - 4.0 - 90.0, buttons_top), vec2(90.0, 30.0));
            let sync = Rect::from_min_size(pos2(apply.min.x - 4.0 - 100.0, buttons_top), vec2(100.0, 30.0));
            if theme::button_at(ui, close, "Close", false, true, theme::base()).clicked() {
                open = false;
            }
            if theme::button_at(ui, apply, "Apply", true, idle && !self.cores.is_empty(), theme::bold()).clicked() {
                self.apply();
            }
            if theme::button_at(ui, sync, "Same for all", false, idle && !self.cores.is_empty(), theme::base()).clicked() {
                self.same_for_all();
            }
        });
        open
    }
}
