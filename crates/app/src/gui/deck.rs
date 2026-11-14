//! Deck panel (track or sampler), drawn into a fixed rect.

use std::path::PathBuf;

use eframe::egui::{self, Align2, Color32, CornerRadius, Pos2, Rect, RichText, Sense, Stroke, Ui, UiBuilder, Vec2};

use collective_engine::deck::Quantize;
use collective_engine::engine::{ClockOwner, DeckMode};
use collective_engine::sampler::{Direction, Rhythm, Step, LANES, STEPS};
use collective_library::key::Compat;

use super::skin::*;
use super::{act, act_f, act_i, learnable, UiState};
use crate::core::Core;

const LEFT_W: f32 = 54.0;
const GAP: f32 = 4.0;

pub fn show(ui: &mut Ui, core: &mut Core, st: &mut UiState, d: usize, rect: Rect) {
    panel(ui, rect, PANEL);
    let inner = rect.shrink(5.0);
    accept_drop(ui, core, d, rect);
    if core.session.decks[d].mode == DeckMode::Sampler {
        sampler_deck(ui, core, d, inner);
    } else {
        track_deck(ui, core, st, d, inner);
    }
}

/// Library rows can be dragged onto a deck.
fn accept_drop(ui: &mut Ui, core: &mut Core, d: usize, rect: Rect) {
    let ctx = ui.ctx().clone();
    if !egui::DragAndDrop::has_payload_of_type::<PathBuf>(&ctx) {
        return;
    }
    let over = ctx.pointer_hover_pos().map(|p| rect.contains(p)).unwrap_or(false);
    if over {
        ui.painter().rect_stroke(rect, CornerRadius::same(3), Stroke::new(2.0, RED), egui::StrokeKind::Inside);
        if ctx.input(|i| i.pointer.any_released()) {
            if let Some(p) = egui::DragAndDrop::take_payload::<PathBuf>(&ctx) {
                if core.session.decks[d].mode == DeckMode::Sampler {
                    let lane = core.session.decks[d].sampler.selected_lane;
                    core.load_sample(d, lane, &p);
                } else {
                    core.load_track(d, &p);
                }
            }
        }
    }
}

fn badge(ui: &mut Ui, core: &mut Core, d: usize, rect: Rect) {
    let r = ui.allocate_rect(rect, Sense::click()).on_hover_text("Right-click: deck type, lock, grid");
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(2), DECK[d]);
    let tc = if d == 1 { Color32::from_gray(20) } else { Color32::WHITE };
    p.text(rect.center(), Align2::CENTER_CENTER, format!("{}", d + 1), font(15.0), tc);
    r.context_menu(|ui| {
        let mode = core.session.decks[d].mode;
        if ui.selectable_label(mode == DeckMode::Track, "Track deck").clicked() {
            core.set_deck_mode(d, DeckMode::Track);
            ui.close();
        }
        if ui.selectable_label(mode == DeckMode::Sampler, "Sampler deck").clicked() {
            core.set_deck_mode(d, DeckMode::Sampler);
            ui.close();
        }
        if let Some(info) = &core.decks[d].info {
            let locked = info.locked;
            ui.separator();
            if ui.button(if locked { "Unlock track" } else { "Lock track (cues not saved)" }).clicked() {
                core.set_track_locked(d, !locked);
                ui.close();
            }
            if ui.button("Grid start here").clicked() {
                core.grid_start_here(d);
                ui.close();
            }
            ui.horizontal(|ui| {
                ui.label("Nudge grid");
                if ui.button("−5 ms").clicked() {
                    core.nudge_grid(d, -5.0);
                }
                if ui.button("+5 ms").clicked() {
                    core.nudge_grid(d, 5.0);
                }
            });
        }
    });
}

fn quant_label(q: Quantize, global: Option<f64>) -> String {
    let beats = match q {
        Quantize::UseGlobal => global,
        Quantize::Off => None,
        Quantize::Beats(b) => Some(b),
    };
    match beats {
        None => "—".into(),
        Some(b) => {
            let bars = b / 4.0;
            if bars >= 1.0 {
                format!("{bars}")
            } else {
                format!("1/{}", (1.0 / bars).round())
            }
        }
    }
}

fn mmss(s: f64) -> String {
    let s = s.max(0.0);
    format!("{:02}:{:02}", (s / 60.0) as u32, (s % 60.0) as u32)
}

fn track_deck(ui: &mut Ui, core: &mut Core, st: &mut UiState, d: usize, r: Rect) {
    let v = core.snap.decks[d];
    let n = d + 1;
    let a = |s: &str| format!("Deck/{n}/{s}");
    let is_master = core.snap.clock.owner == ClockOwner::Deck(d as u8);
    let x0 = r.left();
    let xr = x0 + LEFT_W + GAP * 1.5;

    // ---------------------------------------------------------- row 1: header
    let h1 = 28.0;
    let y1 = r.top();
    let sync_r = Rect::from_min_size(Pos2::new(x0, y1), Vec2::new(LEFT_W, h1));
    let resp = btn(ui, sync_r, "SYNC", if v.sync { Look::On(SILVER) } else { Look::Normal });
    learnable(core, &resp, &a("TempoSync"));
    if resp.clicked() {
        act_i(core, &a("TempoSync"), 1);
    }
    let badge_r = Rect::from_min_size(Pos2::new(xr, y1 + 3.0), Vec2::new(24.0, 24.0));
    badge(ui, core, d, badge_r);

    // Fields from the right.
    let mut x = r.right();
    let eject_r = Rect::from_min_max(Pos2::new(x - 28.0, y1 + 1.0), Pos2::new(x, y1 + h1 - 1.0));
    let er = ui.allocate_rect(eject_r, Sense::click());
    eject_icon(ui, eject_r);
    if er.clicked() && v.loaded {
        act_i(core, &a("Eject"), 1);
    }
    x -= 28.0 + 8.0;
    let midi_r = Rect::from_min_max(Pos2::new(x - 22.0, y1), Pos2::new(x, y1 + h1));
    let midi_on = core.midi.activity.elapsed().as_millis() < 150;
    ui.painter().text(midi_r.left_top() + Vec2::new(0.0, 6.0), Align2::LEFT_TOP, "MIDI", font(7.5), if midi_on { TEXT } else { TEXT_DIM });
    ui.painter().text(midi_r.left_bottom() - Vec2::new(0.0, 4.0), Align2::LEFT_BOTTOM, "OSC", font(7.5), TEXT_DIM);
    x -= 22.0 + 6.0;

    let global_q = core.session.global_quantize();
    let q = core.session.decks[d].quantize;
    let widths = [36.0, 40.0, 56.0, 56.0, 32.0, 42.0];
    let total: f32 = widths.iter().sum::<f32>() + 5.0 * 6.0;
    let mut fx_ = x - total;
    let key = core.deck_key(d);
    let key_txt = core.key_text(key);
    let key_col = key.map(key_color).unwrap_or(TEXT_DIM);
    let b = v.beat.floor() as i64;
    let barbeat = if v.has_grid { format!("{}.{}", b.div_euclid(4) + 1, b.rem_euclid(4) + 1) } else { "—".into() };
    let fields: [(&str, String, Color32); 6] = [
        ("BAR / BEAT", barbeat, TEXT),
        ("REMAIN", if v.loaded { mmss(v.duration_secs - v.position_secs) } else { "--:--".into() }, if v.loaded && v.duration_secs - v.position_secs < 30.0 && v.playing { RED } else { TEXT_DIM }),
        ("ORIGINAL BPM", if v.has_grid { format!("{:.2}", v.track_bpm) } else { "—".into() }, TEXT_DIM),
        ("CURRENT BPM", if v.has_grid { format!("{:.2}", v.bpm) } else { "—".into() }, TEXT_DIM),
        ("KEY", if key_txt.is_empty() { "—".into() } else { key_txt }, key_col),
        ("QUANTIZE", quant_label(q, global_q), TEXT_DIM),
    ];
    for (i, (label, value, col)) in fields.iter().enumerate() {
        let fr = Rect::from_min_size(Pos2::new(fx_, y1 + 1.0), Vec2::new(widths[i], h1 - 2.0));
        field(ui, fr, label, value, *col);
        if *label == "KEY" {
            if let (Some(k), Some(rk)) = (key, core.reference_key()) {
                if !is_master {
                    let hint = match k.compat(rk) {
                        Compat::Same | Compat::Adjacent | Compat::Relative => "harmonic match with the master",
                        Compat::Boost => "+2 energy boost",
                        Compat::Diagonal => "diagonal",
                        Compat::Clash => "key clash with the master",
                    };
                    ui.allocate_rect(fr, Sense::hover()).on_hover_text(hint);
                }
            }
        }
        if *label == "QUANTIZE" {
            let qr = ui.allocate_rect(fr, Sense::click()).on_hover_text("Click: next value. Right-click: use global");
            if qr.clicked() {
                let idx = match q {
                    Quantize::UseGlobal => 0,
                    Quantize::Off => 1,
                    Quantize::Beats(b) => 2 + (8.0f64 / b).log2().round() as i32,
                };
                act_i(core, &a("Quantize"), (idx + 1) % 9);
            }
            if qr.secondary_clicked() {
                act_i(core, &a("Quantize"), 0);
            }
        }
        fx_ += widths[i] + 5.0;
    }
    // Title / artist.
    let title_r = Rect::from_min_max(Pos2::new(badge_r.right() + 7.0, y1), Pos2::new(x - total - 4.0, y1 + h1));
    let (title, sub) = match &core.decks[d].info {
        Some(i) => (
            i.title(),
            format!("{}  /  {}", i.tags.artist.clone().unwrap_or_default(), mmss(v.duration_secs)),
        ),
        None => match (&core.decks[d].loading, &core.decks[d].error) {
            (Some(p), _) => (format!("Loading {}…", p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()), String::new()),
            (None, Some(e)) => ("Could not load".into(), e.clone()),
            _ => ("Empty".into(), String::new()),
        },
    };
    let p = ui.painter_at(title_r);
    p.text(title_r.left_top() + Vec2::new(0.0, 3.0), Align2::LEFT_TOP, title, font(12.0), TEXT);
    p.text(title_r.left_bottom() - Vec2::new(0.0, 2.0), Align2::LEFT_BOTTOM, sub, font(9.5), TEXT_DIM);

    // ---------------------------------------------------------- row 2
    let h2 = 24.0;
    let y2 = y1 + h1 + GAP;
    let mr = Rect::from_min_size(Pos2::new(x0, y2), Vec2::new(LEFT_W, h2));
    let resp = btn(ui, mr, if is_master { "MASTER" } else { "FOLLOW" }, if is_master { Look::On(RED) } else { Look::Normal });
    learnable(core, &resp, &a("TempoMaster"));
    if resp.clicked() {
        act_i(core, &a("TempoMaster"), 1);
    }
    let mut cx = xr;
    let cell = 26.0;
    let pr = Rect::from_min_size(Pos2::new(cx, y2), Vec2::new(cell, h2));
    let resp = btn(ui, pr, "P", if v.keylock { Look::On(SILVER) } else { Look::Normal }).on_hover_text("Pitch preserve (key lock)");
    learnable(core, &resp, &a("PitchPreserve"));
    if resp.clicked() {
        act_i(core, &a("PitchPreserve"), !v.keylock as i32);
    }
    cx += cell + 2.0;
    let semis = if v.semitones == 0.0 { "0".to_string() } else { format!("{:+.0}", v.semitones) };
    for (lbl, addr) in [("-", "PitchDown"), (semis.as_str(), "PitchReset"), ("+", "PitchUp")] {
        let rr = Rect::from_min_size(Pos2::new(cx, y2), Vec2::new(cell, h2));
        let look = if addr == "PitchReset" && v.semitones != 0.0 { Look::On(SILVER) } else { Look::Normal };
        let resp = btn(ui, rr, lbl, look);
        learnable(core, &resp, &a(addr));
        if resp.clicked() {
            act_i(core, &a(addr), 1);
        }
        cx += cell + 2.0;
    }
    // Right group: M /2 X2 SLIP →
    let rg = ["M", "/2", "X2", "SLIP", "→"];
    let rw = [30.0, 30.0, 30.0, 40.0, 30.0];
    let rg_total: f32 = rw.iter().sum::<f32>() + 2.0 * 4.0;
    let mut rx = r.right() - rg_total;
    for (i, lbl) in rg.iter().enumerate() {
        let rr = Rect::from_min_size(Pos2::new(rx, y2), Vec2::new(rw[i], h2));
        let (on, addr, tip) = match *lbl {
            "M" => (v.momentary_cues, "CueMomentary", "Momentary hot cues (play while held)"),
            "/2" => (v.speed_mult == 0.5, "SpeedHalf", "Half speed"),
            "X2" => (v.speed_mult == 2.0, "SpeedDouble", "Double speed"),
            "SLIP" => (v.slip, "Slip", "Slip: the playhead keeps running underneath"),
            _ => (v.reverse, "Reverse", "Reverse (Shift-click: backspin)"),
        };
        let label = if *lbl == "→" && v.reverse { "←" } else { lbl };
        let resp = btn(ui, rr, label, if on { Look::On(SILVER) } else { Look::Normal }).on_hover_text(tip);
        learnable(core, &resp, &a(addr));
        if resp.clicked() {
            if *lbl == "→" && ui.input(|i| i.modifiers.shift) {
                act_i(core, &a("Backspin"), 1);
                act_i(core, &a("Backspin"), 0);
            } else {
                act_i(core, &a(addr), !on as i32);
            }
        }
        rx += rw[i] + 2.0;
    }
    // Bend (master) or EARLY/LATE phase strip (followers).
    let strip = Rect::from_min_max(Pos2::new(cx + 4.0, y2), Pos2::new(r.right() - rg_total - 6.0, y2 + h2));
    if is_master || !v.sync {
        let half = strip.width() / 2.0 - 1.0;
        for (i, (lbl, dir)) in [("< BEND", -1), ("BEND >", 1)].iter().enumerate() {
            let rr = Rect::from_min_size(Pos2::new(strip.left() + i as f32 * (half + 2.0), y2), Vec2::new(half, h2));
            let resp = btn(ui, rr, lbl, Look::Normal);
            let addr = if is_master { a("Bend") } else { a(if *dir < 0 { "PhaseMinus" } else { "PhasePlus" }) };
            learnable(core, &resp, &addr);
            if core.learn {
                continue;
            }
            if resp.is_pointer_button_down_on() {
                if is_master {
                    act_i(core, &addr, *dir);
                } else {
                    act_i(core, &addr, 1);
                }
            } else if resp.drag_stopped() || resp.clicked() {
                act_i(core, &addr, 0);
            }
        }
    } else {
        let resp = ui.allocate_rect(strip, Sense::click_and_drag());
        let p = ui.painter();
        p.rect_filled(strip, CornerRadius::same(3), BTN);
        p.text(strip.left_center() + Vec2::new(8.0, 0.0), Align2::LEFT_CENTER, "EARLY", font(9.5), TEXT);
        p.text(strip.right_center() - Vec2::new(8.0, 0.0), Align2::RIGHT_CENTER, "LATE", font(9.5), TEXT);
        // Marker: phase offset (±0.5 beat across the strip) plus live error.
        let off = (v.phase_offset + v.phase_error).clamp(-0.5, 0.5) as f32;
        let mx = strip.center().x + off * (strip.width() * 0.8);
        p.add(egui::Shape::convex_polygon(
            vec![Pos2::new(mx - 5.0, strip.top() + 8.0), Pos2::new(mx + 5.0, strip.top() + 8.0), Pos2::new(mx, strip.top() + 15.0)],
            TEXT,
            Stroke::NONE,
        ));
        let resp = resp.on_hover_text("Hold left: early, right: late. Double-click: reset phase offset");
        learnable(core, &resp, &a("PhaseBend"));
        if !core.learn {
            if resp.double_clicked() {
                act_i(core, &a("PhaseReset"), 1);
            } else if resp.is_pointer_button_down_on() {
                if let Some(pos) = resp.interact_pointer_pos() {
                    act_i(core, &a("PhaseBend"), if pos.x < strip.center().x { -1 } else { 1 });
                }
            } else if resp.drag_stopped() || resp.clicked() {
                act_i(core, &a("PhaseBend"), 0);
            }
        }
    }

    // ---------------------------------------------------------- body
    let h_bottom = 24.0;
    let yb = r.bottom() - h_bottom;
    let y3 = y2 + h2 + GAP;
    // Play button.
    let play_r = Rect::from_min_size(Pos2::new(x0, yb), Vec2::new(LEFT_W, h_bottom));
    let pr = ui.allocate_rect(play_r, Sense::click());
    ui.painter().rect_filled(play_r, CornerRadius::same(3), if pr.hovered() { BTN_HOVER } else { BTN });
    let blink = v.play_pending && (ui.input(|i| i.time) * 4.0) as i64 % 2 == 0;
    play_icon(ui, play_r, if blink { TEXT_DIM } else { RED }, v.playing);
    learnable(core, &pr, &a("PlayPause"));
    if pr.clicked() {
        act_i(core, &a("PlayPause"), 1);
    }
    // Tempo fader.
    let tf = Rect::from_min_max(Pos2::new(x0, y3), Pos2::new(x0 + LEFT_W, yb - GAP));
    tempo_fader(ui, core, d, tf);

    // Waveforms, pads, bottom row.
    let pads_h = 28.0;
    // Stems strip (only when separation is enabled).
    let stems_h = if core.settings.stems_enabled { 20.0 + GAP } else { 0.0 };
    let ov_h = ((yb - GAP - pads_h - GAP - stems_h - y3) * 0.42).clamp(24.0, 60.0);
    let zoom_h = yb - GAP - pads_h - GAP - stems_h - ov_h - GAP - y3;
    let zoom_r = Rect::from_min_size(Pos2::new(xr, y3), Vec2::new(r.right() - xr, zoom_h));
    let ov_r = Rect::from_min_size(Pos2::new(xr, zoom_r.bottom() + GAP), Vec2::new(r.right() - xr, ov_h));
    let stems_r = Rect::from_min_size(Pos2::new(xr, ov_r.bottom() + GAP), Vec2::new(r.right() - xr, (stems_h - GAP).max(0.0)));
    let pads_r = Rect::from_min_size(Pos2::new(xr, ov_r.bottom() + GAP + stems_h), Vec2::new(r.right() - xr, pads_h));
    waveforms(ui, core, d, zoom_r, ov_r);
    if stems_h > 0.0 {
        stems_row(ui, core, d, stems_r);
    }
    pads(ui, core, d, pads_r);
    bottom_row(ui, core, d, Rect::from_min_max(Pos2::new(xr, yb), Pos2::new(r.right(), r.bottom())));
    let _ = st;
}

fn tempo_fader(ui: &mut Ui, core: &mut Core, d: usize, rect: Rect) {
    let v = core.snap.decks[d];
    let a = format!("Deck/{}/TempoSlider", d + 1);
    let follower = v.sync && core.snap.clock.owner != ClockOwner::Deck(d as u8);
    let resp = ui.allocate_rect(rect, Sense::click_and_drag());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(3), PANEL_HI);
    let track = Rect::from_center_size(rect.center(), Vec2::new(4.0, rect.height() - 22.0));
    p.rect_filled(track, CornerRadius::same(2), Color32::from_gray(10));
    p.line_segment([Pos2::new(track.left() - 10.0, track.center().y), Pos2::new(track.right() + 10.0, track.center().y)], Stroke::new(1.0, TEXT_FAINT));
    let t = ((v.tempo_fader as f32) + 1.0) / 2.0;
    let y = track.top() + t * track.height();
    let cap = Rect::from_center_size(Pos2::new(track.center().x, y), Vec2::new(LEFT_W - 18.0, 9.0));
    let native = v.tempo_fader.abs() < 1e-4;
    p.rect_filled(cap, CornerRadius::same(2), if follower { Color32::from_gray(70) } else { Color32::from_gray(150) });
    p.line_segment([cap.left_center(), cap.right_center()], Stroke::new(1.5, if native { RED } else { Color32::from_gray(40) }));
    let resp = resp.on_hover_text(format!("Tempo {:+.2}% · {}{}", (v.speed - 1.0) * 100.0, v.tempo_range.label(), if follower { " · following the clock" } else { "" }));
    learnable(core, &resp, &a);
    if core.learn || follower {
        return;
    }
    if resp.double_clicked() {
        act_f(core, &a, 0.0);
    } else if resp.dragged() {
        let fine = ui.input(|i| i.modifiers.shift);
        let dv = resp.drag_delta().y / track.height() * 2.0 * if fine { 0.1 } else { 1.0 };
        act_f(core, &a, (v.tempo_fader as f32 + dv).clamp(-1.0, 1.0));
    }
}

/// Cue regions: (start secs, colour) sorted, for section colouring.
fn regions(core: &Core, d: usize) -> Vec<(f64, Color32)> {
    let v = core.snap.decks[d];
    let mut r: Vec<(f64, Color32)> = v.hot_cues.iter().enumerate().filter_map(|(i, c)| c.map(|t| (t, CUE[i]))).collect();
    r.sort_by(|a, b| a.0.total_cmp(&b.0));
    r
}

fn region_color(regions: &[(f64, Color32)], t: f64) -> Color32 {
    let mut c = Color32::from_gray(120);
    for (s, col) in regions {
        if t >= *s {
            c = *col;
        } else {
            break;
        }
    }
    c
}

fn waveforms(ui: &mut Ui, core: &mut Core, d: usize, zoom_r: Rect, ov_r: Rect) {
    let v = core.snap.decks[d];
    let n = d + 1;
    let p = ui.painter();
    p.rect_filled(zoom_r, CornerRadius::same(2), WAVE_BG);
    p.rect_filled(ov_r, CornerRadius::same(2), WAVE_BG);
    let Some(info) = &core.decks[d].info else {
        p.text(zoom_r.center(), Align2::CENTER_CENTER, format!("DROP TRACK ON DECK {n}"), font(11.0), TEXT_FAINT);
        return;
    };
    let wf = info.waveform.clone();
    let regs = regions(core, d);
    let zoom = core.session.decks[d].zoom as f64;
    let span = 10.0 / zoom;
    let pos = v.position_secs;
    let bps = wf.bins_per_sec;
    let nb = wf.len();

    // ---- zoomed
    let pz = ui.painter_at(zoom_r);
    let cols = zoom_r.width() as usize;
    let start = pos - span / 2.0;
    let per = span / cols as f64;
    let mid = zoom_r.center().y;
    let hh = zoom_r.height() / 2.0 - 2.0;
    for x in 0..cols {
        let t0 = start + x as f64 * per;
        let a0 = (t0 * bps) as i64;
        let b0 = ((t0 + per) * bps).ceil() as i64;
        if b0 <= 0 || a0 >= nb as i64 {
            continue;
        }
        let (a0, b0) = (a0.max(0) as usize, (b0.max(a0 + 1) as usize).min(nb));
        let (mut lo, mut mi, mut hi) = (0.0f32, 0.0f32, 0.0f32);
        for i in a0..b0 {
            lo = lo.max(wf.low[i]);
            mi = mi.max(wf.mid[i]);
            hi = hi.max(wf.high[i]);
        }
        let amp = (lo * 0.9 + mi + hi * 0.8).min(1.0);
        let base = region_color(&regs, t0);
        let col = if t0 < pos { dim(base, 0.5) } else { lighten(base, (hi * 0.5).min(0.35)) };
        let xx = zoom_r.left() + x as f32;
        pz.line_segment([Pos2::new(xx, mid - amp * hh), Pos2::new(xx, mid + amp * hh)], Stroke::new(1.0, col));
    }
    let x_of = |t: f64| zoom_r.left() + ((t - start) / span) as f32 * zoom_r.width();
    if v.has_grid && v.track_bpm > 0.0 {
        let spb = 60.0 / v.track_bpm;
        let anchor = v.position_secs - v.beat * spb;
        let first = ((start - anchor) / spb).floor() as i64;
        let last = ((start + span - anchor) / spb).ceil() as i64;
        for b in first..=last {
            let xx = x_of(anchor + b as f64 * spb);
            let bar = b.rem_euclid(4) == 0;
            let c = if bar { Color32::from_white_alpha(70) } else { Color32::from_white_alpha(22) };
            pz.line_segment([Pos2::new(xx, zoom_r.top()), Pos2::new(xx, zoom_r.bottom())], Stroke::new(1.0, c));
        }
    }
    if v.loop_on {
        let lr = Rect::from_min_max(Pos2::new(x_of(v.loop_start_secs), zoom_r.top()), Pos2::new(x_of(v.loop_end_secs), zoom_r.bottom()));
        pz.rect_filled(lr, CornerRadius::ZERO, Color32::from_rgba_unmultiplied(229, 56, 59, 38));
    }
    for (i, c) in v.hot_cues.iter().enumerate() {
        if let Some(t) = c {
            let xx = x_of(*t);
            pz.line_segment([Pos2::new(xx, zoom_r.top()), Pos2::new(xx, zoom_r.bottom())], Stroke::new(1.5, CUE[i]));
            let flag = Rect::from_min_size(Pos2::new(xx, zoom_r.top()), Vec2::new(12.0, 12.0));
            pz.rect_filled(flag, CornerRadius::ZERO, CUE[i]);
            pz.text(flag.center(), Align2::CENTER_CENTER, format!("{}", i + 1), font(8.5), Color32::WHITE);
        }
    }
    pz.line_segment([Pos2::new(zoom_r.center().x, zoom_r.top()), Pos2::new(zoom_r.center().x, zoom_r.bottom())], Stroke::new(2.0, Color32::WHITE));

    let zr = ui.allocate_rect(zoom_r, Sense::click_and_drag());
    if zr.dragged() && v.duration_secs > 0.0 {
        let dt = -zr.drag_delta().x as f64 / zoom_r.width() as f64 * span;
        act_f(core, &format!("Deck/{n}/Seek"), ((v.position_secs + dt) / v.duration_secs) as f32);
    }
    if zr.hovered() {
        let s = ui.input(|i| i.smooth_scroll_delta.y);
        if s != 0.0 {
            act_i(core, &format!("Deck/{n}/WaveformZoom"), if s > 0.0 { 1 } else { -1 });
        }
    }

    // ---- overview, coloured by cue section
    let po = ui.painter_at(ov_r);
    let ocols = ov_r.width().max(1.0) as usize;
    let ov = wf.overview(ocols);
    let omid = ov_r.center().y;
    let oh = ov_r.height() / 2.0 - 1.0;
    let dur = v.duration_secs.max(0.001);
    let prog = (v.position_secs / dur) as f32;
    for (x, o) in ov.iter().enumerate() {
        let t = x as f64 / ocols as f64 * dur;
        let amp = (o[0] * 0.9 + o[1] + o[2] * 0.8).min(1.0);
        let base = region_color(&regs, t);
        let col = if (x as f32) < prog * ocols as f32 { dim(base, 0.45) } else { base };
        let xx = ov_r.left() + x as f32;
        po.line_segment([Pos2::new(xx, omid - amp * oh), Pos2::new(xx, omid + amp * oh)], Stroke::new(1.0, col));
    }
    for (s, c) in &regs {
        let xx = ov_r.left() + (*s / dur) as f32 * ov_r.width();
        po.line_segment([Pos2::new(xx, ov_r.top()), Pos2::new(xx, ov_r.bottom())], Stroke::new(2.0, *c));
    }
    if v.loop_on {
        let x0 = ov_r.left() + (v.loop_start_secs / dur) as f32 * ov_r.width();
        let x1 = ov_r.left() + (v.loop_end_secs / dur) as f32 * ov_r.width();
        po.rect_filled(Rect::from_min_max(Pos2::new(x0, ov_r.top()), Pos2::new(x1.max(x0 + 1.0), ov_r.bottom())), CornerRadius::ZERO, Color32::from_rgba_unmultiplied(255, 255, 255, 30));
    }
    let px = ov_r.left() + prog * ov_r.width();
    po.line_segment([Pos2::new(px, ov_r.top()), Pos2::new(px, ov_r.bottom())], Stroke::new(1.5, Color32::WHITE));
    let or = ui.allocate_rect(ov_r, Sense::click_and_drag());
    if or.clicked() || or.dragged() {
        if let Some(p) = or.interact_pointer_pos() {
            act_f(core, &format!("Deck/{n}/WaveformSeek"), ((p.x - ov_r.left()) / ov_r.width()).clamp(0.0, 1.0));
        }
    }
}

/// Stem toggles (lit = audible), acapella / instrumental shortcuts, and separation status.
fn stems_row(ui: &mut Ui, core: &mut Core, d: usize, rect: Rect) {
    let v = core.snap.decks[d];
    let n = d + 1;
    let a = |s: &str| format!("Deck/{n}/{s}");
    let ready = v.stems_ready;
    let items: [(&str, &str, f32); 6] = [
        ("DRUMS", "StemDrums", 1.0),
        ("BASS", "StemBass", 1.0),
        ("VOCALS", "StemVocals", 1.0),
        ("OTHER", "StemOther", 1.0),
        ("ACAPELLA", "Acapella", 1.25),
        ("INSTR", "Instrumental", 1.0),
    ];
    let status_w = (rect.width() * 0.24).clamp(70.0, 150.0);
    let avail = rect.width() - status_w - GAP - 5.0 * 2.0;
    let unit = avail / items.iter().map(|i| i.2).sum::<f32>();
    let mut x = rect.left();
    for (i, (label, addr, weight)) in items.iter().enumerate() {
        let w = unit * weight;
        let br = Rect::from_min_size(Pos2::new(x, rect.top()), Vec2::new(w, rect.height()));
        x += w + 2.0;
        let on = match i {
            0..=3 => v.stem_gains[i] > 0.5,
            4 => v.stem_gains == crate::core::STEMS_ACAPELLA,
            _ => v.stem_gains == crate::core::STEMS_INSTRUMENTAL,
        };
        // Stems lit while audible; the shortcuts lit while active. Dim until stems are ready.
        let look = match (on, ready) {
            (true, true) => Look::On(if i < 4 { SILVER } else { RED }),
            (true, false) => Look::On(TEXT_FAINT),
            _ => Look::Normal,
        };
        let tip = if ready {
            "Toggle (lit = audible)"
        } else {
            "Stems are still being separated; the setting applies as soon as they are ready"
        };
        let resp = btn_sized(ui, br, label, look, 8.5).on_hover_text(tip);
        learnable(core, &resp, &a(addr));
        if resp.clicked() && v.loaded {
            act_i(core, &a(addr), !on as i32);
        }
    }

    // Status: progress while separating, then the source of the stems.
    let sr = Rect::from_min_max(Pos2::new(rect.right() - status_w, rect.top()), rect.right_bottom());
    let p = ui.painter_at(sr);
    p.rect_filled(sr, CornerRadius::same(3), WAVE_BG);
    let (text, col) = match (&core.decks[d].stems_progress, &core.decks[d].stems_source, v.loaded) {
        (Some(f), _, true) => {
            let fill = Rect::from_min_size(sr.min, Vec2::new(sr.width() * f.clamp(0.0, 1.0), sr.height()));
            p.rect_filled(fill, CornerRadius::same(3), BTN_HOVER);
            (format!("separating {:.0}%", f * 100.0), TEXT)
        }
        (None, Some(src), true) if ready => {
            let placeholder = src.contains("placeholder");
            (src.clone(), if placeholder { RED } else { TEXT_DIM })
        }
        (None, _, true) => ("no stems".into(), TEXT_FAINT),
        _ => (String::new(), TEXT_FAINT),
    };
    p.text(sr.center(), Align2::CENTER_CENTER, text, font(8.5), col);
    if core.decks[d].stems_source.as_deref().is_some_and(|s| s.contains("placeholder")) {
        ui.allocate_rect(sr, Sense::hover())
            .on_hover_text("No separation model is configured, so a low-quality placeholder is used. Set one in Settings → Stems.");
    }
}

fn pads(ui: &mut Ui, core: &mut Core, d: usize, rect: Rect) {
    let v = core.snap.decks[d];
    let n = d + 1;
    let w = (rect.width() - 7.0 * 3.0) / 8.0;
    for i in 0..8 {
        let pr = Rect::from_min_size(Pos2::new(rect.left() + i as f32 * (w + 3.0), rect.top()), Vec2::new(w, rect.height()));
        let resp = ui.allocate_rect(pr, Sense::click_and_drag());
        let set = v.hot_cues[i].is_some();
        let name = core.decks[d].info.as_ref().and_then(|x| x.cues[i].as_ref()).map(|c| c.name.clone()).unwrap_or_default();
        let p = ui.painter();
        p.rect_filled(pr, CornerRadius::same(3), if resp.hovered() { BTN_HOVER } else { BTN });
        p.text(pr.right_top() + Vec2::new(-5.0, 3.0), Align2::RIGHT_TOP, format!("{}", i + 1), font(8.5), if set { TEXT } else { TEXT_FAINT });
        if set {
            let label = if !name.is_empty() {
                name
            } else if v.hot_cue_is_loop[i] {
                "Loop".into()
            } else {
                format!("Cue {}", i + 1)
            };
            p.text(pr.center() + Vec2::new(0.0, 1.0), Align2::CENTER_CENTER, label, font(9.5), TEXT);
            p.rect_filled(
                Rect::from_min_max(Pos2::new(pr.left() + 8.0, pr.bottom() - 4.0), Pos2::new(pr.right() - 8.0, pr.bottom() - 2.0)),
                CornerRadius::ZERO,
                CUE[i],
            );
            if v.hot_cue_is_loop[i] {
                p.text(pr.left_top() + Vec2::new(5.0, 2.0), Align2::LEFT_TOP, "∞", font(9.0), TEXT_DIM);
            }
        }
        let resp = resp.on_hover_text("Click: set / launch · Ctrl-click: delete · Alt-click: loop to next cue");
        let addr = format!("Deck/{n}/Cue{}", i + 1);
        learnable(core, &resp, &addr);
        if core.learn {
            continue;
        }
        let mods = ui.input(|i| i.modifiers);
        let hc = format!("Deck/{n}/HotCue");
        if resp.clicked() && mods.ctrl {
            act_i(core, &format!("Deck/{n}/DeleteCue"), i as i32 + 1);
        } else if resp.clicked() && mods.alt {
            act_i(core, &format!("Deck/{n}/SetCueLoop"), i as i32 + 1);
        } else if v.momentary_cues && set {
            if resp.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed()) {
                act(core, &hc, &[rosc::OscType::Int(i as i32 + 1), rosc::OscType::Int(1)]);
            }
            if resp.drag_stopped() || resp.clicked() {
                act(core, &hc, &[rosc::OscType::Int(i as i32 + 1), rosc::OscType::Int(0)]);
            }
        } else if resp.clicked() {
            act_i(core, &hc, i as i32 + 1);
        }
    }
}

fn bottom_row(ui: &mut Ui, core: &mut Core, d: usize, rect: Rect) {
    let v = core.snap.decks[d];
    let n = d + 1;
    let a = |s: &str| format!("Deck/{n}/{s}");
    let dm = core.session.decks[d].clone();
    let skip = core.settings.skip_beats;
    let skip_txt = if skip.fract() == 0.0 { format!("{}", skip as i64) } else { format!("{skip}") };
    // (label, weight)
    let items: [(&str, f32); 12] = [
        ("loop", 1.6),
        ("I", 0.8),
        ("O", 0.8),
        ("back", 1.8),
        ("fwd", 1.8),
        ("1/32", 1.8),
        ("1/16", 1.8),
        ("1/8", 1.8),
        ("1/4", 1.8),
        ("1/2", 1.8),
        ("D", 0.8),
        ("T", 0.8),
    ];
    let total: f32 = items.iter().map(|x| x.1).sum();
    let gap = 3.0;
    let unit = (rect.width() - gap * (items.len() - 1) as f32) / total;
    let mut x = rect.left();
    let slip_names = ["ThirtySecond", "Sixteenth", "Eighth", "Quarter", "Half"];
    for (idx, (lbl, wgt)) in items.iter().enumerate() {
        let w = unit * wgt;
        let r = Rect::from_min_size(Pos2::new(x, rect.top()), Vec2::new(w, rect.height()));
        x += w + gap;
        match *lbl {
            "loop" => {
                let resp = btn(ui, r, "", if v.loop_on { Look::On(RED) } else { Look::Normal });
                loop_icon(ui, r, if v.loop_on { Color32::WHITE } else { TEXT });
                let resp = resp.on_hover_text(format!("Loop {} (right-click: exit)", if v.loop_on { super::fmt_beats(v.loop_beats) } else { super::fmt_beats(core.settings.default_loop_beats) }));
                learnable(core, &resp, &a("Loop"));
                if resp.clicked() {
                    act_i(core, &a("Loop"), 1);
                }
                if resp.secondary_clicked() {
                    act_i(core, &a("LoopExit"), 1);
                }
            }
            "I" | "O" => {
                let addr = if *lbl == "I" { "LoopIn" } else { "LoopOut" };
                let resp = btn(ui, r, lbl, Look::Normal).on_hover_text("Set loop in/out; while looping, IN halves and OUT doubles");
                learnable(core, &resp, &a(addr));
                if resp.clicked() {
                    act_i(core, &a(addr), 1);
                }
            }
            "back" | "fwd" => {
                let (txt, addr) = if *lbl == "back" { (format!("← {skip_txt}"), "SkipBeatBack") } else { (format!("{skip_txt} →"), "SkipBeatForward") };
                let resp = btn(ui, r, &txt, Look::Normal).on_hover_text(format!("Skip {skip_txt} beats (Settings → Decks to change)"));
                learnable(core, &resp, &a(addr));
                if resp.clicked() {
                    act_i(core, &a(addr), 1);
                }
            }
            "D" | "T" => {
                let (on, addr) = if *lbl == "D" { (dm.slip_dotted, "SlipDotted") } else { (dm.slip_triplet, "SlipTriplet") };
                let resp = btn(ui, r, lbl, if on { Look::On(SILVER) } else { Look::Normal }).on_hover_text("Dotted / triplet slip loops");
                learnable(core, &resp, &a(addr));
                if resp.clicked() {
                    act_i(core, &a(addr), !on as i32);
                }
            }
            frac => {
                let name = slip_names[idx - 5];
                let addr = a(&format!("SlipLoop/{name}"));
                let resp = btn(ui, r, frac, Look::Normal).on_hover_text(format!("Slip loop {frac} bar (hold)"));
                if resp.is_pointer_button_down_on() {
                    ui.painter().rect_filled(r, CornerRadius::same(3), RED);
                    ui.painter().text(r.center(), Align2::CENTER_CENTER, frac, font(11.0), Color32::WHITE);
                }
                learnable(core, &resp, &addr);
                if core.learn {
                    continue;
                }
                if resp.is_pointer_button_down_on() {
                    act_i(core, &addr, 1);
                } else if resp.drag_stopped() || resp.clicked() {
                    act_i(core, &addr, 0);
                }
            }
        }
    }
}

// ---------------------------------------------------------------- sampler deck

fn sampler_deck(ui: &mut Ui, core: &mut Core, d: usize, r: Rect) {
    let n = d + 1;
    let sv = core.snap.samplers[d];
    let sm = core.session.decks[d].sampler.clone();
    let sel = sm.selected_lane;
    let color = DECK[d];

    // Header row like a track deck.
    let h1 = 26.0;
    let play_r = Rect::from_min_size(r.left_top(), Vec2::new(LEFT_W, h1));
    let pr = ui.allocate_rect(play_r, Sense::click());
    ui.painter().rect_filled(play_r, CornerRadius::same(3), BTN);
    play_icon(ui, play_r, RED, sv.playing);
    learnable(core, &pr, &format!("Deck/{n}/Sampler/PlayPause"));
    if pr.clicked() {
        act_i(core, &format!("Deck/{n}/Sampler/PlayPause"), 1);
    }
    let badge_r = Rect::from_min_size(Pos2::new(r.left() + LEFT_W + 6.0, r.top() + 1.0), Vec2::new(24.0, 24.0));
    badge(ui, core, d, badge_r);
    ui.painter().text(badge_r.right_center() + Vec2::new(8.0, 0.0), Align2::LEFT_CENTER, format!("SAMPLER · {}", sm.name), font(12.0), TEXT);
    let mut x = r.right();
    for (lbl, i) in [("2×", 2), ("1×", 1), ("½", 0)] {
        let br = Rect::from_min_size(Pos2::new(x - 28.0, r.top()), Vec2::new(28.0, h1));
        let on = [0.5, 1.0, 2.0][i] == sm.speed;
        if btn(ui, br, lbl, if on { Look::On(SILVER) } else { Look::Normal }).clicked() {
            act_i(core, &format!("Deck/{n}/Sampler/Speed"), i as i32);
        }
        x -= 30.0;
    }
    for p in (0..8).rev() {
        let br = Rect::from_min_size(Pos2::new(x - 22.0, r.top()), Vec2::new(22.0, h1));
        if btn(ui, br, &format!("{}", p + 1), if sm.pattern == p { Look::On(RED) } else { Look::Normal }).on_hover_text("Pattern").clicked() {
            act_i(core, &format!("Deck/{n}/Sampler/Pattern"), p as i32);
        }
        x -= 24.0;
    }

    // Lane grid.
    let pattern = sm.patterns[sm.pattern];
    let grid_top = r.top() + h1 + GAP;
    let ctl_h = 44.0;
    let grid_h = r.bottom() - ctl_h - GAP - grid_top;
    let row_h = (grid_h / LANES as f32).floor();
    let name_w = 96.0;
    let step_w = (r.width() - name_w - 4.0) / STEPS as f32;
    for lane in 0..LANES {
        let y = grid_top + lane as f32 * row_h;
        let lp = sm.lanes[lane].params;
        let nr = Rect::from_min_size(Pos2::new(r.left(), y), Vec2::new(name_w, row_h - 2.0));
        let name = sm.lanes[lane]
            .sample
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().chars().take(14).collect::<String>())
            .unwrap_or_else(|| format!("Lane {}", lane + 1));
        let peak = sv.lane_peak[lane].min(1.0);
        let fill = if sel == lane { dim(color, 0.55) } else { lighten(BTN, peak * 0.4) };
        let resp = btn_sized(ui, nr, &name, if lp.mute { Look::Off } else { Look::On(fill) }, 9.5)
            .on_hover_text("Click: select · Ctrl-click: mute · Right-click: trigger");
        if resp.clicked() {
            if ui.input(|i| i.modifiers.ctrl) {
                core.set_lane(d, lane, |p| p.mute = !p.mute);
            } else {
                core.session.decks[d].sampler.selected_lane = lane;
            }
        }
        if resp.secondary_clicked() {
            core.route(&format!("Deck/{n}/Sampler/Trigger"), &[rosc::OscType::Int(lane as i32 + 1)]);
        }
        for s in 0..STEPS {
            let sr = Rect::from_min_size(Pos2::new(r.left() + name_w + 4.0 + s as f32 * step_w, y), Vec2::new(step_w - 2.0, row_h - 2.0));
            let step = pattern[lane][s];
            let active = s < lp.steps as usize;
            let playing = sv.playing && sv.current_step[lane] as usize == s;
            let resp = ui.allocate_rect(sr, Sense::click());
            let base = if s % 4 == 0 { Color32::from_gray(52) } else { Color32::from_gray(40) };
            let f = if step.on { lighten(dim(color, 0.5 + 0.5 * step.velocity), 0.0) } else if active { base } else { Color32::from_gray(22) };
            let p = ui.painter();
            p.rect_filled(sr, CornerRadius::same(2), f);
            if playing {
                p.rect_stroke(sr, CornerRadius::same(2), Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Inside);
            }
            if resp.clicked() {
                core.set_step(d, lane, s, Step { on: !step.on, velocity: step.velocity });
            }
            if resp.hovered() && step.on {
                let sc = ui.input(|i| i.smooth_scroll_delta.y);
                if sc != 0.0 {
                    core.set_step(d, lane, s, Step { on: true, velocity: (step.velocity + sc / 400.0).clamp(0.05, 1.0) });
                }
            }
        }
    }

    // Selected lane controls.
    let cr = Rect::from_min_max(Pos2::new(r.left(), r.bottom() - ctl_h), r.right_bottom());
    let lp = sm.lanes[sel].params;
    let mut kx = cr.left() + 16.0;
    let ky = cr.top() + 16.0;
    let mut vals = [lp.attack, lp.hold, lp.release, lp.curve, lp.volume, sm.swing];
    let names = ["ATTACK", "HOLD", "RELEASE", "CURVE", "VOLUME", "SWING"];
    let defaults = [0.0, 1.0, 0.1, 0.5, 0.75, 0.0];
    for i in 0..6 {
        let resp = small_knob(ui, Pos2::new(kx, ky), 12.0, &mut vals[i], defaults[i], false, RED);
        ui.painter().text(Pos2::new(kx, ky + 22.0), Align2::CENTER_CENTER, names[i], font(8.0), TEXT_DIM);
        if resp.changed() {
            let addr = if i == 5 { format!("Deck/{n}/Sampler/Swing") } else { format!("Deck/{n}/Sampler/{}", names[i]) };
            core.route(&addr, &[rosc::OscType::Float(vals[i])]);
        }
        kx += 46.0;
    }
    let mut pitch = (lp.pitch + 24.0) / 48.0;
    let resp = small_knob(ui, Pos2::new(kx, ky), 12.0, &mut pitch, 0.5, true, RED);
    ui.painter().text(Pos2::new(kx, ky + 22.0), Align2::CENTER_CENTER, format!("{:+.0} ST", lp.pitch), font(8.0), TEXT_DIM);
    if resp.changed() {
        core.set_lane(d, sel, |p| p.pitch = (pitch * 48.0 - 24.0).round());
    }
    kx += 46.0;
    let mut steps = (lp.steps as f32 - 1.0) / 15.0;
    let resp = small_knob(ui, Pos2::new(kx, ky), 12.0, &mut steps, 1.0, false, RED);
    ui.painter().text(Pos2::new(kx, ky + 22.0), Align2::CENTER_CENTER, format!("{} STEPS", lp.steps), font(8.0), TEXT_DIM);
    if resp.changed() {
        core.set_lane(d, sel, |p| p.steps = (steps * 15.0 + 1.0).round() as u8);
    }
    kx += 30.0;
    let dir = match lp.direction {
        Direction::Forward => "→",
        Direction::Reverse => "←",
        Direction::PingPong => "↔",
        Direction::Random => "?",
    };
    let rh = match lp.rhythm {
        Rhythm::Normal => "N",
        Rhythm::Dotted => "D",
        Rhythm::Triplet => "T",
    };
    let laneset = |core: &mut Core, ctl: &str| {
        core.route(
            &format!("Deck/{n}/Sampler/LaneSet"),
            &[rosc::OscType::Int(sel as i32 + 1), rosc::OscType::String(ctl.into()), rosc::OscType::Float(1.0)],
        );
    };
    let bw = 34.0;
    let by = cr.top() + 6.0;
    if btn(ui, Rect::from_min_size(Pos2::new(kx, by), Vec2::new(bw, 22.0)), dir, Look::Normal).on_hover_text("Direction").clicked() {
        laneset(core, "Direction");
    }
    if btn(ui, Rect::from_min_size(Pos2::new(kx + bw + 3.0, by), Vec2::new(bw, 22.0)), rh, Look::Normal).on_hover_text("Rate").clicked() {
        laneset(core, "Rhythm");
    }
    if btn(ui, Rect::from_min_size(Pos2::new(kx + 2.0 * (bw + 3.0), by), Vec2::new(bw + 10.0, 22.0)), if lp.mono { "MONO" } else { "POLY" }, Look::Normal).clicked() {
        core.set_lane(d, sel, |p| p.mono = !p.mono);
    }
    let clr = Rect::from_min_size(Pos2::new(kx + 3.0 * (bw + 3.0) + 10.0, by), Vec2::new(54.0, 22.0));
    if btn(ui, clr, "CLEAR", Look::Normal).on_hover_text("Remove this lane's sample (drag a file onto the deck to load one)").clicked() {
        core.clear_sample(d, sel);
    }
    let mut child = ui.new_child(UiBuilder::new().max_rect(Rect::from_min_max(Pos2::new(clr.right() + 8.0, by), cr.right_bottom())));
    child.label(RichText::new(format!("Lane {} · drop a file here to load it", sel + 1)).small().color(TEXT_DIM));
}
