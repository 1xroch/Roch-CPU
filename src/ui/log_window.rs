//! The log in its own window, like the Roch GPU "Log" button. Hidden rather than closed.

use std::sync::{Arc, Mutex};

use eframe::egui::{self, IconData, Pos2, Sense, Vec2, ViewportBuilder, ViewportId, vec2};

use super::text::{self, APP_NAME};
use super::theme;

/// Every line the window has been asked to log, time-stamped as it arrives.
#[derive(Default)]
pub struct LogBuffer {
    text: Mutex<String>,
}

impl LogBuffer {
    pub fn append(&self, message: &str) {
        let line = format_line(&text::clock(), message);
        self.text.lock().unwrap_or_else(|e| e.into_inner()).push_str(&line);
    }

    pub fn text(&self) -> String {
        self.text.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// `[HH:mm:ss] message` and a line break.
pub fn format_line(clock: &str, message: &str) -> String {
    format!("[{clock}] {message}\n")
}

pub const VIEWPORT: &str = "roch-cpu-log";

#[derive(Default)]
pub struct LogWindow {
    pub open: bool,
    /// Beside the main window, where it was when the log was opened.
    pub position: Option<Pos2>,
    /// The size the user last gave it; reopening keeps it, as hiding the .NET form did.
    size: Option<Vec2>,
    theme_sent: theme::TitleTheme,
}

impl LogWindow {
    pub fn show(&mut self, ctx: &egui::Context, log: &Arc<LogBuffer>, icon: &Arc<IconData>) {
        if !self.open {
            self.theme_sent = theme::TitleTheme::default();
            return;
        }
        let mut builder = ViewportBuilder::default()
            .with_title(format!("{APP_NAME} - Log"))
            .with_inner_size(self.size.unwrap_or(vec2(620.0, 420.0)))
            .with_taskbar(false)
            .with_icon(icon.clone());
        if let Some(p) = self.position {
            builder = builder.with_position(p);
        }
        let theme_sent = &mut self.theme_sent;
        let size = &mut self.size;
        let close = ctx.show_viewport_immediate(ViewportId::from_hash_of(VIEWPORT), builder, |ui, _class| {
            theme_sent.sync(ui.ctx());
            if let Some(r) = ui.input(|i| i.viewport().inner_rect) {
                *size = Some(r.size());
            }
            let rect = ui.max_rect();
            ui.painter().rect_filled(rect, 0.0, theme::bg());
            let inner = rect.shrink(8.0);
            let copy_rect = egui::Rect::from_min_max(egui::pos2(inner.min.x, inner.max.y - 28.0), inner.max);
            let text_rect = egui::Rect::from_min_max(inner.min, egui::pos2(inner.max.x, copy_rect.min.y));
            let content = log.text();
            ui.scope_builder(egui::UiBuilder::new().max_rect(text_rect), |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                    let mut view: &str = content.as_str();
                    ui.add(
                        egui::TextEdit::multiline(&mut view)
                            .font(theme::base())
                            .text_color(theme::text())
                            .frame(egui::Frame::NONE)
                            .desired_width(f32::INFINITY)
                            .desired_rows(1),
                    );
                    ui.allocate_exact_size(Vec2::ZERO, Sense::hover());
                });
            });
            if theme::button_at(ui, copy_rect, "Copy", false, true, theme::base()).clicked() {
                ui.ctx().copy_text(content.replace('\n', "\r\n"));
            }
            ui.input(|i| i.viewport().close_requested())
        });
        if close {
            self.open = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_time_stamped() {
        assert_eq!(format_line("09:05:01", "Driver: WinRing0"), "[09:05:01] Driver: WinRing0\n");
        let log = LogBuffer::default();
        log.append("one");
        log.append("two");
        let t = log.text();
        assert_eq!(t.lines().count(), 2);
        assert!(t.lines().nth(1).unwrap().ends_with("] two"));
    }
}
