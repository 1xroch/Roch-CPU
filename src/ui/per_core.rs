//! Turbo ratio table editor: the multiplier the CPU may use for each number of active cores
//! (MSR 0x1AD/0x1AE for P-cores, 0x650/0x651 for E-cores). A fixed-size dialog window.

use std::sync::Arc;

use eframe::egui::{self, Align2, Id, IconData, Margin, Rect, Sense, Ui, ViewportBuilder, ViewportId, pos2, vec2};

use super::model::{IntelInfo, UiModel};
use super::text;
use super::theme::{self, Tone};

pub const VIEWPORT: &str = "roch-cpu-per-core";
const GROUPS: usize = 8;
pub const SIZE: egui::Vec2 = egui::Vec2::new(520.0, 420.0);

pub struct PerCoreDialog {
    model: Arc<dyn UiModel>,
    intel: IntelInfo,
    p: Vec<String>,
    e: Vec<String>,
    e_enabled: Vec<bool>,
    p_labels: Vec<String>,
    e_labels: Vec<String>,
    e_labels_muted: bool,
    status: String,
    status_tone: Tone,
    theme_sent: theme::TitleTheme,
    /// Centred on the main window when opened (`CenterParent`).
    pub position: Option<egui::Pos2>,
}

impl PerCoreDialog {
    pub fn new(model: Arc<dyn UiModel>, intel: IntelInfo) -> Self {
        let mut d = Self {
            model,
            intel,
            p: vec![String::new(); GROUPS],
            e: vec![String::new(); GROUPS],
            e_enabled: vec![true; GROUPS],
            p_labels: (0..GROUPS).map(|i| format!("group {}", i + 1)).collect(),
            e_labels: (0..GROUPS).map(|i| format!("group {}", i + 1)).collect(),
            e_labels_muted: false,
            status: String::new(),
            status_tone: Tone::Text,
            theme_sent: theme::TitleTheme::default(),
            position: None,
        };
        d.fill();
        d
    }

    fn fill(&mut self) {
        let r: anyhow::Result<()> = (|| {
            let pt = self.model.read_p_core_turbo_table()?;
            for i in 0..GROUPS {
                self.p[i] = pt.ratios.get(i).copied().unwrap_or(0).to_string();
                self.p_labels[i] = text::p_group_label(pt.cores.get(i).copied().unwrap_or(0));
            }
            if self.intel.e_core_count > 0 {
                let et = self.model.read_e_core_turbo_table()?;
                let ratios: Vec<i32> = (0..GROUPS).map(|i| et.ratios.get(i).copied().unwrap_or(0)).collect();
                let cores: Vec<i32> = (0..GROUPS).map(|i| et.cores.get(i).copied().unwrap_or(0)).collect();
                for i in 0..GROUPS {
                    self.e_enabled[i] = ratios[i] > 0;
                    self.e[i] = if ratios[i] > 0 { ratios[i].to_string() } else { "N/A".into() };
                    self.e_labels[i] = text::e_group_label(i, &ratios, &cores);
                }
            } else {
                for i in 0..GROUPS {
                    self.e[i] = "N/A".into();
                    self.e_enabled[i] = false;
                }
                self.e_labels_muted = true;
            }
            self.status = "Values read from the CPU.".into();
            Ok(())
        })();
        if let Err(e) = r {
            self.status = e.to_string();
            self.status_tone = Tone::Danger;
        }
    }

    fn apply(&mut self) {
        let e: Vec<(String, bool)> = self.e.iter().cloned().zip(self.e_enabled.iter().copied()).collect();
        let r: anyhow::Result<()> = (|| {
            let (pr, er) = text::validate_turbo(&self.p, &e, self.intel.e_core_count).map_err(anyhow::Error::msg)?;
            self.model.write_p_core_turbo_table(&pr)?;
            if self.intel.e_core_count > 0 {
                self.model.write_e_core_turbo_table(&er)?;
            }
            Ok(())
        })();
        match r {
            Ok(()) => {
                self.fill();
                self.status = format!("Tables read back at {}; live clocks depend on CPU limits.", text::clock());
                self.status_tone = Tone::Text;
            }
            Err(e) => {
                self.status = e.to_string();
                self.status_tone = Tone::Danger;
            }
        }
    }

    fn same_for_all(&mut self) {
        let p0 = self.p[0].clone();
        self.p.iter_mut().skip(1).for_each(|t| *t = p0.clone());
        let e0 = self.e[0].clone();
        for i in 1..GROUPS {
            if self.e_enabled[i] {
                self.e[i] = e0.clone();
            }
        }
    }

    /// Shows the dialog; returns false once it has been closed.
    pub fn show(&mut self, ctx: &egui::Context, icon: &Arc<IconData>) -> bool {
        let mut builder = ViewportBuilder::default()
            .with_title("Per-core ratio table")
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
        ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
            ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
            theme::wrapped_label(
                ui,
                "The ratio the CPU may reach with N cores active. The last group is the all-core limit.",
                theme::base(),
                theme::text(),
                inner.width(),
                Margin::ZERO,
            );
            ui.add_space(12.0);
            let top = ui.cursor().min.y;
            let half = inner.width() / 2.0;
            for (col, title) in ["P-cores", "E-cores"].iter().enumerate() {
                let x = inner.min.x + col as f32 * half;
                theme::paint_text(ui, pos2(x, top), Align2::LEFT_TOP, &title.to_uppercase(), theme::bold(), theme::text(), None);
            }
            ui.allocate_exact_size(vec2(inner.width(), 18.0), Sense::hover());
            let table_top = ui.cursor().min.y;
            for i in 0..GROUPS {
                let y = table_top + i as f32 * 28.0;
                for col in 0..2 {
                    let x = inner.min.x + col as f32 * half;
                    let (label, muted) = if col == 0 { (&self.p_labels[i], false) } else { (&self.e_labels[i], self.e_labels_muted) };
                    theme::paint_text(
                        ui,
                        pos2(x, y + 6.0),
                        Align2::LEFT_TOP,
                        label,
                        theme::base(),
                        if muted { theme::muted() } else { theme::text() },
                        None,
                    );
                    let box_rect = Rect::from_min_size(pos2(x + half - 12.0 - 64.0, y + 2.0), vec2(64.0, 24.0));
                    let id = Id::unique(("turbo", col, i));
                    if col == 0 {
                        theme::value_box(ui, box_rect, id, &mut self.p[i], true, false, theme::accent());
                    } else {
                        let enabled = self.e_enabled[i];
                        let color = if enabled { theme::accent() } else { theme::muted() };
                        theme::value_box(ui, box_rect, id, &mut self.e[i], enabled, false, color);
                    }
                }
            }
            ui.allocate_exact_size(vec2(inner.width(), GROUPS as f32 * 28.0), Sense::hover());

            // status and buttons at the bottom, right to left: Close, Apply, Same for all
            let buttons_top = inner.max.y - 30.0;
            theme::paint_text(
                ui,
                pos2(inner.min.x, buttons_top - 24.0),
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
            if theme::button_at(ui, apply, "Apply", true, true, theme::bold()).clicked() {
                self.apply();
            }
            if theme::button_at(ui, sync, "Same for all", false, true, theme::base()).clicked() {
                self.same_for_all();
            }
        });
        open
    }
}
