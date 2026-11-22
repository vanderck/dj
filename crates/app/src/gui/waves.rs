//! Stacked, beat-aligned waveform lanes for all decks.

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{self, Align2, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, Ui, Vec2};

use dj_engine::engine::DeckMode;

use super::skin::*;
use super::{act_f, UiState};
use crate::core::Core;

/// Playhead position as a fraction of the lane width.
const PLAYHEAD: f32 = 0.35;

pub fn show(ui: &mut Ui, core: &mut Core, st: &mut UiState, rect: Rect) {
    let n = core.deck_count();
    let focused: Vec<usize> = st.focus.to_vec();
    // Focused (big) decks get tall lanes.
    // Equal lanes when every deck is shown in full (controller layout); otherwise the two big
    // decks get the large lanes.
    let weights: Vec<f32> = (0..n).map(|d| if n <= 2 || st.dense || focused.contains(&d) { 3.0 } else { 1.0 }).collect();
    let prog_h = 6.0;
    let gap = 6.0;
    let total_w: f32 = weights.iter().sum();
    let avail = rect.height() - (n as f32) * (prog_h + gap);
    let mut y = rect.top();
    for d in 0..n {
        let h = avail * weights[d] / total_w;
        let lane = Rect::from_min_size(Pos2::new(rect.left(), y), Vec2::new(rect.width(), h));
        let prog = Rect::from_min_size(Pos2::new(rect.left(), lane.bottom() + 2.0), Vec2::new(rect.width(), prog_h - 2.0));
        draw_lane(ui, core, st, d, lane, prog);
        y = prog.bottom() + gap;
    }
}

fn draw_lane(ui: &mut Ui, core: &mut Core, st: &mut UiState, d: usize, lane: Rect, prog: Rect) {
    let p = ui.painter_at(lane.union(prog));
    p.rect_filled(lane, CornerRadius::same(4), Color32::from_rgb(0x0c, 0x0c, 0x0e));
    let color = DECK[d];
    let v = core.snap.decks[d];
    let badge = Rect::from_min_size(lane.left_top() + Vec2::new(6.0, 6.0), Vec2::splat(if lane.height() > 50.0 { 22.0 } else { 18.0 }));

    // Drop target.
    let ctx = ui.ctx().clone();
    if egui::DragAndDrop::has_payload_of_type::<PathBuf>(&ctx) {
        if ctx.pointer_hover_pos().map(|p| lane.contains(p)).unwrap_or(false) {
            p.rect_stroke(lane, CornerRadius::same(4), Stroke::new(2.0, color), egui::StrokeKind::Inside);
            if ctx.input(|i| i.pointer.any_released()) {
                if let Some(path) = egui::DragAndDrop::take_payload::<PathBuf>(&ctx) {
                    core.request_load(d, &path);
                }
            }
        }
    }

    let info = core.decks[d].info.as_ref();
    if core.session.decks[d].mode == DeckMode::Sampler || info.is_none() {
        deck_badge(ui, badge, d, 12.0);
        let msg = if core.session.decks[d].mode == DeckMode::Sampler { "Sampler" } else { "" };
        p.text(Pos2::new(badge.right() + 10.0, badge.center().y), Align2::LEFT_CENTER, msg, sans(11.0), TEXT_FAINT);
        p.rect_filled(prog, CornerRadius::same(2), Color32::from_rgb(0x1e, 0x1e, 0x23));
        return;
    }
    let wf = info.unwrap().waveform.clone();
    let names: Vec<String> = info.unwrap().cues.iter().map(|c| c.as_ref().map(|c| c.name.clone()).unwrap_or_default()).collect();
    let zones = energy_zones(&v.hot_cues, &names, v.duration_secs);
    let stem_wave = core.decks[d].stem_wave.clone();
    let sections = core.decks[d].sections.clone();

    // Common scale: pixels per beat is the same for every deck, so synced
    // decks line up beat for beat.
    let beats_visible = 32.0 / st.lane_zoom as f64;
    let px_per_beat = lane.width() as f64 / beats_visible;
    let bpm = if v.has_grid && v.track_bpm > 0.0 { v.track_bpm } else { 120.0 };
    let px_per_sec = px_per_beat * bpm / 60.0;
    let play_x = lane.left() + lane.width() * PLAYHEAD;
    let t_at = |x: f32| v.position_secs + (x - play_x) as f64 / px_per_sec;
    let x_at = |t: f64| play_x + ((t - v.position_secs) * px_per_sec) as f32;

    // Energy zones (Mixed In Key "Energy N" cues): tint the lane, hotter = redder.
    for z in &zones {
        let (x0, x1) = (x_at(z.start).max(lane.left()), x_at(z.end).min(lane.right()));
        if x1 > x0 {
            let c = energy_color(z.level);
            let a = 0.05 + 0.02 * z.level as f32;
            p.rect_filled(Rect::from_min_max(Pos2::new(x0, lane.top()), Pos2::new(x1, lane.bottom())), CornerRadius::ZERO, alpha(c, a));
        }
    }

    // ...and a solid energy band along the bottom of the lane.
    let band_h = if lane.height() > 50.0 { 4.0 } else { 2.0 };
    for z in &zones {
        let (x0, x1) = (x_at(z.start).max(lane.left()), x_at(z.end).min(lane.right()));
        if x1 > x0 {
            p.rect_filled(Rect::from_min_max(Pos2::new(x0, lane.bottom() - band_h), Pos2::new(x1, lane.bottom())), CornerRadius::ZERO, energy_color(z.level));
        }
    }

    // Bar lines (behind the waveform).
    if v.has_grid {
        let spb = 60.0 / v.track_bpm;
        let anchor = v.position_secs - v.beat * spb;
        let b0 = ((t_at(lane.left()) - anchor) / spb).floor() as i64;
        let b1 = ((t_at(lane.right()) - anchor) / spb).ceil() as i64;
        for b in b0..=b1 {
            if b.rem_euclid(4) != 0 {
                continue;
            }
            let x = x_at(anchor + b as f64 * spb);
            p.line_segment([Pos2::new(x, lane.top()), Pos2::new(x, lane.bottom())], Stroke::new(1.0, Color32::from_rgb(0x2a, 0x2a, 0x30)));
        }
    }

    // Bars-style waveform, scaled to the track's own loud parts (they reach ~85 % of the lane,
    // the rest in proportion), so breaks, quiet intros and silence stand out.
    let ref_mix = loud_ref(ui.ctx(), (Arc::as_ptr(&wf) as usize, wf.len()), || wf.low.iter().zip(&wf.mid).zip(&wf.high).map(|((l, m), h)| mix_level(*l, *m, *h)).collect());
    let ref_stems = stem_wave.as_ref().map(|sw| {
        loud_ref(ui.ctx(), (Arc::as_ptr(sw) as usize, sw.levels[0].len()), || (0..sw.levels[0].len()).map(|i| (0..4).map(|k| sw.levels[k][i]).fold(0.0, f32::max)).collect())
    });
    let bar_w = if lane.height() > 50.0 { 3.0 } else { 2.0 };
    let step = bar_w + 1.0;
    let mid = lane.center().y;
    let hh = lane.height() / 2.0 - 3.0;
    let bps = wf.bins_per_sec;
    let nb = wf.len() as i64;
    let mut x = lane.left();
    while x < lane.right() {
        let t0 = t_at(x);
        let t1 = t_at(x + step);
        let (a, b) = ((t0 * bps) as i64, ((t1 * bps).ceil() as i64).max((t0 * bps) as i64 + 1));
        let played = x + bar_w < play_x;
        if let Some(sw) = stem_wave.as_deref() {
            // Separated: one bar per stem, loudest at the back so quieter stems stay visible.
            // Muted stems are grey, so the lane shows what is actually audible.
            let n = sw.levels[0].len() as i64;
            if b > 0 && a < n {
                let (a, b) = (a.max(0) as usize, b.min(n) as usize);
                let r = ref_stems.unwrap_or(1.0);
                let mut bars: [(f32, usize); 4] = std::array::from_fn(|k| (shape(sw.levels[k][a..b].iter().cloned().fold(0.0, f32::max), r), k));
                bars.sort_by(|p, q| q.0.total_cmp(&p.0));
                for (amp, k) in bars {
                    if amp <= 0.002 {
                        continue;
                    }
                    let h = (amp * hh).max(1.0);
                    let base = if v.stem_gains[k] > 0.5 { STEM[k] } else { Color32::from_gray(70) };
                    let c = if played { dim(base, 0.42) } else { base };
                    p.rect_filled(Rect::from_min_max(Pos2::new(x, mid - h), Pos2::new(x + bar_w, mid + h)), CornerRadius::ZERO, c);
                }
            }
            x += step;
            continue;
        }
        if b > 0 && a < nb {
            let (a, b) = (a.max(0) as usize, (b.min(nb)) as usize);
            let mut amp = 0.0f32;
            for i in a..b {
                amp = amp.max(mix_level(wf.low[i], wf.mid[i], wf.high[i]));
            }
            let amp = shape(amp, ref_mix);
            let h = (amp * hh).max(1.0);
            let c = if played { dim(color, 0.42) } else { color };
            p.rect_filled(Rect::from_min_max(Pos2::new(x, mid - h), Pos2::new(x + bar_w, mid + h)), CornerRadius::ZERO, c);
        }
        x += step;
    }

    // Loop region.
    if v.loop_on {
        let r = Rect::from_min_max(Pos2::new(x_at(v.loop_start_secs), lane.top()), Pos2::new(x_at(v.loop_end_secs), lane.bottom()));
        p.rect_filled(r, CornerRadius::ZERO, Color32::from_rgba_unmultiplied(255, 255, 255, 26));
        p.rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.0, Color32::from_white_alpha(90)), egui::StrokeKind::Inside);
    }

    // Cue flags.
    let small = lane.height() < 50.0;
    for (i, c) in v.hot_cues.iter().enumerate() {
        let Some(t) = c else { continue };
        let x = x_at(*t);
        if x < lane.left() - 80.0 || x > lane.right() {
            continue;
        }
        p.line_segment([Pos2::new(x, lane.top()), Pos2::new(x, lane.bottom())], Stroke::new(1.5, CUE[i]));
        let label = if names[i].is_empty() { format!("{}", i + 1) } else { format!("{} {}", i + 1, names[i]) };
        let font = bold(if small { 9.5 } else { 11.0 });
        let galley = p.layout_no_wrap(label, font, on(CUE[i]));
        let fr = Rect::from_min_size(Pos2::new(x, lane.top() + 2.0), galley.size() + Vec2::new(8.0, 2.0));
        p.rect_filled(fr, CornerRadius::same(3), CUE[i]);
        p.galley(fr.min + Vec2::new(4.0, 1.0), galley, on(CUE[i]));
    }

    // A planned or running mix on this deck: where it starts and ends (shaded), and where the
    // basses swap.
    if let Some(m) = core.automix.as_ref().filter(|m| m.plan.incoming == d || m.plan.outgoing == d) {
        let plan = &m.plan;
        let now = core.mix_beat();
        let spb = 60.0 / v.track_bpm.max(1.0);
        // Track time on this deck at mix beat `b`.
        let t_of = |b: f64| {
            if plan.outgoing == d {
                v.position_secs + (b - now) * spb
            } else {
                let entry = plan.entry_secs.unwrap_or(v.loop_start_secs);
                entry + (b - (plan.start_beat - plan.lead_in)) * spb
            }
        };
        let (b0, b1) = (plan.start_beat, plan.start_beat + plan.length_beats);
        let (x0, x1) = (x_at(t_of(b0)), x_at(t_of(b1)));
        if x1 > lane.left() && x0 < lane.right() {
            let r = Rect::from_min_max(Pos2::new(x0.max(lane.left()), lane.top()), Pos2::new(x1.min(lane.right()), lane.bottom()));
            p.rect_filled(r, CornerRadius::ZERO, Color32::from_rgba_unmultiplied(0x4a, 0xde, 0x80, 22));
            for x in [x0, x1] {
                p.line_segment([Pos2::new(x, lane.top()), Pos2::new(x, lane.bottom())], Stroke::new(1.0, GREEN));
            }
            let font = bold(if small { 9.0 } else { 10.0 });
            p.text(Pos2::new(x0 + 4.0, lane.bottom() - 2.0), Align2::LEFT_BOTTOM, if plan.outgoing == d { "mix out" } else { "mix in" }, font.clone(), GREEN);
            if let Some(b) = &plan.blend {
                let xs = x_at(t_of(b0 + b.swap * 4.0));
                p.line_segment([Pos2::new(xs, lane.top()), Pos2::new(xs, lane.bottom())], Stroke::new(2.0, GREEN));
                p.text(Pos2::new(xs + 4.0, lane.bottom() - 2.0), Align2::LEFT_BOTTOM, "bass swap", font, GREEN);
            }
        }
    }

    // Playhead.
    p.line_segment([Pos2::new(play_x, lane.top()), Pos2::new(play_x, lane.bottom())], Stroke::new(2.0, Color32::WHITE));
    deck_badge(ui, badge, d, if small { 11.0 } else { 13.0 });

    // Interaction: drag to scrub, wheel to zoom all lanes.
    let resp = ui.allocate_rect(lane, Sense::click_and_drag());
    if resp.dragged() && v.duration_secs > 0.0 {
        let dt = -resp.drag_delta().x as f64 / px_per_sec;
        act_f(core, &format!("Deck/{}/Seek", d + 1), ((v.position_secs + dt) / v.duration_secs) as f32);
    }
    if resp.hovered() {
        let s = ui.input(|i| i.smooth_scroll_delta.y);
        if s != 0.0 {
            st.lane_zoom = (st.lane_zoom * if s > 0.0 { 1.08 } else { 1.0 / 1.08 }).clamp(0.25, 4.0);
        }
    }

    // Progress strip with cue ticks; click to seek.
    let p = ui.painter();
    p.rect_filled(prog, CornerRadius::same(2), Color32::from_rgb(0x22, 0x22, 0x28));
    let dur = v.duration_secs.max(0.001);
    let frac = (v.position_secs / dur).clamp(0.0, 1.0) as f32;
    let px = |t: f64| prog.left() + (t / dur).clamp(0.0, 1.0) as f32 * prog.width();
    if zones.is_empty() {
        p.rect_filled(Rect::from_min_size(prog.min, Vec2::new(prog.width() * frac, prog.height())), CornerRadius::same(2), color);
    } else {
        // The whole track's energy profile; the played part is darkened.
        for z in &zones {
            p.rect_filled(Rect::from_min_max(Pos2::new(px(z.start), prog.top()), Pos2::new(px(z.end), prog.bottom())), CornerRadius::ZERO, energy_color(z.level));
        }
        p.rect_filled(Rect::from_min_size(prog.min, Vec2::new(prog.width() * frac, prog.height())), CornerRadius::ZERO, Color32::from_black_alpha(150));
        let hx = prog.left() + prog.width() * frac;
        p.rect_filled(Rect::from_center_size(Pos2::new(hx, prog.center().y), Vec2::new(2.0, prog.height() + 4.0)), CornerRadius::ZERO, color);
    }
    // Cue ticks in the same colours as the cue flags and pads.
    for (i, c) in v.hot_cues.iter().enumerate() {
        let Some(t) = c else { continue };
        p.rect_filled(Rect::from_center_size(Pos2::new(px(*t), prog.center().y), Vec2::new(3.0, prog.height() + 2.0)), CornerRadius::ZERO, CUE[i]);
    }
    // The song's sections: a notch where each starts; hover for its name.
    for s in sections.iter().flat_map(|s| s.iter()).skip(1) {
        p.rect_filled(Rect::from_center_size(Pos2::new(px(s.start), prog.center().y), Vec2::new(1.5, prog.height() + 2.0)), CornerRadius::ZERO, Color32::from_black_alpha(220));
    }
    let mut pr = ui.allocate_rect(prog.expand2(Vec2::new(0.0, 2.0)), Sense::click_and_drag());
    if let (Some(pos), Some(list)) = (pr.hover_pos(), sections.as_deref()) {
        let t = ((pos.x - prog.left()) / prog.width()).clamp(0.0, 1.0) as f64 * dur;
        if let Some(s) = list.iter().find(|s| s.start <= t && t < s.end) {
            let mmss = |x: f64| format!("{}:{:02}", (x / 60.0) as u32, (x % 60.0) as u32);
            pr = pr.on_hover_text(format!("{} {}–{}", s.label, mmss(s.start), mmss(s.end)));
        }
    }
    if pr.clicked() || pr.dragged() {
        if let Some(pos) = pr.interact_pointer_pos() {
            act_f(core, &format!("Deck/{}/WaveformSeek", d + 1), ((pos.x - prog.left()) / prog.width()).clamp(0.0, 1.0));
        }
    }
}

/// A stretch of the track at one Mixed In Key energy level.
struct Zone {
    start: f64,
    end: f64,
    level: u8,
}

/// Energy level from a cue name such as "Energy 6" (Mixed In Key's cue naming).
fn cue_energy(name: &str) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    let rest = &lower[lower.find("energy")? + "energy".len()..];
    let digits: String = rest.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u8>().ok().filter(|e| (1..=10).contains(e))
}

/// Zones between consecutive energy cues; the last one runs to the end of the track.
fn energy_zones(cues: &[Option<f64>], names: &[String], duration: f64) -> Vec<Zone> {
    let mut points: Vec<(f64, u8)> = cues
        .iter()
        .zip(names)
        .filter_map(|(t, n)| Some(((*t)?, cue_energy(n)?)))
        .collect();
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    points
        .iter()
        .enumerate()
        .map(|(i, &(start, level))| Zone { start, end: points.get(i + 1).map_or(duration, |p| p.0), level })
        .filter(|z| z.end > z.start)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mixed_in_key_energy_names() {
        assert_eq!(cue_energy("Energy 6"), Some(6));
        assert_eq!(cue_energy("energy10"), Some(10));
        assert_eq!(cue_energy("Drop - Energy 8 (MIK)"), Some(8));
        assert_eq!(cue_energy("Energy 0"), None);
        assert_eq!(cue_energy("Chorus"), None);
    }

    #[test]
    fn zones_follow_cue_order_and_end_at_the_next_cue() {
        let cues = [Some(20.0), Some(0.5), None, Some(56.0), Some(40.0)];
        let names = ["Energy 4", "Energy 2", "", "Energy 8", "Break"].map(String::from);
        let z = energy_zones(&cues, &names, 190.0);
        let got: Vec<(f64, f64, u8)> = z.iter().map(|z| (z.start, z.end, z.level)).collect();
        assert_eq!(got, vec![(0.5, 20.0, 2), (20.0, 56.0, 4), (56.0, 190.0, 8)]);
    }
}

/// One waveform bin's level from its bands.
fn mix_level(low: f32, mid: f32, high: f32) -> f32 {
    low * 0.9 + mid + high * 0.7
}

/// Displayed height (0..1 of the half lane) of `level`, for a track whose loud parts are at
/// `loud`: those reach 85 % and the rest is in proportion (a section at half the loudness shows
/// at half that), so quiet parts read as quiet and silence as flat.
fn shape(level: f32, loud: f32) -> f32 {
    (level / loud.max(1e-6)).clamp(0.0, 1.0) * 0.85
}

/// The loud level of a track's waveform (its 95th percentile, ignoring silence), worked out once
/// per waveform (`key`: its address and length) and kept in egui's memory.
fn loud_ref(ctx: &egui::Context, key: (usize, usize), levels: impl FnOnce() -> Vec<f32>) -> f32 {
    let id = egui::Id::unique(("wave-loud-ref", key));
    if let Some(r) = ctx.memory(|m| m.data.get_temp::<f32>(id)) {
        return r;
    }
    let mut v: Vec<f32> = levels().into_iter().filter(|x| *x > 1e-4).collect();
    let r = if v.is_empty() {
        1.0
    } else {
        let k = ((v.len() as f32 * 0.95) as usize).min(v.len() - 1);
        *v.select_nth_unstable_by(k, |a, b| a.total_cmp(b)).1
    };
    ctx.memory_mut(|m| m.data.insert_temp(id, r));
    r
}
