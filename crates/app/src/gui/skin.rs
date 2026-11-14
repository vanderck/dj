//! The performance skin: dark panels, red accents, painter-drawn controls.

use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, Vec2};

use collective_library::Key;

pub const BG: Color32 = Color32::from_rgb(0x0e, 0x0e, 0x0e);
pub const PANEL: Color32 = Color32::from_rgb(0x19, 0x19, 0x19);
pub const PANEL_HI: Color32 = Color32::from_rgb(0x21, 0x21, 0x21);
pub const BTN: Color32 = Color32::from_rgb(0x2a, 0x2a, 0x2a);
pub const BTN_HOVER: Color32 = Color32::from_rgb(0x36, 0x36, 0x36);
pub const BORDER: Color32 = Color32::from_rgb(0x30, 0x30, 0x30);
pub const TEXT: Color32 = Color32::from_rgb(0xde, 0xde, 0xde);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x86, 0x86, 0x86);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x55, 0x55, 0x55);
pub const RED: Color32 = Color32::from_rgb(0xe5, 0x38, 0x3b);
pub const SILVER: Color32 = Color32::from_rgb(0xc8, 0xc8, 0xc8);
pub const WAVE_BG: Color32 = Color32::from_rgb(0x0b, 0x0b, 0x0b);

/// Deck badge colours (1 red, 2 yellow, 3 blue, 4 purple).
pub const DECK: [Color32; 4] = [
    Color32::from_rgb(0xe5, 0x38, 0x3b),
    Color32::from_rgb(0xf2, 0xc8, 0x3c),
    Color32::from_rgb(0x3a, 0x8e, 0xe6),
    Color32::from_rgb(0x8a, 0x63, 0xe6),
];

/// Hot-cue palette by pad number.
pub const CUE: [Color32; 8] = [
    Color32::from_rgb(0xe5, 0x38, 0x3b),
    Color32::from_rgb(0xf0, 0x8c, 0x2e),
    Color32::from_rgb(0xf2, 0xc8, 0x4b),
    Color32::from_rgb(0xe8, 0xdc, 0xc0),
    Color32::from_rgb(0x4f, 0xb3, 0xe8),
    Color32::from_rgb(0x3a, 0x6f, 0xe0),
    Color32::from_rgb(0x8a, 0x63, 0xe6),
    Color32::from_rgb(0x5f, 0xd0, 0xb0),
];

pub fn font(size: f32) -> FontId {
    FontId::proportional(size)
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

pub fn dim(c: Color32, f: f32) -> Color32 {
    Color32::from_rgb((c.r() as f32 * f) as u8, (c.g() as f32 * f) as u8, (c.b() as f32 * f) as u8)
}

pub fn lighten(c: Color32, f: f32) -> Color32 {
    let l = |x: u8| (x as f32 + (255.0 - x as f32) * f) as u8;
    Color32::from_rgb(l(c.r()), l(c.g()), l(c.b()))
}

/// Colour for a key on the Camelot wheel (minor keys slightly deeper).
pub fn key_color(k: Key) -> Color32 {
    let n = k.camelot_number() as f32;
    let hue = ((n - 1.0) * 30.0 + 350.0) % 360.0;
    let (s, v) = if k.minor { (0.62, 0.92) } else { (0.5, 1.0) };
    hsv(hue, s, v)
}

pub fn hsv(h: f32, s: f32, v: f32) -> Color32 {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h / 60.0) as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    Color32::from_rgb(((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8)
}

#[derive(Clone, Copy, PartialEq)]
pub enum Look {
    Normal,
    /// Lit with a fill colour (dark text).
    On(Color32),
    /// Dim/disabled.
    Off,
}

/// Painter button at an exact rect.
pub fn btn(ui: &mut Ui, rect: Rect, label: &str, look: Look) -> Response {
    btn_sized(ui, rect, label, look, 11.0)
}

pub fn btn_sized(ui: &mut Ui, rect: Rect, label: &str, look: Look, size: f32) -> Response {
    let r = ui.allocate_rect(rect, Sense::click_and_drag());
    let hover = r.hovered();
    let (fill, text) = match look {
        Look::Normal => (if hover { BTN_HOVER } else { BTN }, TEXT),
        Look::On(c) => (if hover { lighten(c, 0.12) } else { c }, if luminance(c) > 0.55 { Color32::from_gray(20) } else { Color32::WHITE }),
        Look::Off => (PANEL_HI, TEXT_FAINT),
    };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(3), fill);
    p.text(rect.center(), Align2::CENTER_CENTER, label, font(size), text);
    r
}

fn luminance(c: Color32) -> f32 {
    (0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32) / 255.0
}

/// Section title with lines either side: "—— FILTER ——".
pub fn rule_title(ui: &Ui, rect: Rect, text: &str) {
    let p = ui.painter();
    let galley = p.layout_no_wrap(text.to_string(), font(10.0), TEXT_DIM);
    let w = galley.size().x;
    let c = rect.center();
    p.galley(Pos2::new(c.x - w / 2.0, c.y - galley.size().y / 2.0), galley, TEXT_DIM);
    let y = c.y;
    p.line_segment([Pos2::new(rect.left() + 6.0, y), Pos2::new(c.x - w / 2.0 - 6.0, y)], Stroke::new(1.0, BORDER));
    p.line_segment([Pos2::new(c.x + w / 2.0 + 6.0, y), Pos2::new(rect.right() - 6.0, y)], Stroke::new(1.0, BORDER));
}

/// Small label over a value, as in the deck header fields.
pub fn field(ui: &Ui, rect: Rect, label: &str, value: &str, color: Color32) {
    let p = ui.painter();
    p.text(Pos2::new(rect.left(), rect.top()), Align2::LEFT_TOP, label, font(7.5), TEXT_DIM);
    p.text(Pos2::new(rect.left(), rect.bottom()), Align2::LEFT_BOTTOM, value, mono(12.5), color);
}

fn arc(c: Pos2, r: f32, from: f32, to: f32) -> Vec<Pos2> {
    let n = ((to - from).abs() * 12.0).ceil().max(2.0) as usize;
    (0..=n).map(|i| c + Vec2::angled(from + (to - from) * i as f32 / n as f32) * r).collect()
}

const A0: f32 = std::f32::consts::PI * 0.75;
const SWEEP: f32 = std::f32::consts::PI * 1.5;

/// Interaction shared by the knobs: vertical drag (Shift fine), wheel,
/// double-click reset. Ctrl/Cmd-drag reports via `alt` instead of `value`.
pub fn knob_drag(ui: &Ui, r: &mut Response, value: &mut f32, default: f32, alt: Option<&mut f32>) {
    let mods = ui.input(|i| i.modifiers);
    if r.dragged() {
        let d = -r.drag_delta().y / if mods.shift { 700.0 } else { 160.0 };
        match alt {
            Some(a) if mods.command => *a = (*a + d).clamp(0.0, 1.0),
            _ => *value = (*value + d).clamp(0.0, 1.0),
        }
        r.mark_changed();
    } else if r.double_clicked() {
        *value = default;
        r.mark_changed();
    } else if r.hovered() {
        let s = ui.input(|i| i.smooth_scroll_delta.y);
        if s != 0.0 {
            *value = (*value + s / 1500.0).clamp(0.0, 1.0);
            r.mark_changed();
        }
    }
}

/// Dark knob with a red value arc (filter / EQ style). `value` is 0..1.
pub fn small_knob(ui: &mut Ui, center: Pos2, radius: f32, value: &mut f32, default: f32, bipolar: bool, color: Color32) -> Response {
    let rect = Rect::from_center_size(center, Vec2::splat(radius * 2.0 + 6.0));
    let mut r = ui.allocate_rect(rect, Sense::click_and_drag());
    knob_drag(ui, &mut r, value, default, None);
    let p = ui.painter();
    p.circle_filled(center, radius, Color32::from_gray(40));
    p.circle_stroke(center, radius, Stroke::new(1.0, Color32::from_gray(60)));
    let t = value.clamp(0.0, 1.0);
    let (from, to) = if bipolar { (0.5f32.min(t), 0.5f32.max(t)) } else { (0.0, t) };
    if to - from > 0.002 {
        p.add(Shape::line(arc(center, radius + 2.5, A0 + SWEEP * from, A0 + SWEEP * to), Stroke::new(2.5, color)));
    }
    let a = A0 + SWEEP * t;
    p.line_segment([center + Vec2::angled(a) * (radius * 0.35), center + Vec2::angled(a) * (radius - 2.0)], Stroke::new(2.0, TEXT));
    r
}

/// The big channel volume knob: dotted VU ring, trim ring, "F" key below.
/// Returns (volume response, trim changed, F clicked).
#[allow(clippy::too_many_arguments)]
pub fn volume_knob(
    ui: &mut Ui,
    center: Pos2,
    radius: f32,
    value: &mut f32,
    trim: &mut f32,
    vu: f32,
    fader_start: bool,
    color: Color32,
) -> (Response, bool, bool) {
    let rect = Rect::from_center_size(center, Vec2::splat(radius * 2.0));
    let mut r = ui.allocate_rect(rect, Sense::click_and_drag());
    let trim_before = *trim;
    knob_drag(ui, &mut r, value, 1.0, Some(trim));
    let p = ui.painter();
    // Dotted VU ring.
    let dots = 28;
    let lit = (vu.clamp(0.0, 1.0) * dots as f32).round() as usize;
    for i in 0..dots {
        let a = A0 + SWEEP * i as f32 / (dots - 1) as f32;
        let pos = center + Vec2::angled(a) * (radius + 7.0);
        let c = if i < lit {
            if i as f32 > dots as f32 * 0.9 {
                RED
            } else if i as f32 > dots as f32 * 0.75 {
                Color32::from_rgb(0xf0, 0x8c, 0x2e)
            } else {
                Color32::from_gray(190)
            }
        } else {
            Color32::from_gray(45)
        };
        p.circle_filled(pos, 1.3, c);
    }
    // Body.
    p.circle_filled(center, radius, Color32::from_gray(14));
    p.circle_filled(center, radius - 2.0, Color32::from_gray(44));
    p.circle_filled(center, radius - 6.0, Color32::from_gray(36));
    for i in 0..48 {
        let a = i as f32 / 48.0 * std::f32::consts::TAU;
        p.line_segment([center + Vec2::angled(a) * (radius - 5.0), center + Vec2::angled(a) * (radius - 2.5)], Stroke::new(1.0, Color32::from_gray(64)));
    }
    // Trim ring (inner): arc from centre.
    let tt = trim.clamp(0.0, 1.0);
    if (tt - 0.5).abs() > 0.01 {
        p.add(Shape::line(arc(center, radius - 7.5, A0 + SWEEP * 0.5f32.min(tt), A0 + SWEEP * 0.5f32.max(tt)), Stroke::new(2.0, color)));
    }
    let a = A0 + SWEEP * value.clamp(0.0, 1.0);
    p.line_segment([center + Vec2::angled(a) * (radius * 0.25), center + Vec2::angled(a) * (radius - 9.0)], Stroke::new(3.0, TEXT));
    // Fader-start key.
    let f_center = Pos2::new(center.x, center.y + radius + 3.0);
    let f_rect = Rect::from_center_size(f_center, Vec2::new(22.0, 14.0));
    let fr = ui.allocate_rect(f_rect, Sense::click());
    let p = ui.painter();
    p.rect_filled(f_rect, CornerRadius::same(7), if fader_start { RED } else { Color32::from_gray(26) });
    p.text(f_center, Align2::CENTER_CENTER, "F", font(9.0), if fader_start { Color32::WHITE } else { TEXT_DIM });
    let trim_changed = *trim != trim_before;
    (r, trim_changed, fr.clicked())
}

/// Horizontal dotted meter (header master / cue).
pub fn dot_meter(ui: &Ui, rect: Rect, peak: [f32; 2]) {
    let p = ui.painter();
    let n = 24;
    let w = rect.width() / n as f32;
    for ch in 0..2 {
        let db = 20.0 * peak[ch].max(1e-5).log10();
        let t = ((db + 48.0) / 51.0).clamp(0.0, 1.0);
        let lit = (t * n as f32).round() as usize;
        let y = rect.top() + rect.height() * (0.3 + 0.4 * ch as f32);
        for i in 0..n {
            let x = rect.left() + (i as f32 + 0.5) * w;
            let c = if i < lit {
                if i == n - 1 {
                    RED
                } else if i >= n - 5 {
                    Color32::from_rgb(0xf0, 0x8c, 0x2e)
                } else {
                    Color32::from_gray(180)
                }
            } else {
                Color32::from_gray(40)
            };
            p.rect_filled(Rect::from_center_size(Pos2::new(x, y), Vec2::new(w * 0.6, 2.0)), CornerRadius::ZERO, c);
        }
    }
}

/// Panel background with a hairline border.
pub fn panel(ui: &Ui, rect: Rect, fill: Color32) {
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(2), fill);
    p.rect_stroke(rect, CornerRadius::same(2), Stroke::new(1.0, Color32::from_gray(10)), StrokeKind::Inside);
}

pub fn play_icon(ui: &Ui, rect: Rect, color: Color32, playing: bool) {
    let p = ui.painter();
    let c = rect.center();
    let s = rect.height().min(rect.width()) * 0.32;
    if playing {
        for dx in [-s * 0.45, s * 0.45] {
            p.rect_filled(Rect::from_center_size(c + Vec2::new(dx, 0.0), Vec2::new(s * 0.45, s * 1.6)), CornerRadius::ZERO, color);
        }
    } else {
        p.add(Shape::convex_polygon(
            vec![c + Vec2::new(-s * 0.7, -s), c + Vec2::new(s, 0.0), c + Vec2::new(-s * 0.7, s)],
            color,
            Stroke::NONE,
        ));
    }
}

pub fn eject_icon(ui: &Ui, rect: Rect) {
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(3), RED);
    let c = rect.center();
    let s = rect.height() * 0.28;
    p.add(Shape::convex_polygon(
        vec![c + Vec2::new(-s, s * 0.2), c + Vec2::new(0.0, -s), c + Vec2::new(s, s * 0.2)],
        Color32::WHITE,
        Stroke::NONE,
    ));
    p.rect_filled(Rect::from_min_max(c + Vec2::new(-s, s * 0.45), c + Vec2::new(s, s * 0.8)), CornerRadius::ZERO, Color32::WHITE);
}

pub fn loop_icon(ui: &Ui, rect: Rect, color: Color32) {
    let p = ui.painter();
    let c = rect.center();
    let r = Rect::from_center_size(c, Vec2::new(14.0, 9.0));
    p.rect_stroke(r, CornerRadius::same(3), Stroke::new(1.6, color), StrokeKind::Middle);
    p.add(Shape::convex_polygon(
        vec![Pos2::new(r.right() - 3.0, r.top() - 3.0), Pos2::new(r.right() + 1.5, r.top()), Pos2::new(r.right() - 3.0, r.top() + 3.0)],
        color,
        Stroke::NONE,
    ));
}

pub fn set_visuals(ctx: &egui::Context, scale: f32) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = Color32::from_gray(6);
    v.faint_bg_color = Color32::from_gray(22);
    v.widgets.noninteractive.bg_fill = PANEL;
    v.widgets.inactive.weak_bg_fill = BTN;
    v.widgets.inactive.bg_fill = BTN;
    v.widgets.hovered.weak_bg_fill = BTN_HOVER;
    v.widgets.active.weak_bg_fill = dim(RED, 0.8);
    v.selection.bg_fill = dim(RED, 0.55);
    v.selection.stroke = Stroke::new(1.0, RED);
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.override_text_color = Some(TEXT);
    ctx.set_visuals(v);
    ctx.set_pixels_per_point(scale.clamp(0.5, 2.0));
}
