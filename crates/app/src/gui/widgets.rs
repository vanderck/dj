//! Custom controls: knobs, faders, meters, lit buttons, waveforms.

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, Vec2};

use collective_library::Waveform;

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

pub fn dim(c: Color32, f: f32) -> Color32 {
    Color32::from_rgb((c.r() as f32 * f) as u8, (c.g() as f32 * f) as u8, (c.b() as f32 * f) as u8)
}

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

/// Vertical fader with an optional meter alongside.
pub fn vfader(ui: &mut Ui, value: &mut f32, height: f32, meter: Option<[f32; 2]>, color: Color32) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(34.0, height), Sense::click_and_drag());
    let track = Rect::from_center_size(Pos2::new(rect.left() + 10.0, rect.center().y), Vec2::new(4.0, height - 12.0));
    if resp.dragged() || resp.is_pointer_button_down_on() {
        if let Some(pos) = resp.interact_pointer_pos() {
            *value = (1.0 - (pos.y - track.top()) / track.height()).clamp(0.0, 1.0);
            resp.mark_changed();
        }
    }
    if resp.double_clicked() {
        *value = 1.0;
        resp.mark_changed();
    }
    let p = ui.painter();
    p.rect_filled(track, CornerRadius::same(2), ui.visuals().widgets.inactive.bg_fill);
    let y = track.bottom() - *value * track.height();
    let fill = Rect::from_min_max(Pos2::new(track.left(), y), track.right_bottom());
    p.rect_filled(fill, CornerRadius::same(2), dim(color, 0.6));
    let cap = Rect::from_center_size(Pos2::new(track.center().x, y), Vec2::new(18.0, 8.0));
    p.rect_filled(cap, CornerRadius::same(2), ui.visuals().text_color());
    if let Some(m) = meter {
        let mr = Rect::from_min_max(Pos2::new(rect.left() + 20.0, track.top()), Pos2::new(rect.right() - 2.0, track.bottom()));
        draw_meter(ui, mr, m);
    }
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

/// Horizontal value slider bar (used for crossfader and small params).
pub fn hslider(ui: &mut Ui, value: &mut f32, min: f32, max: f32, width: f32, color: Color32) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::click_and_drag());
    if resp.dragged() || resp.is_pointer_button_down_on() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let t = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            *value = min + t * (max - min);
            resp.mark_changed();
        }
    }
    let p = ui.painter();
    let track = Rect::from_center_size(rect.center(), Vec2::new(rect.width(), 4.0));
    p.rect_filled(track, CornerRadius::same(2), ui.visuals().widgets.inactive.bg_fill);
    let t = ((*value - min) / (max - min)).clamp(0.0, 1.0);
    let x = rect.left() + t * rect.width();
    p.rect_filled(Rect::from_center_size(Pos2::new(x, rect.center().y), Vec2::new(10.0, 16.0)), CornerRadius::same(2), color);
    resp
}

pub struct WaveView<'a> {
    pub wf: &'a Waveform,
    pub color: Color32,
}

fn band_color(base: Color32, lo: f32, mid: f32, hi: f32) -> Color32 {
    // Lows tint toward the deck colour, highs toward white.
    let s = (lo + mid + hi).max(1e-4);
    let (l, m, h) = (lo / s, mid / s, hi / s);
    let r = base.r() as f32 * l + 255.0 * h + 200.0 * m * 0.6;
    let g = base.g() as f32 * l + 255.0 * h + 120.0 * m;
    let b = base.b() as f32 * l + 255.0 * h + 60.0 * m;
    Color32::from_rgb(r.min(255.0) as u8, g.min(255.0) as u8, b.min(255.0) as u8)
}

impl WaveView<'_> {
    /// Scrolling waveform centred on `pos_secs`, showing `span_secs`.
    pub fn draw_scrolling(&self, ui: &Ui, rect: Rect, pos_secs: f64, span_secs: f64) {
        let p = ui.painter_at(rect);
        p.rect_filled(rect, CornerRadius::same(3), Color32::from_gray(14));
        let n = self.wf.len();
        if n == 0 {
            return;
        }
        let bps = self.wf.bins_per_sec;
        let cols = rect.width() as usize;
        let start = pos_secs - span_secs / 2.0;
        let per_col = span_secs / cols as f64;
        let mid = rect.center().y;
        let h = rect.height() / 2.0 - 2.0;
        for x in 0..cols {
            let t0 = start + x as f64 * per_col;
            let a = (t0 * bps) as i64;
            let b = ((t0 + per_col) * bps).ceil() as i64;
            if b <= 0 || a >= n as i64 {
                continue;
            }
            let (a, b) = (a.max(0) as usize, (b.max(a + 1) as usize).min(n));
            let mut lo = 0.0f32;
            let mut mi = 0.0f32;
            let mut hi = 0.0f32;
            for i in a..b {
                lo = lo.max(self.wf.low[i]);
                mi = mi.max(self.wf.mid[i]);
                hi = hi.max(self.wf.high[i]);
            }
            let amp = (lo + mi + hi).min(1.0);
            let played = t0 < pos_secs;
            let mut col = band_color(self.color, lo, mi, hi);
            if played {
                col = dim(col, 0.55);
            }
            let xx = rect.left() + x as f32;
            p.line_segment([Pos2::new(xx, mid - amp * h), Pos2::new(xx, mid + amp * h)], Stroke::new(1.0, col));
        }
    }

    pub fn draw_overview(&self, ui: &Ui, rect: Rect, progress: f32) {
        let p = ui.painter_at(rect);
        p.rect_filled(rect, CornerRadius::same(2), Color32::from_gray(14));
        let cols = rect.width().max(1.0) as usize;
        let ov = self.wf.overview(cols);
        let mid = rect.center().y;
        let h = rect.height() / 2.0 - 1.0;
        for (x, o) in ov.iter().enumerate() {
            let amp = (o[0] + o[1] + o[2]).min(1.0);
            let mut col = band_color(self.color, o[0], o[1], o[2]);
            if (x as f32) < progress * cols as f32 {
                col = dim(col, 0.45);
            }
            let xx = rect.left() + x as f32;
            p.line_segment([Pos2::new(xx, mid - amp * h), Pos2::new(xx, mid + amp * h)], Stroke::new(1.0, col));
        }
        let px = rect.left() + progress * rect.width();
        p.line_segment([Pos2::new(px, rect.top()), Pos2::new(px, rect.bottom())], Stroke::new(1.5, Color32::WHITE));
    }
}

pub fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0);
    format!("{}:{:04.1}", (s / 60.0) as u32, s % 60.0)
}

pub fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}
