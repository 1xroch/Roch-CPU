//! The Roch Studio look shared with Roch GPU and Roch Viewer: black, grey, red, white.
//! No OS chrome on the main window; a drawn title bar; flat controls with a red accent on hover.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};

use anyhow::Result;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontData, FontDefinitions, FontFamily, FontId, Margin, Rect, Response,
    Sense, Stroke, StrokeKind, Ui, Visuals, vec2,
};

static DARK: AtomicBool = AtomicBool::new(false);
static PREVIEW: AtomicBool = AtomicBool::new(false);
static LOADED: Once = Once::new();

fn preference_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Roch CPU").join("theme.txt"))
}

/// Light unless the user has chosen dark.
fn load_dark() -> bool {
    preference_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .is_some_and(|t| t.trim() == "dark")
}

pub fn is_dark() -> bool {
    LOADED.call_once(|| {
        if !PREVIEW.load(Ordering::SeqCst) {
            DARK.store(load_dark(), Ordering::SeqCst);
        }
    });
    DARK.load(Ordering::SeqCst)
}

/// The explicit fixture mode changes colors only in memory, without saving user preferences.
pub fn configure_preview(dark: bool) {
    PREVIEW.store(true, Ordering::SeqCst);
    LOADED.call_once(|| {});
    DARK.store(dark, Ordering::SeqCst);
}

/// Flips the palette and saves the choice (except in the preview). The palette has already
/// flipped when this returns an error; only the save failed.
pub fn toggle() -> Result<()> {
    let dark = !is_dark();
    DARK.store(dark, Ordering::SeqCst);
    if !PREVIEW.load(Ordering::SeqCst) {
        let path = preference_path().ok_or_else(|| anyhow::anyhow!("LOCALAPPDATA is not set."))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, if dark { "dark" } else { "light" })?;
    }
    Ok(())
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

fn tone(dark: u32, light: u32) -> Color32 {
    hex(if is_dark() { dark } else { light })
}

// Pure surfaces and shared semantic roles from the Roch family UI contract.
pub fn bg() -> Color32 {
    tone(0x000000, 0xFFFFFF)
}
pub fn panel() -> Color32 {
    bg()
}
pub fn panel_alt() -> Color32 {
    bg()
}
/// Title bar: the same colour as the body, so the window reads as one surface.
pub fn title_bar() -> Color32 {
    bg()
}
pub fn border() -> Color32 {
    tone(0x383838, 0xD0D0D0)
}
pub fn text() -> Color32 {
    tone(0xFFFFFF, 0x000000)
}
pub fn muted() -> Color32 {
    tone(0xA0A0A0, 0x62666D)
}
pub fn title_hover() -> Color32 {
    tone(0x222222, 0xE6E6E6)
}
pub const CLOSE_HOVER: Color32 = hex(0xC42B1C);
pub fn accent() -> Color32 {
    tone(0xFF5A5F, 0xB91C1C)
}
pub const SELECTED: Color32 = hex(0xD0343A);
pub const HOVER: Color32 = hex(0xE0383E);
pub const ACCENT_DIM: Color32 = SELECTED;
pub fn warn() -> Color32 {
    accent()
}
pub fn danger() -> Color32 {
    accent()
}
pub fn ok() -> Color32 {
    text()
}

/// Text colour roles, resolved against the palette at paint time so a theme switch recolours
/// every label that was already shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Text,
    Muted,
    Accent,
    Warn,
    Danger,
    Ok,
}

impl Tone {
    pub fn color(self) -> Color32 {
        match self {
            Tone::Text => text(),
            Tone::Muted => muted(),
            Tone::Accent => accent(),
            Tone::Warn => warn(),
            Tone::Danger => danger(),
            Tone::Ok => ok(),
        }
    }
}

// ---------------------------------------------------------------------- fonts
// Viewer uses 12 logical pixels (9 pt Consolas at 96 DPI); captions use Segoe UI Symbol.
pub const SIZE: f32 = 12.0;
pub const GLYPH_SIZE: f32 = 18.0;
pub const CLOSE_GLYPH_SIZE: f32 = 20.0;

pub fn base() -> FontId {
    FontId::new(SIZE, FontFamily::Monospace)
}
pub fn bold() -> FontId {
    FontId::new(SIZE, FontFamily::Name("bold".into()))
}
pub fn glyph(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("glyph".into()))
}

fn system_font(file: &str) -> Option<Vec<u8>> {
    let dir = std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    std::fs::read(dir.join("Fonts").join(file)).ok()
}

/// Consolas (regular and bold) and Segoe UI Symbol from the Windows font folder, each falling
/// back to egui's bundled fonts when the file is missing.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let defaults_mono = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let defaults_prop = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut add = |name: &str, file: &str| -> bool {
        match system_font(file) {
            Some(bytes) => {
                fonts.font_data.insert(name.into(), Arc::new(FontData::from_owned(bytes)));
                true
            }
            None => false,
        }
    };
    let consolas = add("consolas", "consola.ttf");
    let consolas_bold = add("consolas-bold", "consolab.ttf");
    let symbol = add("segoe-symbol", "seguisym.ttf");

    let chain = |first: Option<&str>, rest: &[String]| -> Vec<String> {
        let mut v: Vec<String> = first.map(|f| vec![f.to_string()]).unwrap_or_default();
        if symbol {
            rest.iter().for_each(|r| v.push(r.clone()));
            v.push("segoe-symbol".into());
        } else {
            rest.iter().for_each(|r| v.push(r.clone()));
        }
        v
    };
    let regular = chain(consolas.then_some("consolas"), &defaults_mono);
    let bold_chain = chain(consolas_bold.then_some("consolas-bold").or(consolas.then_some("consolas")), &defaults_mono);
    let glyphs = {
        let mut v: Vec<String> = if symbol { vec!["segoe-symbol".into()] } else { Vec::new() };
        v.extend(defaults_prop.iter().cloned());
        v
    };
    fonts.families.insert(FontFamily::Monospace, regular.clone());
    fonts.families.insert(FontFamily::Proportional, regular);
    fonts.families.insert(FontFamily::Name("bold".into()), bold_chain);
    fonts.families.insert(FontFamily::Name("glyph".into()), glyphs);
    ctx.set_fonts(fonts);
}

/// egui's own widgets (scroll bars, tooltips, text selection) in the current palette.
pub fn apply_visuals(ctx: &egui::Context) {
    let mut v = if is_dark() { Visuals::dark() } else { Visuals::light() };
    v.panel_fill = bg();
    v.window_fill = bg();
    v.faint_bg_color = bg();
    v.extreme_bg_color = panel_alt();
    v.window_stroke = Stroke::new(1.0, border());
    v.override_text_color = None;
    v.hyperlink_color = accent();
    v.selection.bg_fill = SELECTED.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, text());
    v.text_cursor.stroke = Stroke::new(1.0, text());
    v.window_corner_radius = CornerRadius::ZERO;
    v.menu_corner_radius = CornerRadius::ZERO;
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = CornerRadius::ZERO;
        w.fg_stroke.color = text();
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, border());
    v.widgets.inactive.bg_fill = border();
    v.widgets.inactive.weak_bg_fill = panel_alt();
    v.widgets.hovered.bg_fill = muted();
    v.widgets.active.bg_fill = accent();
    ctx.set_visuals_of(egui::Theme::Light, v.clone());
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.global_style_mut(|s| {
        s.interaction.selectable_labels = false;
        s.spacing.tooltip_width = 380.0;
        // A solid bar beside the rows, like the WinForms AutoScroll viewport, not one floating over them.
        s.spacing.scroll = egui::style::ScrollStyle::solid();
        s.override_font_id = Some(base());
    });
}

/// Keeps the OS title bar of a secondary window (log, dialogs) in the palette, like
/// `DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)`. The request is repeated over the
/// first frames because a command sent while the native window is still being created is lost.
#[derive(Default)]
pub struct TitleTheme {
    sent: Option<bool>,
    frames: u8,
}

impl TitleTheme {
    pub fn sync(&mut self, ctx: &egui::Context) {
        self.frames = self.frames.saturating_add(1);
        if self.sent != Some(is_dark()) || self.frames <= 4 {
            self.sent = Some(is_dark());
            ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(system_theme()));
            ctx.request_repaint();
        }
    }
}

/// The OS theme to request for windows that keep the OS title bar.
pub fn system_theme() -> egui::SystemTheme {
    if is_dark() { egui::SystemTheme::Dark } else { egui::SystemTheme::Light }
}

// ---------------------------------------------------------------------- painting helpers
/// Lays out `text` (wrapping at `wrap` when given) and paints it at `pos` aligned by `align`.
pub fn paint_text(ui: &Ui, pos: egui::Pos2, align: Align2, text: &str, font: FontId, color: Color32, wrap: Option<f32>) -> Rect {
    let galley = match wrap {
        Some(w) => ui.painter().layout(text.to_owned(), font, color, w),
        None => ui.painter().layout_no_wrap(text.to_owned(), font, color),
    };
    let rect = align.anchor_size(pos, galley.size());
    ui.painter().galley(rect.min, galley, color);
    rect
}

/// Height of `text` wrapped at `width`.
pub fn text_height(ui: &Ui, text: &str, font: FontId, width: f32) -> f32 {
    ui.painter().layout(text.to_owned(), font, Color32::WHITE, width).size().y
}

/// One-line text truncated with an ellipsis to fit `rect`, right- or left-aligned and centred
/// vertically (WinForms `AutoEllipsis`).
pub fn paint_truncated(ui: &Ui, rect: Rect, right: bool, text: &str, font: FontId, color: Color32) {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width().max(1.0));
    let galley = ui.painter().layout_job(job);
    let size = galley.size();
    let x = if right { rect.max.x - size.x } else { rect.min.x };
    let y = rect.center().y - size.y / 2.0;
    ui.painter().galley(egui::pos2(x, y), galley, color);
}

/// A word-wrapped label that takes its natural height; returns its hover response.
pub fn wrapped_label(ui: &mut Ui, text: &str, font: FontId, color: Color32, max_width: f32, margin: Margin) -> Response {
    let width = (ui.available_width() - margin.left as f32 - margin.right as f32).min(max_width).max(10.0);
    let galley = ui.painter().layout(text.to_owned(), font, color, width);
    let size = vec2(
        ui.available_width().max(galley.size().x + margin.left as f32),
        galley.size().y + margin.top as f32 + margin.bottom as f32,
    );
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().galley(rect.min + vec2(margin.left as f32, margin.top as f32), galley, color);
    response
}

/// Flat button in an exact rectangle: plate in the panel colour, grey border, red border on
/// hover. Primary = solid red.
pub fn button_at(ui: &mut Ui, rect: Rect, label: &str, primary: bool, enabled: bool, font: FontId) -> Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let response = ui.interact(rect, ui.scope_id().with(("button", label)), sense);
    paint_button(ui, rect, &response, label, primary, enabled, font);
    if enabled { response.on_hover_cursor(CursorIcon::PointingHand) } else { response }
}

fn paint_button(ui: &Ui, rect: Rect, response: &Response, label: &str, primary: bool, enabled: bool, font: FontId) {
    let hovered = enabled && response.hovered();
    let pressed = enabled && response.is_pointer_button_down_on();
    let (fill, border_color, fg) = if !enabled {
        // WinForms painted a disabled flat button on the panel plate with muted text.
        (panel_alt(), border(), muted())
    } else if primary {
        (if pressed { ACCENT_DIM } else if hovered { HOVER } else { SELECTED }, SELECTED, Color32::WHITE)
    } else {
        (if pressed { panel() } else { panel_alt() }, if hovered { accent() } else { border() }, text())
    };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::ZERO, fill);
    painter.rect_stroke(rect, CornerRadius::ZERO, Stroke::new(1.0, border_color), StrokeKind::Inside);
    let galley = painter.layout_no_wrap(label.to_owned(), font, fg);
    let pos = rect.center() - galley.size() / 2.0;
    painter.galley(pos, galley, fg);
}

/// What a title-bar glyph button shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Caption {
    Minimise,
    Close,
    /// The theme button shows where it goes: a moon in light mode, a sun in dark mode.
    Theme,
}

// Same visible caption characters and symbol font as Roch Viewer.
pub const GLYPH_MINIMISE: &str = "\u{2212}";
pub const GLYPH_CLOSE: &str = "\u{00D7}";
pub const GLYPH_SUN: &str = "\u{2600}";

/// Title-bar glyph button (minimise / close / theme): sits flat on the title bar until hovered,
/// then lifts to grey, or to red for close, the way Windows does it.
pub fn title_button(ui: &mut Ui, rect: Rect, caption: Caption) -> Response {
    let response = ui.interact(rect, ui.scope_id().with(("caption", caption as u8)), Sense::click());
    let hovered = response.hovered();
    let close = caption == Caption::Close;
    let fill = if hovered { if close { CLOSE_HOVER } else { title_hover() } } else { title_bar() };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::ZERO, fill);
    let fg = if hovered && close { Color32::WHITE } else { text() };
    match caption {
        // Draw the common filled crescent instead of the font's outlined moon glyph.
        Caption::Theme if !is_dark() => crate::ui::moon::paint(painter, rect, fg),
        Caption::Theme => {
            paint_text(ui, rect.center(), Align2::CENTER_CENTER, GLYPH_SUN, glyph(GLYPH_SIZE), fg, None);
        }
        Caption::Minimise => {
            paint_text(ui, rect.center(), Align2::CENTER_CENTER, GLYPH_MINIMISE, glyph(GLYPH_SIZE), fg, None);
        }
        Caption::Close => {
            paint_text(ui, rect.center(), Align2::CENTER_CENTER, GLYPH_CLOSE, glyph(CLOSE_GLYPH_SIZE), fg, None);
        }
    }
    response
}

/// The frame of a value box: panel plate with a 1 px grey border, red while focused.
pub fn value_frame(focused: bool) -> egui::Frame {
    egui::Frame::new()
        .fill(panel_alt())
        .stroke(Stroke::new(1.0, if focused { accent() } else { border() }))
        .inner_margin(Margin { left: 5, right: 5, top: 4, bottom: 2 })
        .corner_radius(CornerRadius::ZERO)
}

/// A typeable value in red on a panel plate with a 1 px grey border (the Roch GPU "ValueBox"),
/// right-aligned. `enabled = false` greys it out and takes it out of the tab order;
/// `read_only` keeps it selectable but not editable (the preview's `TextBox.ReadOnly`). The
/// whole text is selected when the box gains focus, as `tb.SelectAll()` did.
#[allow(clippy::too_many_arguments)]
pub fn value_box(ui: &mut Ui, rect: Rect, id: egui::Id, text: &mut String, enabled: bool, read_only: bool, color: Color32) -> Response {
    let focused = enabled && ui.memory(|m| m.has_focus(id));
    let mut make = |buffer: &mut dyn egui::TextBuffer| -> Response {
        let edit = egui::TextEdit::singleline(buffer)
            .id(id)
            .frame(value_frame(focused))
            .font(base())
            .text_color(color)
            .align(Align2::RIGHT_CENTER)
            .desired_width(f32::INFINITY)
            .min_size(rect.size())
            .clip_text(true)
            .interactive(enabled);
        ui.put(rect, edit)
    };
    let response = if read_only || !enabled {
        let mut view: &str = text.as_str();
        make(&mut view)
    } else {
        make(text)
    };
    if response.gained_focus() {
        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
            state.cursor.set_char_range(Some(egui::text::CCursorRange::select_all_str(text)));
            state.store(ui.ctx(), id);
        }
    }
    response
}

/// Flat radio button; the checked one reads in the brand red.
pub fn radio(ui: &mut Ui, label: &str, checked: bool, enabled: bool) -> Response {
    let galley = ui.painter().layout_no_wrap(label.to_owned(), base(), text());
    let size = vec2(16.0 + galley.size().x, galley.size().y.max(16.0));
    let (rect, response) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let fg = if !enabled { muted() } else if checked { SELECTED } else { text() };
    let center = egui::pos2(rect.min.x + 6.5, rect.center().y);
    let painter = ui.painter();
    painter.circle_stroke(center, 5.5, Stroke::new(1.0, if checked { SELECTED } else { fg }));
    if checked {
        painter.circle_filled(center, 3.0, if enabled { SELECTED } else { muted() });
    }
    let pos = egui::pos2(rect.min.x + 16.0, rect.center().y - galley.size().y / 2.0);
    painter.galley_with_override_text_color(pos, galley, fg);
    response
}

/// A section card: panel fill, 1 px border.
pub fn section_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(panel())
        .stroke(Stroke::new(1.0, border()))
        .inner_margin(Margin { left: 10, right: 10, top: 3, bottom: 4 })
        .corner_radius(CornerRadius::ZERO)
}

/// A 1 px horizontal rule across the available width.
pub fn rule(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::ZERO, border());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_decodes_rgb() {
        assert_eq!(hex(0xD0343A), Color32::from_rgb(0xD0, 0x34, 0x3A));
        assert_eq!(CLOSE_HOVER, Color32::from_rgb(0xC4, 0x2B, 0x1C));
    }
}
