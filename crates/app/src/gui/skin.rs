//! Visual language: near-black background, graphite cards, deck colours,
//! Inter + JetBrains Mono, rounded painter-drawn controls.

use std::sync::Arc;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Pos2, Rect, Response, Sense, Stroke, StrokeKind,
    Ui, Vec2,
};

pub const BG: Color32 = Color32::from_rgb(0x10, 0x10, 0x12);
pub const CARD: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x1e);
pub const CTRL: Color32 = Color32::from_rgb(0x26, 0x26, 0x2c);
pub const CTRL_HOVER: Color32 = Color32::from_rgb(0x30, 0x30, 0x37);
pub const WELL: Color32 = Color32::from_rgb(0x0b, 0x0b, 0x0d);
pub const BORDER: Color32 = Color32::from_rgb(0x2c, 0x2c, 0x33);
pub const TEXT: Color32 = Color32::from_rgb(0xee, 0xee, 0xf2);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8e, 0x8e, 0x98);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x58, 0x58, 0x60);
pub const LIGHT: Color32 = Color32::from_rgb(0xe4, 0xe4, 0xea);
pub const RED: Color32 = Color32::from_rgb(0xe5, 0x48, 0x4d);
pub const ORANGE: Color32 = Color32::from_rgb(0xe8, 0x9a, 0x2c);
pub const GREEN: Color32 = Color32::from_rgb(0x4a, 0xde, 0x80);

/// Deck colours: 1 red, 2 yellow, 3 blue, 4 purple.
pub const DECK: [Color32; 4] = [
    Color32::from_rgb(0xe5, 0x48, 0x4d),
    Color32::from_rgb(0xf2, 0xc8, 0x3c),
    Color32::from_rgb(0x38, 0x8e, 0xec),
    Color32::from_rgb(0x8b, 0x5c, 0xf6),
];

/// Hot-cue palette by pad number.
/// Stem colours (waveform lanes and the deck stem buttons): drums, bass, vocals, other.
pub const STEM: [Color32; 4] = [
    Color32::from_rgb(0xfb, 0xbf, 0x24),
    Color32::from_rgb(0xa7, 0x8b, 0xfa),
    Color32::from_rgb(0x22, 0xd3, 0xee),
    Color32::from_rgb(0x4a, 0xde, 0x80),
];

/// Mixed In Key energy level (1..=10) as a heat colour: dark grey (calm) to red (peak).
pub fn energy_color(level: u8) -> Color32 {
    let t = (level.clamp(1, 10) - 1) as f32 / 9.0;
    let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Color32::from_rgb(lerp(0x3a, 0xff), lerp(0x3a, 0x3b), lerp(0x44, 0x30))
}

pub const CUE: [Color32; 8] = [
    Color32::from_rgb(0xe5, 0x48, 0x4d),
    Color32::from_rgb(0xe8, 0x8a, 0x2c),
    Color32::from_rgb(0xe8, 0xc8, 0x4b),
    Color32::from_rgb(0xd8, 0xd0, 0xb8),
    Color32::from_rgb(0x4f, 0xb3, 0xe8),
    Color32::from_rgb(0x3a, 0x6f, 0xe0),
    Color32::from_rgb(0x8a, 0x63, 0xe6),
    Color32::from_rgb(0x5f, 0xd0, 0xb0),
];

pub fn install_fonts(ctx: &egui::Context) {
    let mut f = FontDefinitions::default();
    f.font_data.insert("inter".into(), Arc::new(FontData::from_static(include_bytes!("../../assets/fonts/Inter-Medium.otf"))));
    f.font_data.insert("inter-semibold".into(), Arc::new(FontData::from_static(include_bytes!("../../assets/fonts/Inter-SemiBold.otf"))));
    f.font_data.insert("jbmono".into(), Arc::new(FontData::from_static(include_bytes!("../../assets/fonts/JetBrainsMono-Medium.ttf"))));
    if let Some(v) = f.families.get_mut(&FontFamily::Proportional) {
        v.insert(0, "inter".into());
    }
    if let Some(v) = f.families.get_mut(&FontFamily::Monospace) {
        v.insert(0, "jbmono".into());
    }
    let mut bold = vec!["inter-semibold".to_string()];
    bold.extend(f.families.get(&FontFamily::Proportional).cloned().unwrap_or_default());
    f.families.insert(FontFamily::Name("bold".into()), bold);
    ctx.set_fonts(f);
}

pub fn sans(size: f32) -> FontId {
    FontId::proportional(size)
}

pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

pub fn dim(c: Color32, f: f32) -> Color32 {
    Color32::from_rgb((c.r() as f32 * f) as u8, (c.g() as f32 * f) as u8, (c.b() as f32 * f) as u8)
}

pub fn alpha(c: Color32, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a * 255.0) as u8)
}

pub fn lighten(c: Color32, f: f32) -> Color32 {
    let l = |x: u8| (x as f32 + (255.0 - x as f32) * f) as u8;
    Color32::from_rgb(l(c.r()), l(c.g()), l(c.b()))
}

/// Text colour that reads on top of `fill`.
pub fn on(fill: Color32) -> Color32 {
    let l = (0.299 * fill.r() as f32 + 0.587 * fill.g() as f32 + 0.114 * fill.b() as f32) / 255.0;
    if l > 0.55 {
        Color32::from_rgb(0x14, 0x14, 0x16)
    } else {
        Color32::WHITE
    }
}

pub fn card(ui: &Ui, rect: Rect) {
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(12), CARD);
    p.rect_stroke(rect, CornerRadius::same(12), Stroke::new(1.0, Color32::from_rgb(0x22, 0x22, 0x27)), StrokeKind::Inside);
}

#[derive(Clone, Copy, PartialEq)]
pub enum Style {
    /// Dark control.
    Dark,
    /// Filled with a colour.
    Fill(Color32),
    /// Dark fill with a coloured outline (and coloured text if `true`).
    Outline(Color32, bool),
    /// Light/white "on" state.
    Light,
    /// Dashed-looking empty placeholder.
    Empty,
}

/// Button labels drawn as the universal symbols (shapes, whatever the font has): a play arrow
/// and the two pause bars.
pub const PLAY: &str = "\u{25B6}";
pub const PAUSE: &str = "\u{23F8}";

/// The play arrow or the pause bars, `size` high, centred on `c`.
pub fn transport_icon(p: &egui::Painter, c: Pos2, size: f32, pause: bool, color: Color32) {
    let h = size;
    if pause {
        let (w, gap) = (0.3 * h, 0.26 * h);
        for x in [c.x - gap / 2.0 - w, c.x + gap / 2.0] {
            let r = Rect::from_min_size(Pos2::new(x, c.y - h / 2.0), Vec2::new(w, h));
            p.rect_filled(r, CornerRadius::same(((0.12 * h) as u8).max(1)), color);
        }
    } else {
        // (Shifted right a little: a triangle's weight sits left of its box's centre.)
        let (l, r) = (c.x - 0.36 * h, c.x + 0.5 * h);
        let pts = vec![Pos2::new(l, c.y - h / 2.0), Pos2::new(r, c.y), Pos2::new(l, c.y + h / 2.0)];
        p.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
    }
}

pub fn button(ui: &mut Ui, rect: Rect, label: &str, style: Style, font: FontId) -> Response {
    let r = ui.allocate_rect(rect, Sense::click_and_drag());
    let hover = r.hovered();
    let pressed = r.is_pointer_button_down_on();
    let radius = CornerRadius::same(((rect.height() * 0.22) as u8).clamp(5, 10));
    let p = ui.painter();
    let (fill, text, stroke) = match style {
        Style::Dark => (if hover { CTRL_HOVER } else { CTRL }, TEXT, Stroke::NONE),
        Style::Fill(c) => (if hover { lighten(c, 0.08) } else { c }, on(c), Stroke::NONE),
        Style::Outline(c, colored_text) => (
            if hover { CTRL_HOVER } else { Color32::from_rgb(0x1c, 0x1c, 0x21) },
            if colored_text { c } else { TEXT },
            Stroke::new(1.5, c),
        ),
        Style::Light => (if hover { Color32::WHITE } else { LIGHT }, Color32::from_rgb(0x14, 0x14, 0x16), Stroke::NONE),
        Style::Empty => (Color32::from_rgb(0x17, 0x17, 0x1b), TEXT_FAINT, Stroke::new(1.0, Color32::from_rgb(0x2a, 0x2a, 0x30))),
    };
    let fill = if pressed { lighten(fill, 0.12) } else { fill };
    p.rect_filled(rect, radius, fill);
    if stroke.width > 0.0 {
        p.rect_stroke(rect, radius, stroke, StrokeKind::Inside);
    }
    if label == PLAY || label == PAUSE {
        transport_icon(p, rect.center(), (font.size * 0.95).min(rect.height() * 0.5), label == PAUSE, text);
    } else if !label.is_empty() {
        p.text(rect.center(), Align2::CENTER_CENTER, label, font, text);
    }
    r
}

/// Rounded "pill" toggle (header / browser filters).
pub fn pill(ui: &mut Ui, rect: Rect, label: &str, on_: bool) -> Response {
    let r = ui.allocate_rect(rect, Sense::click());
    let p = ui.painter();
    let rad = CornerRadius::same((rect.height() / 2.0) as u8);
    let (fill, text) = if on_ { (LIGHT, Color32::from_rgb(0x14, 0x14, 0x16)) } else { (if r.hovered() { CTRL_HOVER } else { CTRL }, TEXT) };
    p.rect_filled(rect, rad, fill);
    p.text(rect.center(), Align2::CENTER_CENTER, label, bold(12.0), text);
    r
}

pub fn pill_width(ui: &Ui, label: &str, size: f32) -> f32 {
    ui.painter().layout_no_wrap(label.to_string(), bold(size), TEXT).size().x + 22.0
}

pub fn deck_badge(ui: &Ui, rect: Rect, d: usize, size: f32) {
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same((rect.height() * 0.18) as u8), DECK[d]);
    p.text(rect.center(), Align2::CENTER_CENTER, format!("{}", d + 1), bold(size), on(DECK[d]));
}

/// Rounded key chip ("6B") with a coloured border.
pub fn key_chip(ui: &Ui, rect: Rect, text: &str, border: Color32, size: f32) {
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(6), Color32::from_rgb(0x14, 0x14, 0x18));
    p.rect_stroke(rect, CornerRadius::same(6), Stroke::new(1.5, border), StrokeKind::Inside);
    p.text(rect.center(), Align2::CENTER_CENTER, text, mono(size), TEXT);
}

/// Horizontal mini slider used for Hi/Mid/Low/Filter. `value` 0..1, centre
/// default. Returns (response, label clicked).
pub fn mini_slider(ui: &mut Ui, rect: Rect, value: &mut f32, default: f32, label: &str, killed: bool, accent: Color32) -> (Response, bool) {
    let track = Rect::from_min_size(rect.min, Vec2::new(rect.width(), 16.0));
    let mut r = ui.allocate_rect(track, Sense::click_and_drag());
    if r.dragged() {
        let fine = ui.input(|i| i.modifiers.shift);
        *value = (*value + r.drag_delta().x / track.width() * if fine { 0.25 } else { 1.0 }).clamp(0.0, 1.0);
        r.mark_changed();
    } else if r.double_clicked() {
        *value = default;
        r.mark_changed();
    }
    let p = ui.painter();
    p.rect_filled(track, CornerRadius::same(6), WELL);
    let x = track.left() + 5.0 + *value * (track.width() - 10.0);
    let handle = Rect::from_center_size(Pos2::new(x, track.center().y), Vec2::new(8.0, 16.0));
    let col = if killed { RED } else if (*value - default).abs() > 0.01 { accent } else { Color32::from_gray(150) };
    p.rect_filled(handle, CornerRadius::same(3), col);
    let lr = Rect::from_min_size(Pos2::new(rect.left(), track.bottom() + 1.0), Vec2::new(rect.width(), 14.0));
    let lresp = ui.allocate_rect(lr, Sense::click());
    ui.painter().text(lr.center(), Align2::CENTER_CENTER, label, sans(11.0), if killed { RED } else { TEXT_DIM });
    (r, lresp.clicked())
}

pub fn set_visuals(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = WELL;
    v.faint_bg_color = Color32::from_gray(24);
    v.widgets.noninteractive.bg_fill = CARD;
    v.widgets.inactive.weak_bg_fill = CTRL;
    v.widgets.inactive.bg_fill = CTRL;
    v.widgets.hovered.weak_bg_fill = CTRL_HOVER;
    v.widgets.active.weak_bg_fill = dim(RED, 0.8);
    v.selection.bg_fill = dim(RED, 0.55);
    v.selection.stroke = Stroke::new(1.0, RED);
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = CornerRadius::same(12);
    v.override_text_color = Some(TEXT);
    // dj is dark whatever the system theme: pin it, and give both themes dj's style (else a Mac
    // in light mode gets egui's light windows and menus).
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, v.clone());
    ctx.set_visuals_of(egui::Theme::Light, v);
}

#[cfg(test)]
mod transport_tests {
    use super::*;

    /// What the button paints, flattened.
    fn painted(label: &str) -> Vec<egui::Shape> {
        let ctx = egui::Context::default();
        install_fonts(&ctx);
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let r = Rect::from_min_size(Pos2::new(10.0, 10.0), Vec2::new(120.0, 48.0));
            button(ui, r, label, Style::Outline(Color32::RED, false), bold(17.0));
        });
        out.textures_delta.clear(); // (no GPU here to upload them to)
        out.shapes.into_iter().map(|c| c.shape).collect()
    }

    #[test]
    fn play_and_pause_are_drawn_as_their_symbols() {
        let is_text = |s: &egui::Shape| matches!(s, egui::Shape::Text(_));
        let play = painted(PLAY);
        assert!(!play.iter().any(is_text), "no text, whatever the font");
        assert!(play.iter().any(|s| matches!(s, egui::Shape::Path(p) if p.points.len() == 3 && p.closed)), "a triangle");
        let pause = painted(PAUSE);
        assert!(!pause.iter().any(is_text));
        let bars = pause.iter().filter(|s| matches!(s, egui::Shape::Rect(r) if r.rect.height() < 30.0)).count();
        assert_eq!(bars, 2, "two bars");
        assert!(painted("Sync").iter().any(is_text), "other labels stay text");
    }
}
