//! Custom controls: knobs, faders, meters, lit buttons, waveforms.

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, Vec2};


pub const ACCENT: Color32 = Color32::from_rgb(0x3d, 0xd6, 0xc6);
pub const WARN: Color32 = Color32::from_rgb(0xf2, 0x8c, 0x28);
pub const DANGER: Color32 = Color32::from_rgb(0xe6, 0x3b, 0x3b);
pub const OK: Color32 = Color32::from_rgb(0x5b, 0xc2, 0x36);
pub const DECK_COLORS: [Color32; 4] = [
    Color32::from_rgb(0x3b, 0x9b, 0xf2),
    Color32::from_rgb(0xf2, 0x6b, 0x3b),
    Color32::from_rgb(0x9b, 0x6b, 0xf2),
    Color32::from_rgb(0x3b, 0xd6, 0x8c),
];

/// A rotary knob. Vertical drag changes the value (Shift = fine);
/// double-click resets. Returns the response (changed() when moved).
pub fn knob(ui: &mut Ui, value: &mut f32, min: f32, max: f32, default: f32, label: &str, color: Color32) -> Response {
    let size = Vec2::new(40.0, 50.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    if resp.dragged() {
        let fine = ui.input(|i| i.modifiers.shift);
        let delta = -resp.drag_delta().y / if fine { 600.0 } else { 150.0 };
        *value = (*value + delta * (max - min)).clamp(min, max);
        resp.mark_changed();
    }
    if resp.double_clicked() {
        *value = default;
        resp.mark_changed();
    }
    if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            *value = (*value + scroll / 2000.0 * (max - min)).clamp(min, max);
            resp.mark_changed();
        }
    }
    let p = ui.painter();
    let c = Pos2::new(rect.center().x, rect.top() + 18.0);
    let r = 14.0;
    let t = ((*value - min) / (max - min)).clamp(0.0, 1.0);
    let a0 = std::f32::consts::PI * 0.75;
    let sweep = std::f32::consts::PI * 1.5;
    let arc = |from: f32, to: f32| -> Vec<Pos2> {
        let n = 24;
        (0..=n).map(|i| {
            let a = from + (to - from) * i as f32 / n as f32;
            c + Vec2::angled(a) * r
        }).collect()
    };
    let bg = ui.visuals().widgets.inactive.bg_fill;
    p.add(Shape::line(arc(a0, a0 + sweep), Stroke::new(3.0, bg)));
    // Bipolar knobs draw from the centre.
    let centre = (default - min) / (max - min);
    let (from, to) = if (centre - 0.5).abs() < 0.01 { (0.5f32.min(t), 0.5f32.max(t)) } else { (0.0, t) };
    if to > from {
        p.add(Shape::line(arc(a0 + sweep * from, a0 + sweep * to), Stroke::new(3.0, color)));
    }
    let a = a0 + sweep * t;
    p.line_segment([c, c + Vec2::angled(a) * (r - 3.0)], Stroke::new(2.0, ui.visuals().text_color()));
    p.text(
        Pos2::new(rect.center().x, rect.bottom() - 6.0),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(10.0),
        ui.visuals().weak_text_color(),
    );
    resp
}

/// Stereo peak meter (dB scale, −48..+3).
pub fn draw_meter(ui: &Ui, rect: Rect, peak: [f32; 2]) {
    let p = ui.painter();
    let w = (rect.width() - 2.0) / 2.0;
    for ch in 0..2 {
        let r = Rect::from_min_size(Pos2::new(rect.left() + ch as f32 * (w + 2.0), rect.top()), Vec2::new(w, rect.height()));
        p.rect_filled(r, CornerRadius::ZERO, Color32::from_gray(20));
        let db = 20.0 * peak[ch].max(1e-5).log10();
        let t = ((db + 48.0) / 51.0).clamp(0.0, 1.0);
        let top = r.bottom() - t * r.height();
        let col = if db > -0.5 {
            DANGER
        } else if db > -6.0 {
            WARN
        } else {
            OK
        };
        p.rect_filled(Rect::from_min_max(Pos2::new(r.left(), top), r.right_bottom()), CornerRadius::ZERO, col);
    }
}

/// A button that lights up when `on`.
pub fn lit(ui: &mut Ui, on: bool, text: &str, color: Color32) -> Response {
    let fill = if on { color } else { ui.visuals().widgets.inactive.weak_bg_fill };
    let txt = if on {
        egui::RichText::new(text).color(Color32::BLACK).strong()
    } else {
        egui::RichText::new(text)
    };
    ui.add(egui::Button::new(txt).fill(fill).min_size(Vec2::new(28.0, 22.0)))
}

pub fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0);
    format!("{}:{:04.1}", (s / 60.0) as u32, s % 60.0)
}

