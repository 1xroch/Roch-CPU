//! Per-core Curve Optimizer editor: the voltage/frequency curve offset the SMU applies to each
//! core, in counts (one count is roughly 3 to 5 mV). Negative undervolts. The core numbering
//! follows the SMU's physical map (CCD, core-in-CCD), which is what the BIOS shows too.

use std::sync::Arc;

use eframe::egui::{self, Align2, Id, IconData, Rect, Ui, ViewportBuilder, ViewportId, pos2, vec2};

use super::model::{AmdInfo, CoreInfo, UiModel};
use super::text;
use super::theme::{self, Tone};

pub const VIEWPORT: &str = "roch-cpu-curve-optimizer";
const ROW_HEIGHT: f32 = 30.0;

struct Row {
    core: CoreInfo,
    text: String,
    status: String,
    tone: Tone,
}

pub struct CurveOptimizerDialog {
    model: Arc<dyn UiModel>,
    amd: AmdInfo,
    rows: Vec<Row>,
    status: String,
    status_tone: Tone,
    theme_sent: theme::TitleTheme,
    /// Centred on the main window when opened (`CenterParent`).
    pub position: Option<egui::Pos2>,
}

impl CurveOptimizerDialog {
    pub fn new(model: Arc<dyn UiModel>) -> Self {
        let amd = model.amd().unwrap_or_default();
        let rows = model
            .amd_cores()
            .into_iter()
            .map(|core| Row { status: core.location.clone(), core, text: String::new(), tone: Tone::Text })
            .collect();
        let mut d = Self { model, amd, rows, status: String::new(), status_tone: Tone::Text, theme_sent: theme::TitleTheme::default(), position: None };
        d.fill();
        d
    }

    pub fn size(&self) -> egui::Vec2 {
        let cols = self.columns();
        vec2(if cols == 2 { 620.0 } else { 440.0 }, 100.0 + ROW_HEIGHT * self.per_column() as f32)
    }

    fn columns(&self) -> usize {
        if self.rows.len() > 8 { 2 } else { 1 }
    }

    fn per_column(&self) -> usize {
        self.rows.len().div_ceil(self.columns()).max(1)
    }

    fn fill(&mut self) {
        let mut unread = 0;
        for row in &mut self.rows {
            match self.model.read_curve_optimizer(&row.core) {
                Some(v) => row.text = v.to_string(),
                None => {
                    unread += 1;
                    if row.text.is_empty() {
                        row.text = "0".into();
                    }
                }
            }
        }
        self.status_tone = Tone::Text;
        self.status = text::co_fill_status(unread, self.amd.has_curve_optimizer_readback);
    }

    fn apply(&mut self) {
        let range = self.amd.co_range;
        let (mut ok, mut failed) = (0, 0);
        for row in &mut self.rows {
            let margin = match text::validate_margin(&row.text, range) {
                Ok(m) => m,
                Err(e) => {
                    row.status = e;
                    row.tone = Tone::Danger;
                    failed += 1;
                    continue;
                }
            };
            match self.model.apply_curve_optimizer(&row.core, margin) {
                Ok(()) => {
                    row.status = row.core.location.clone();
                    row.tone = Tone::Text;
                    ok += 1;
                }
                Err(e) => {
                    row.status = e.to_string();
                    row.tone = Tone::Danger;
                    failed += 1;
                }
            }
        }
        self.status = if failed == 0 {
            format!("Applied {ok} core(s) at {}.", text::clock())
        } else {
            format!("{ok} applied, {failed} failed.")
        };
        self.status_tone = if failed == 0 { Tone::Text } else { Tone::Danger };
        if self.amd.has_curve_optimizer_readback {
            self.fill();
        }
    }

    /// Shows the dialog; returns false once it has been closed.
    pub fn show(&mut self, ctx: &egui::Context, icon: &Arc<IconData>) -> bool {
        let size = self.size();
        let mut builder = ViewportBuilder::default()
            .with_title("Curve Optimizer")
            .with_inner_size(size)
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
            self.contents(ui);
            !ui.input(|i| i.viewport().close_requested())
        })
    }

    fn contents(&mut self, ui: &mut Ui) {
        let full = ui.max_rect();
        ui.painter().rect_filled(full, 0.0, theme::bg());
        let inner = full.shrink(14.0);
        let cols = self.columns();
        let per_col = self.per_column();
        let col_width = inner.width() / cols as f32;
        let name_width = self
            .rows
            .iter()
            .map(|r| ui.painter().layout_no_wrap(r.core.label.clone(), theme::base(), theme::text()).size().x)
            .fold(0.0, f32::max);
        for (i, row) in self.rows.iter_mut().enumerate() {
            let (col, line) = (i / per_col, i % per_col);
            let x = inner.min.x + col as f32 * col_width;
            let y = inner.min.y + line as f32 * ROW_HEIGHT;
            theme::paint_text(ui, pos2(x, y + 6.0), Align2::LEFT_TOP, &row.core.label, theme::base(), theme::text(), None);
            let box_rect = Rect::from_min_size(pos2(x + col_width - 12.0 - 64.0, y + 2.0), vec2(64.0, 24.0));
            let status_rect = Rect::from_min_max(pos2(x + name_width + 8.0, y), pos2(box_rect.min.x - 8.0, y + 26.0));
            theme::paint_truncated(ui, status_rect, false, &row.status, theme::base(), row.tone.color());
            theme::value_box(ui, box_rect, Id::unique(("co", row.core.index)), &mut row.text, true, false, theme::accent());
        }

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
        // Right to left: Apply, then Refresh.
        let apply = Rect::from_min_size(pos2(inner.max.x - 90.0, buttons_top), vec2(90.0, 30.0));
        let refresh = Rect::from_min_size(pos2(apply.min.x - 4.0 - 80.0, buttons_top), vec2(80.0, 30.0));
        if theme::button_at(ui, apply, "Apply", true, true, theme::bold()).clicked() {
            self.apply();
        }
        if theme::button_at(ui, refresh, "Refresh", false, true, theme::base()).clicked() {
            self.fill();
        }
    }
}
