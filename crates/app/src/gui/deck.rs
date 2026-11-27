//! Deck cards: large (focused) and compact, plus the sampler grid.

use std::path::PathBuf;

use eframe::egui::{self, Align2, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, Ui, Vec2};

use dj_engine::engine::{ClockOwner, DeckCmd, DeckMode};
use dj_engine::sampler::{Direction, Rhythm, Step, LANES, STEPS};
use dj_engine::Command;
use dj_library::key::Compat;

use super::skin::*;
use super::{act, act_f, act_i, learnable, UiState};
use crate::core::automix::{MixHow, MixPlan};
use crate::core::Core;
use crate::settings::MixStyle;

fn mmss_remain(s: f64) -> String {
    let s = s.max(0.0);
    format!("-{}:{:02}", (s / 60.0) as u32, (s % 60.0) as u32)
}

/// Accept library rows dropped on a deck card.
fn accept_drop(ui: &mut Ui, core: &mut Core, d: usize, rect: Rect) {
    let ctx = ui.ctx().clone();
    if !egui::DragAndDrop::has_payload_of_type::<PathBuf>(&ctx) {
        return;
    }
    if ctx.pointer_hover_pos().map(|p| rect.contains(p)).unwrap_or(false) {
        ui.painter().rect_stroke(rect, CornerRadius::same(12), Stroke::new(2.0, DECK[d]), egui::StrokeKind::Inside);
        if ctx.input(|i| i.pointer.any_released()) {
            if let Some(p) = egui::DragAndDrop::take_payload::<PathBuf>(&ctx) {
                if core.session.decks[d].mode == DeckMode::Sampler {
                    let lane = core.session.decks[d].sampler.selected_lane;
                    core.load_sample(d, lane, &p);
                } else {
                    core.request_load(d, &p);
                }
            }
        }
    }
}

/// Key chip border: green when it mixes with the deck you hear (see `Core::reference_deck`),
/// red on a clash.
fn key_border(core: &Core, d: usize) -> Color32 {
    let is_reference = core.reference_deck() == Some(d);
    match (core.deck_key(d), core.reference_key()) {
        (Some(_), _) if is_reference => GREEN,
        // (From the heard deck's key to this one: two up is the boost.)
        (Some(k), Some(r)) => match r.compat(k) {
            Compat::Same | Compat::Adjacent | Compat::Relative | Compat::Boost => GREEN,
            Compat::Clash => dim(RED, 0.8),
            Compat::Diagonal => Color32::from_gray(90),
        },
        _ => Color32::from_gray(70),
    }
}

/// Badge with a context menu (deck type, lock, grid tools).
fn badge_menu(ui: &mut Ui, core: &mut Core, d: usize, rect: Rect, size: f32) {
    deck_badge(ui, rect, d, size);
    let r = ui.allocate_rect(rect, Sense::click()).on_hover_text("Right-click: deck type, lock, grid");
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
        ui.separator();
        ui.horizontal(|ui| {
            use dj_engine::deck::TempoRange as T;
            ui.label("Tempo range");
            let cur = core.snap.decks[d].tempo_range;
            for (i, rg) in [T::Pct6, T::Pct10, T::Pct16, T::Pct50, T::Semitones12].iter().enumerate() {
                if ui.selectable_label(cur == *rg, rg.label()).clicked() {
                    act_i(core, &format!("Deck/{}/TempoRange", d + 1), i as i32);
                }
            }
        });
        if core.decks[d].info.is_some() {
            let locked = core.decks[d].info.as_ref().map(|i| i.locked).unwrap_or(false);
            ui.separator();
            if ui.button(if locked { "Unlock track" } else { "Lock track (cues not saved)" }).clicked() {
                core.set_track_locked(d, !locked);
                ui.close();
            }
            if core.settings.stems_enabled && ui.button("Detect cue points").on_hover_text("Find this track's sections (Intro, Drop, Break, Vocal, Outro) from its stems: one within a bar of a cue it has replaces that cue, the others go on free pads").clicked() {
                core.redetect_cues(d);
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
            if ui.button("Eject").clicked() {
                core.eject(d);
                ui.close();
            }
        }
    });
}

/// Title + artist block.
fn title_block(ui: &Ui, core: &Core, d: usize, rect: Rect, big: bool) {
    let p = ui.painter_at(rect);
    let (title, artist) = match &core.decks[d].info {
        Some(i) => (i.title(), i.tags.artist.clone().unwrap_or_default()),
        None => match (&core.decks[d].loading, &core.decks[d].error) {
            (Some(path), _) => (format!("Loading {}…", path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()), String::new()),
            (None, Some(e)) => ("Could not load".into(), e.clone()),
            _ => ("Empty".into(), "Drag a track here".into()),
        },
    };
    let (ts, as_) = if big { (17.0, 13.0) } else { (14.0, 12.0) };
    let title = fit_text(ui, &title, bold(ts), rect.width());
    p.text(Pos2::new(rect.left(), rect.center().y - 1.0), Align2::LEFT_BOTTOM, title, bold(ts), TEXT);
    // Stems status after the artist (the stems themselves are on the channel faders).
    let status = if big { stems_status(core, d) } else { None };
    let status_w = status.as_ref().map_or(0.0, |(t, _)| ui.painter().layout_no_wrap(format!(" · {t}"), sans(as_), TEXT).size().x);
    let artist = fit_text(ui, &artist, sans(as_), (rect.width() - status_w).max(40.0));
    let aw = if artist.is_empty() { 0.0 } else { ui.painter().layout_no_wrap(artist.clone(), sans(as_), TEXT).size().x };
    p.text(Pos2::new(rect.left(), rect.center().y + 2.0), Align2::LEFT_TOP, artist, sans(as_), TEXT_DIM);
    if let Some((text, col)) = status {
        let text = if aw > 0.0 { format!(" · {text}") } else { text };
        p.text(Pos2::new(rect.left() + aw, rect.center().y + 2.0), Align2::LEFT_TOP, text, sans(as_), col);
    }
}

/// Short stems status for the deck header while a deck has no stems: separation progress, or why
/// there are none (stem levels only change the sound once a deck has stems). Hidden once ready.
pub fn stems_status(core: &Core, d: usize) -> Option<(String, Color32)> {
    let v = core.snap.decks[d];
    if !v.loaded {
        return None;
    }
    if !core.settings.stems_enabled {
        return Some(("stems off (Settings)".into(), TEXT_FAINT));
    }
    // Once the stems are there, nothing to say.
    match core.decks[d].stems_progress {
        Some(f) => Some((format!("stems {:.0}%", f * 100.0), DECK[d])),
        None if v.stems_ready => None,
        None if core.decks[d].stems_model_loading => Some(("stems: model loading…".into(), TEXT_FAINT)),
        None => Some((core.decks[d].stems_missing.unwrap_or("stems…").into(), TEXT_FAINT)),
    }
}

/// Hold-to-preview CUE button behaviour.
fn cue_hold(core: &mut Core, resp: &egui::Response, addr: &str) {
    if core.learn {
        return;
    }
    if resp.drag_started() || (resp.is_pointer_button_down_on() && resp.ctx.input(|i| i.pointer.primary_pressed())) {
        act_i(core, addr, 1);
    }
    if resp.drag_stopped() || resp.clicked() {
        act_i(core, addr, 0);
    }
}

pub fn big(ui: &mut Ui, core: &mut Core, st: &mut UiState, d: usize, rect: Rect) {
    card(ui, rect);
    accept_drop(ui, core, d, rect);
    if core.session.decks[d].mode == DeckMode::Sampler {
        sampler(ui, core, d, rect);
        return;
    }
    let v = core.snap.decks[d];
    let n = d + 1;
    let a = |s: &str| format!("Deck/{n}/{s}");
    let color = DECK[d];
    let is_master = core.snap.clock.owner == ClockOwner::Deck(d as u8);
    // Controller layout: a MIDI controller has its own cue pads, so the pads become one slim
    // row and everything is smaller, to fit two decks per side.
    let dense = st.dense;
    let pad = if dense { 10.0 } else { 16.0 };
    let r = rect.shrink(pad);
    let w = r.width();
    let gap = 8.0;

    // ---------------------------------------------------------- header
    let hh = if dense { 34.0 } else { 44.0 };
    let badge = Rect::from_min_size(r.min, Vec2::splat(hh));
    badge_menu(ui, core, d, badge, 22.0);
    let time_w = 92.0;
    let time_r = Rect::from_min_size(Pos2::new(r.right() - time_w, r.top()), Vec2::new(time_w, hh));
    ui.painter().text(time_r.right_center(), Align2::RIGHT_CENTER, mmss_remain(v.duration_secs - v.position_secs), mono(if dense { 24.0 } else { 30.0 }), if v.loaded && v.playing && v.duration_secs - v.position_secs < 30.0 { RED } else { TEXT });
    let bpm_r = Rect::from_min_size(Pos2::new(time_r.left() - 88.0, r.top()), Vec2::new(80.0, hh));
    let p = ui.painter();
    // (The tempo change in % is shown inside the tempo slider.)
    let bpm_text = if !v.has_grid {
        "—".into()
    } else if core.decks[d].info.as_ref().is_some_and(|i| i.grid_source == Some(dj_library::GridSource::Detected)) {
        format!("≈{:.2}", v.bpm) // detected, not from a tag
    } else {
        format!("{:.2}", v.bpm)
    };
    let bpm_font = mono(if dense { 18.0 } else { 22.0 });
    let tag_font = bold(if dense { 8.5 } else { 9.5 });
    // The deck that sets the master tempo (chosen automatically) says so above its BPM: the pair
    // is centred together, so the label never sits on the digits.
    let bpm_size = p.layout_no_wrap(bpm_text.clone(), bpm_font.clone(), TEXT).size();
    let bpm_h = bpm_size.y;
    // The key chip goes left of the BPM as it is drawn ("≈120.00" is wider than "120.00").
    let bpm_left = bpm_r.right() - bpm_size.x.max(bpm_r.width());
    let tag_h = if is_master { p.layout_no_wrap("MASTER".into(), tag_font.clone(), RED).size().y } else { 0.0 };
    let top = bpm_r.center().y - (bpm_h + tag_h) / 2.0;
    if is_master {
        p.text(Pos2::new(bpm_r.right(), top), Align2::RIGHT_TOP, "MASTER", tag_font, RED);
    }
    p.text(Pos2::new(bpm_r.right(), top + tag_h), Align2::RIGHT_TOP, bpm_text, bpm_font, TEXT);
    let key = core.key_text(core.deck_key(d));
    let chip = Rect::from_min_size(Pos2::new(bpm_left - 56.0, r.top() + (hh - 30.0).max(4.0) / 2.0), Vec2::new(46.0, (hh - 8.0).min(30.0)));
    if !key.is_empty() {
        key_chip(ui, chip, &key, key_border(core, d), 15.0);
    }
    let title_r = Rect::from_min_max(Pos2::new(badge.right() + 14.0, r.top()), Pos2::new(chip.left() - 10.0, r.top() + hh));
    title_block(ui, core, d, title_r, true);

    // ---------------------------------------------------------- transport row
    // Vertical scale: rows grow to fill the card (e.g. 2-deck mode) instead of leaving space
    // unused. Base layout heights: transport 52, pads 2×36+8, stems 26, tempo 30, loops 30 +
    // gaps; never smaller than today's sizes, at most 1.8×.
    // The stems row only exists without the internal mixer (otherwise stems are on the faders
    // and their status is in the header).
    let stems_on = core.settings.stems_enabled && core.external_routing();
    let pads_base = if dense { 30.0 } else { 2.0 * 36.0 + 8.0 };
    // Likewise the tempo slider: with the internal mixer, tempo is a fader function.
    let tempo_on = core.external_routing();
    let tempo_base = if tempo_on { 30.0 + 12.0 } else { 0.0 };
    let base = 52.0 + 10.0 + pads_base + if stems_on { 10.0 + 26.0 + 10.0 } else { 14.0 } + tempo_base + 30.0;
    let k = ((r.bottom() - (r.top() + hh + 12.0)) / base).clamp(if dense { 0.55 } else { 1.0 }, 1.8);
    let y = r.top() + hh + 12.0;
    let bh = 52.0 * k;
    // (The main cue is the first pad of the cue row; dj picks the tempo master itself.)
    // A third button, Mix, while this deck waits silently in a loop to be mixed in.
    let mixable = mix_shown(core, d);
    let big_w = if mixable { (w - 2.0 * gap) / 3.0 } else { (w - gap) / 2.0 };
    let mut x = r.left();
    let mut big_btn = |ui: &mut Ui, label: &str, style: Style, wf: f32| -> egui::Response {
        let rr = Rect::from_min_size(Pos2::new(x, y), Vec2::new(big_w * wf, bh));
        x += big_w * wf + gap;
        button(ui, rr, label, style, bold(17.0 * k.min(1.35)))
    };
    let play_style = if v.playing { Style::Fill(color) } else if v.play_pending { Style::Outline(color, true) } else { Style::Outline(color, false) };
    let resp = big_btn(ui, if v.playing { PAUSE } else if v.play_pending { "Wait…" } else { PLAY }, play_style, 1.0);
    learnable(core, &resp, &a("PlayPause"));
    if resp.clicked() {
        act_i(core, &a("PlayPause"), 1);
    }
    let resp = big_btn(ui, "Sync", if v.sync { Style::Light } else { Style::Dark }, 1.0);
    learnable(core, &resp, &a("TempoSync"));
    if resp.clicked() {
        act_i(core, &a("TempoSync"), 1);
    }
    if mixable {
        let rr = Rect::from_min_size(Pos2::new(x, y), Vec2::new(big_w, bh));
        mix_button(ui, core, st, d, rr, bold(17.0 * k.min(1.35)));
    }

    // ---------------------------------------------------------- hot cues
    // With stems enabled the pads are a little shorter to make room for the stems row.
    let y = y + bh + (if stems_on { 10.0 } else { 14.0 }) * k;
    // Pads take whatever height is left (rows below: stems 26+10+10, tempo 30+12, loops 30).
    let below = (if stems_on { 10.0 + 26.0 + 10.0 } else { 14.0 } + tempo_base + 30.0) * k;
    // The cue row: the main cue first, then the hot cues that are set (empty ones are not
    // shown), then one "+" pad to set the next free one. At most 9 pads.
    let (cols, rows) = if dense { (9, 1) } else { (5, 2) };
    let ph = if dense { (r.bottom() - y - below).clamp(16.0, 40.0 * k) } else { ((r.bottom() - y - 8.0 - below) / 2.0).clamp(28.0, 46.0 * k) };
    let pgap = if dense { 6.0 } else { gap };
    let pw = (w - (cols - 1) as f32 * pgap) / cols as f32;
    // Narrow pads show just the number (the pad colour says the rest).
    let pad_labels = pw >= 56.0;
    let pad_font = if dense { 13.0 } else { 16.0 };
    let slot = |k: usize| Rect::from_min_size(Pos2::new(r.left() + (k % cols) as f32 * (pw + pgap), y + (k / cols) as f32 * (ph + 8.0)), Vec2::new(pw, ph));
    let rad = CornerRadius::same(8);

    let cue_r = slot(0);
    let resp = ui.allocate_rect(cue_r, Sense::click_and_drag()).on_hover_text("Main cue: hold to preview, click while paused to set");
    let p = ui.painter();
    p.rect_filled(cue_r, rad, if resp.hovered() { dim(ORANGE, 0.30) } else { dim(ORANGE, 0.18) });
    p.rect_stroke(cue_r, rad, Stroke::new(1.5, ORANGE), egui::StrokeKind::Inside);
    p.text(cue_r.center(), Align2::CENTER_CENTER, "Cue", bold(pad_font), TEXT);
    learnable(core, &resp, &a("Cue"));
    cue_hold(core, &resp, &a("Cue"));

    let set: Vec<usize> = (0..8).filter(|&i| v.hot_cues[i].is_some()).collect();
    let next_free = (0..8).find(|&i| v.hot_cues[i].is_none());
    let mut shown: Vec<(usize, bool)> = set.iter().map(|&i| (i, true)).collect();
    if let Some(f) = next_free {
        shown.push((f, false));
    }
    for (k, (i, is_set)) in shown.into_iter().enumerate() {
        let rr = slot(k + 1);
        let name = core.decks[d].info.as_ref().and_then(|x| x.cues[i].as_ref()).map(|c| c.name.clone()).unwrap_or_default();
        let resp = ui.allocate_rect(rr, Sense::click_and_drag());
        let p = ui.painter();
        if is_set {
            let c = CUE[i];
            p.rect_filled(rr, rad, if resp.hovered() { dim(c, 0.42) } else { dim(c, 0.32) });
            p.rect_stroke(rr, rad, Stroke::new(1.5, dim(c, 0.85)), egui::StrokeKind::Inside);
            // The cue's name (pads are in order, so no number needed); a cue without a name shows
            // its number. The name fits inside the pad: a smaller font first, then shortened.
            if pad_labels && !name.is_empty() {
                let room = rr.width() - 14.0;
                let font = [pad_font - 1.0, pad_font - 3.0, pad_font - 4.5]
                    .into_iter()
                    .map(sans)
                    .find(|f| ui.painter().layout_no_wrap(name.clone(), f.clone(), TEXT).size().x <= room)
                    .unwrap_or_else(|| sans(pad_font - 4.5));
                let label = fit_text(ui, &name, font.clone(), room);
                ui.painter().text(rr.center(), Align2::CENTER_CENTER, label, font, TEXT);
            } else {
                let label = if v.hot_cue_is_loop[i] { format!("{} Loop", i + 1) } else { format!("{}", i + 1) };
                p.text(rr.center(), Align2::CENTER_CENTER, label, bold(pad_font), TEXT);
            }
        } else {
            // "+": set hot cue `i` (the first free one) here.
            p.rect_stroke(rr, rad, Stroke::new(1.0, Color32::from_rgb(0x2a, 0x2a, 0x30)), egui::StrokeKind::Inside);
            p.text(rr.center(), Align2::CENTER_CENTER, "+", bold(pad_font + 2.0), if resp.hovered() { TEXT } else { TEXT_FAINT });
        }
        let tip = if !is_set {
            "Set a hot cue at the playhead"
        } else if core.settings.cue_pads == crate::settings::CuePadMode::Loop {
            "Click: loop from this cue (loop size below) · Ctrl-click: delete · Alt-click: loop to the next cue"
        } else {
            "Click: jump to this cue · Ctrl-click: delete · Alt-click: loop to the next cue"
        };
        let resp = resp.on_hover_text(tip);
        let addr = a(&format!("Cue{}", i + 1));
        learnable(core, &resp, &addr);
        if core.learn {
            continue;
        }
        let mods = ui.input(|i| i.modifiers);
        let hc = a("HotCue");
        if resp.clicked() && mods.ctrl {
            act_i(core, &a("DeleteCue"), i as i32 + 1);
        } else if resp.clicked() && mods.alt {
            act_i(core, &a("SetCueLoop"), i as i32 + 1);
        } else if v.momentary_cues && is_set {
            if resp.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed()) {
                act(core, &hc, &[rosc::OscType::Int(i as i32 + 1), rosc::OscType::Int(1)]);
            }
            if resp.drag_stopped() || resp.clicked() {
                act(core, &hc, &[rosc::OscType::Int(i as i32 + 1), rosc::OscType::Int(0)]);
            }
        } else if resp.clicked() {
            // Like a controller pad: sets an empty cue; a set one loops from it or jumps to it
            // (Settings → Hot cue pads).
            act_i(core, &addr, 1);
        }
    }

    // ---------------------------------------------------------- stems
    let after_cues = y + rows as f32 * ph + (rows - 1) as f32 * 8.0;
    let y = if stems_on {
        let stems_h = 26.0 * k;
        stems_row(ui, core, d, Rect::from_min_size(Pos2::new(r.left(), after_cues + 10.0 * k), Vec2::new(w, stems_h)));
        after_cues + (10.0 + 10.0) * k + stems_h
    } else {
        after_cues + 14.0 * k
    };

    // ---------------------------------------------------------- tempo (external mixer only)
    let th = if tempo_on { 30.0 * k } else { 0.0 };
    if tempo_on {
    let minus = Rect::from_min_size(Pos2::new(r.left(), y), Vec2::new(36.0, th));
    let plus = Rect::from_min_size(Pos2::new(r.right() - 36.0, y), Vec2::new(36.0, th));
    let follower = v.sync && !is_master;
    for (rr, label, dir) in [(minus, "−", -1.0f32), (plus, "+", 1.0)] {
        let resp = button(ui, rr, label, Style::Dark, bold(18.0)).on_hover_text("Fine tempo step");
        if resp.clicked() {
            act_f(core, &a("TempoSlider"), (shown_fader(&v, follower) as f32 + dir * 0.01).clamp(-1.0, 1.0));
        }
    }
    let track = Rect::from_min_max(Pos2::new(minus.right() + gap, y), Pos2::new(plus.left() - gap, y + th));
    let resp = ui.allocate_rect(track, Sense::click_and_drag());
    let p = ui.painter();
    p.rect_filled(track, CornerRadius::same(8), WELL);
    let cx = track.center().x;
    // Followers show where their actual speed sits on the slider.
    let shown = shown_fader(&v, follower);
    let hx = cx + shown as f32 * (track.width() / 2.0 - 8.0);
    p.rect_filled(Rect::from_min_max(Pos2::new(cx.min(hx), track.top()), Pos2::new(cx.max(hx), track.bottom())), CornerRadius::same(4), alpha(color, 0.32));
    p.line_segment([Pos2::new(cx, track.top() + 6.0), Pos2::new(cx, track.bottom() - 6.0)], Stroke::new(1.0, TEXT_FAINT));
    p.text(Pos2::new(track.left() + 12.0, track.center().y), Align2::LEFT_CENTER, format!("Tempo {}", v.tempo_range.label()), sans(13.0), if follower { TEXT_DIM } else { TEXT });
    let pct = (v.speed - 1.0) * 100.0;
    let pct_col = if pct.abs() < 0.05 { TEXT_DIM } else { color };
    p.text(Pos2::new(track.right() - 12.0, track.center().y), Align2::RIGHT_CENTER, format!("{pct:+.1}%"), mono(13.0 * k.min(1.3)), pct_col);
    let handle = Rect::from_center_size(Pos2::new(hx, track.center().y), Vec2::new(12.0, th - 4.0));
    p.rect_filled(handle, CornerRadius::same(4), color);
    p.line_segment([Pos2::new(hx, handle.top() + 6.0), Pos2::new(hx, handle.bottom() - 6.0)], Stroke::new(2.0, on(color)));
    let resp = resp.on_hover_text(format!("{:+.2}% · drag (Shift: fine), double-click: reset, right-click: range{}", (v.speed - 1.0) * 100.0, if follower { " · synced: sets the tempo of the mix" } else { "" }));
    learnable(core, &resp, &a("TempoSlider"));
    if !core.learn {
        if resp.double_clicked() {
            act_f(core, &a("TempoSlider"), 0.0);
        } else if resp.dragged() {
            let fine = ui.input(|i| i.modifiers.shift);
            let dv = resp.drag_delta().x / (track.width() / 2.0) * if fine { 0.1 } else { 1.0 };
            act_f(core, &a("TempoSlider"), (shown as f32 + dv).clamp(-1.0, 1.0));
        }
    }
    resp.context_menu(|ui| {
        use dj_engine::deck::TempoRange as T;
        for (i, rg) in [T::Pct6, T::Pct10, T::Pct16, T::Pct50, T::Semitones12].iter().enumerate() {
            if ui.selectable_label(v.tempo_range == *rg, rg.label()).clicked() {
                act_i(core, &a("TempoRange"), i as i32);
                ui.close();
            }
        }
    });
    }

    // ---------------------------------------------------------- loops
    let y = if tempo_on { y + th + 12.0 * k } else { y };
    let lh = 30.0 * k;
    let size = core.loop_size[d];
    let size_txt = if v.loop_on { beats_label(v.loop_beats) } else { beats_label(size) };
    let pending = core.decks[d].loop_in_pending && !v.loop_on;
    let in_out = if v.loop_on { "Exit" } else if pending { "Out" } else { "In" };
    let labels = [in_out, "÷2", size_txt.as_str(), "×2", "«", "»"];
    let lgap = 6.0;
    let lw = (w - lgap * (labels.len() as f32 - 1.0)) / labels.len() as f32;
    for (i, label) in labels.iter().enumerate() {
        let rr = Rect::from_min_size(Pos2::new(r.left() + i as f32 * (lw + lgap), y), Vec2::new(lw, lh));
        let style = match i {
            0 if pending => Style::Outline(color, true),
            0 | 2 if v.loop_on => Style::Light,
            _ => Style::Dark,
        };
        let resp = button(ui, rr, label, style, bold(13.0 * k.min(1.3)));
        match i {
            0 => {
                let resp = resp.on_hover_text("Loop in, then loop out · while looping: exit");
                learnable(core, &resp, &a("LoopInOut"));
                if resp.clicked() {
                    act_i(core, &a("LoopInOut"), 1);
                }
            }
            1 | 3 => {
                if resp.clicked() {
                    let f = if i == 1 { 0.5 } else { 2.0 };
                    act_f(core, &a("LoopSize"), (size * f) as f32);
                    if v.loop_on {
                        act_i(core, &a(if i == 1 { "LoopHalve" } else { "LoopDouble" }), 1);
                    }
                }
            }
            2 => {
                let resp = resp.on_hover_text("Loop this many beats");
                learnable(core, &resp, &a("Loop"));
                if resp.clicked() && !core.learn {
                    if v.loop_on {
                        act_i(core, &a("LoopExit"), 1);
                    } else {
                        act_f(core, &a("LoopBeats"), size as f32);
                    }
                }
            }
            _ => {
                if resp.clicked() {
                    let dir = if i == 4 { -1.0 } else { 1.0 };
                    core.send(Command::Deck { deck: d as u8, cmd: DeckCmd::Skip(dir * size) });
                }
            }
        }
    }
}

/// External-mixer mode only: stem toggles (light = audible) and the separation status. With the
/// internal mixer, stems are on the channel faders (Drums/Bass/Vocals/Other with ON/OFF).
fn stems_row(ui: &mut Ui, core: &mut Core, d: usize, r: Rect) {
    let v = core.snap.decks[d];
    let n = d + 1;
    let a = |s: &str| format!("Deck/{n}/{s}");
    let color = DECK[d];
    let ready = v.stems_ready;
    let gap = 6.0;
    let all: [(&str, &str, f32); 4] = [
        ("Drums", "StemDrums", 1.0),
        ("Bass", "StemBass", 1.0),
        ("Vocals", "StemVocals", 1.0),
        ("Other", "StemOther", 1.0),
    ];
    let status_w = (r.width() * 0.22).clamp(80.0, 150.0);
    let items: Vec<(usize, &(&str, &str, f32))> = all.iter().enumerate().collect();
    let unit = (r.width() - status_w - gap * items.len() as f32) / items.iter().map(|(_, i)| i.2).sum::<f32>();
    let mut x = r.left();
    for (i, (label, addr, weight)) in items {
        let rr = Rect::from_min_size(Pos2::new(x, r.top()), Vec2::new(unit * weight, r.height()));
        x += unit * weight + gap;
        let on_ = v.stem_gains[i] > crate::core::STEM_AUDIBLE;
        // Until the stems are ready, settings are shown in outline: they apply once separated.
        let style = match (on_, ready) {
            (true, true) => Style::Light,
            (true, false) => Style::Outline(TEXT_FAINT, false),
            (false, _) => Style::Dark,
        };
        let tip = if ready { "Lit = audible" } else { "Still separating: the setting applies as soon as stems are ready" };
        let resp = button(ui, rr, label, style, bold(11.5)).on_hover_text(tip);
        // Colour mark: the stem's colour in the waveform lanes.
        let mark = Rect::from_min_size(Pos2::new(rr.left() + 6.0, rr.bottom() - 4.0), Vec2::new(rr.width() - 12.0, 2.0));
        ui.painter().rect_filled(mark, CornerRadius::same(1), if on_ { STEM[i] } else { dim(STEM[i], 0.5) });
        learnable(core, &resp, &a(addr));
        if resp.clicked() && v.loaded {
            act_i(core, &a(addr), !on_ as i32);
        }
    }

    // Status pill: progress while separating, then where the stems came from.
    let sr = Rect::from_min_max(Pos2::new(r.right() - status_w, r.top()), r.right_bottom());
    let rad = CornerRadius::same(6);
    let p = ui.painter_at(sr);
    p.rect_filled(sr, rad, WELL);
    let (text, col) = match (core.decks[d].stems_progress, &core.decks[d].stems_source, v.loaded) {
        (Some(f), _, true) => {
            let fill = Rect::from_min_size(sr.min, Vec2::new(sr.width() * f.clamp(0.0, 1.0), sr.height()));
            p.rect_filled(fill, rad, alpha(color, 0.35));
            (format!("Separating {:.0}%", f * 100.0), TEXT)
        }
        (None, _, true) if ready => (String::new(), TEXT_DIM),
        (None, _, true) if core.decks[d].stems_model_loading => ("Loading model…".into(), TEXT_FAINT),
        (None, _, true) => (core.decks[d].stems_missing.unwrap_or("No stems").into(), TEXT_FAINT),
        _ => ("Stems".into(), TEXT_FAINT),
    };
    let fitted = fit_text(ui, &text, bold(11.0), sr.width() - 12.0);
    p.text(sr.center(), Align2::CENTER_CENTER, &fitted, bold(11.0), col);
    let hover = ui.allocate_rect(sr, Sense::hover());
    if fitted != text {
        hover.on_hover_text(text);
    }
}

/// `text`, shortened with "…" until it fits in `max_w` points.
fn fit_text(ui: &Ui, text: &str, font: egui::FontId, max_w: f32) -> String {
    let width = |s: &str| ui.painter().layout_no_wrap(s.to_string(), font.clone(), TEXT).size().x;
    if width(text) <= max_w {
        return text.to_string();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let candidate = format!("{}…", chars.iter().collect::<String>().trim_end());
        if width(&candidate) <= max_w {
            return candidate;
        }
    }
    "…".into()
}

pub fn beats_label(b: f64) -> String {
    if !(b.is_finite() && b > 0.0) {
        "—".into()
    } else if b >= 1.0 {
        if (b - b.round()).abs() < 0.01 { format!("{}", b.round() as i64) } else { format!("{b:.1}") }
    } else {
        format!("1/{}", (1.0 / b).round() as i64)
    }
}

pub fn compact(ui: &mut Ui, core: &mut Core, st: &mut UiState, d: usize, rect: Rect) {
    card(ui, rect);
    accept_drop(ui, core, d, rect);
    let v = core.snap.decks[d];
    let n = d + 1;
    let a = |s: &str| format!("Deck/{n}/{s}");
    let color = DECK[d];
    let r = rect.shrink2(Vec2::new(14.0, 12.0));
    let row_h = 38.0;
    let badge = Rect::from_min_size(Pos2::new(r.left(), r.top() + 3.0), Vec2::splat(32.0));
    badge_menu(ui, core, d, badge, 16.0);
    let sampler = core.session.decks[d].mode == DeckMode::Sampler;

    // Buttons from the right, narrower when needed so the title (at least ~130 points) and the key
    // chip keep their room.
    let mix = !sampler && mix_shown(core, d);
    let widths = [72.0, 76.0, 66.0, 80.0, if mix { 80.0 } else { 0.0 }];
    let gap = 8.0;
    let need: f32 = widths.iter().sum::<f32>() + gap * (if mix { 4.0 } else { 3.0 });
    let room = r.width() - (32.0 + 12.0) - 130.0 - 56.0;
    let k = (room / need).clamp(0.6, 1.0);
    let bw = 72.0 * k;
    let mut x = r.right();
    let mut next = |w: f32| {
        x -= w;
        let rr = Rect::from_min_size(Pos2::new(x, r.top()), Vec2::new(w, row_h));
        x -= gap * k;
        rr
    };
    let playing = if sampler { core.snap.samplers[d].playing } else { v.playing };
    let sync_r = next(bw);
    let play_r = next(76.0 * k);
    let cue_r = next(66.0 * k);
    let swap_r = next(80.0 * k);
    let mix_r = mix.then(|| next(80.0 * k));
    if let Some(mix_r) = mix_r {
        mix_button(ui, core, st, d, mix_r, bold(14.0));
    }
    let resp = button(ui, swap_r, if k < 0.85 { "⇄" } else { "⇄ Swap" }, Style::Outline(Color32::from_gray(110), false), bold(14.0))
        .on_hover_text("Show this deck in the large view");
    if resp.clicked() {
        st.focus[d % 2] = d;
    }
    if !sampler {
        let resp = button(ui, sync_r, "Sync", if v.sync { Style::Light } else { Style::Dark }, bold(15.0));
        learnable(core, &resp, &a("TempoSync"));
        if resp.clicked() {
            act_i(core, &a("TempoSync"), 1);
        }
        let resp = button(ui, cue_r, "Cue", Style::Outline(ORANGE, false), bold(15.0));
        learnable(core, &resp, &a("Cue"));
        cue_hold(core, &resp, &a("Cue"));
    }
    let resp = button(
        ui,
        play_r,
        if playing { PAUSE } else { PLAY },
        if playing { Style::Fill(color) } else { Style::Outline(color, false) },
        bold(15.0),
    );
    let play_addr = if sampler { a("Sampler/PlayPause") } else { a("PlayPause") };
    learnable(core, &resp, &play_addr);
    if resp.clicked() {
        act_i(core, &play_addr, 1);
    }
    let chip = Rect::from_min_size(Pos2::new(mix_r.unwrap_or(swap_r).left() - 48.0, r.top() + 7.0), Vec2::new(40.0, 24.0));
    if !sampler {
        let key = core.key_text(core.deck_key(d));
        if !key.is_empty() {
            key_chip(ui, chip, &key, key_border(core, d), 12.5);
        }
    }
    let title_r = Rect::from_min_max(Pos2::new(badge.right() + 12.0, r.top()), Pos2::new(chip.left() - 8.0, r.top() + row_h));
    if sampler {
        let p = ui.painter_at(title_r);
        p.text(title_r.left_center(), Align2::LEFT_CENTER, format!("Sampler · {}", core.session.decks[d].sampler.name), bold(14.0), TEXT);
    } else {
        title_block(ui, core, d, title_r, false);
    }

}

// ---------------------------------------------------------------- sampler

fn sampler(ui: &mut Ui, core: &mut Core, d: usize, rect: Rect) {
    let n = d + 1;
    let sv = core.snap.samplers[d];
    let sm = core.session.decks[d].sampler.clone();
    let sel = sm.selected_lane;
    let color = DECK[d];
    let r = rect.shrink(16.0);

    let hh = 40.0;
    let badge = Rect::from_min_size(r.min, Vec2::splat(hh));
    badge_menu(ui, core, d, badge, 20.0);
    ui.painter().text(Pos2::new(badge.right() + 12.0, badge.center().y), Align2::LEFT_CENTER, format!("Sampler · {}", sm.name), bold(16.0), TEXT);
    let pr = Rect::from_min_size(Pos2::new(r.right() - 90.0, r.top()), Vec2::new(90.0, hh));
    let resp = button(ui, pr, if sv.playing { PAUSE } else { PLAY }, if sv.playing { Style::Fill(color) } else { Style::Outline(color, false) }, bold(15.0));
    learnable(core, &resp, &format!("Deck/{n}/Sampler/PlayPause"));
    if resp.clicked() {
        act_i(core, &format!("Deck/{n}/Sampler/PlayPause"), 1);
    }
    let mut x = pr.left() - 8.0;
    for (lbl, i) in [("2×", 2usize), ("1×", 1), ("½", 0)] {
        x -= 34.0;
        let on_ = [0.5, 1.0, 2.0][i] == sm.speed;
        if button(ui, Rect::from_min_size(Pos2::new(x, r.top() + 4.0), Vec2::new(32.0, hh - 8.0)), lbl, if on_ { Style::Light } else { Style::Dark }, bold(12.0)).clicked() {
            act_i(core, &format!("Deck/{n}/Sampler/Speed"), i as i32);
        }
    }
    x -= 10.0;
    for p in (0..8).rev() {
        x -= 26.0;
        let style = if sm.pattern == p { Style::Fill(color) } else { Style::Dark };
        if button(ui, Rect::from_min_size(Pos2::new(x, r.top() + 4.0), Vec2::new(24.0, hh - 8.0)), &format!("{}", p + 1), style, bold(12.0))
            .on_hover_text("Pattern")
            .clicked()
        {
            act_i(core, &format!("Deck/{n}/Sampler/Pattern"), p as i32);
        }
    }

    let grid_top = r.top() + hh + 12.0;
    let ctl_h = 54.0;
    let grid_h = r.bottom() - ctl_h - 10.0 - grid_top;
    let row_h = (grid_h / LANES as f32).floor();
    let name_w = 110.0;
    let step_w = (r.width() - name_w - 6.0) / STEPS as f32;
    let pattern = sm.patterns[sm.pattern];
    for lane in 0..LANES {
        let y = grid_top + lane as f32 * row_h;
        let lp = sm.lanes[lane].params;
        let nr = Rect::from_min_size(Pos2::new(r.left(), y), Vec2::new(name_w, row_h - 3.0));
        let name = sm.lanes[lane]
            .sample
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().chars().take(14).collect::<String>())
            .unwrap_or_else(|| format!("Lane {}", lane + 1));
        let style = if lp.mute {
            Style::Empty
        } else if sel == lane {
            Style::Fill(dim(color, 0.6))
        } else {
            Style::Fill(lighten(CTRL, sv.lane_peak[lane].min(1.0) * 0.35))
        };
        let resp = button(ui, nr, &name, style, sans(11.5)).on_hover_text("Click: select · Ctrl-click: mute · Right-click: trigger");
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
            let sr = Rect::from_min_size(Pos2::new(r.left() + name_w + 6.0 + s as f32 * step_w, y), Vec2::new(step_w - 3.0, row_h - 3.0));
            let step = pattern[lane][s];
            let active = s < lp.steps as usize;
            let resp = ui.allocate_rect(sr, Sense::click());
            let p = ui.painter();
            let fill = if step.on {
                lighten(dim(color, 0.45 + 0.55 * step.velocity), 0.0)
            } else if active {
                if s % 4 == 0 {
                    Color32::from_gray(48)
                } else {
                    Color32::from_gray(36)
                }
            } else {
                Color32::from_gray(22)
            };
            p.rect_filled(sr, CornerRadius::same(3), fill);
            if sv.playing && sv.current_step[lane] as usize == s {
                p.rect_stroke(sr, CornerRadius::same(3), Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Inside);
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

    // Lane controls as mini sliders.
    let lp = sm.lanes[sel].params;
    let cy = r.bottom() - ctl_h + 6.0;
    let items: [(&str, f32, f32); 8] = [
        ("Attack", lp.attack, 0.0),
        ("Hold", lp.hold, 1.0),
        ("Release", lp.release, 0.1),
        ("Curve", lp.curve, 0.5),
        ("Volume", lp.volume, 0.75),
        ("Pitch", (lp.pitch + 24.0) / 48.0, 0.5),
        ("Swing", sm.swing, 0.0),
        ("Steps", (lp.steps as f32 - 1.0) / 15.0, 1.0),
    ];
    let cw = (r.width() * 0.68 - 7.0 * 10.0) / 8.0;
    let mut x = r.left();
    for (label, val, def) in items {
        let rr = Rect::from_min_size(Pos2::new(x, cy), Vec2::new(cw, 32.0));
        x += cw + 10.0;
        let mut vv = val;
        let (resp, _) = mini_slider(ui, rr, &mut vv, def, label, false, color);
        if resp.changed() {
            match label {
                "Pitch" => core.set_lane(d, sel, |p| p.pitch = (vv * 48.0 - 24.0).round()),
                "Steps" => core.set_lane(d, sel, |p| p.steps = (vv * 15.0 + 1.0).round() as u8),
                "Swing" => {
                    act_f(core, &format!("Deck/{n}/Sampler/Swing"), vv);
                }
                l => {
                    core.route(&format!("Deck/{n}/Sampler/{l}"), &[rosc::OscType::Float(vv)]);
                }
            }
        }
    }
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
    let bw = 40.0;
    for (i, (label, what)) in [(dir, "Direction"), (rh, "Rhythm"), (if lp.mono { "Mono" } else { "Poly" }, "Mono"), ("Clear", "Clear")].iter().enumerate() {
        let rr = Rect::from_min_size(Pos2::new(x + i as f32 * (bw + 16.0), cy), Vec2::new(bw + 12.0, 30.0));
        if button(ui, rr, label, Style::Dark, bold(12.0)).clicked() {
            match *what {
                "Mono" => core.set_lane(d, sel, |p| p.mono = !p.mono),
                "Clear" => core.clear_sample(d, sel),
                w => laneset(core, w),
            }
        }
    }
    let steps_txt = format!("Lane {} · {} steps", sel + 1, lp.steps);
    ui.painter().text(Pos2::new(r.right(), cy + 44.0), Align2::RIGHT_CENTER, steps_txt, sans(11.0), TEXT_DIM);
}

/// Whether deck `d` shows the Mix button: it can be mixed in now, or its mix is under way.
fn mix_shown(core: &Core, d: usize) -> bool {
    core.automix.as_ref().is_some_and(|m| m.plan.incoming == d) || core.mix_target(d).is_some() || core.fader_mix_waiting_out() == Some(d)
}

/// What Now and Best would do for a deck, with the remembered style (shown in the Mix popup).
pub struct MixPreview {
    deck: usize,
    style: MixStyle,
    at: std::time::Instant,
    now: MixPlan,
    best: MixPlan,
}

/// The Mix popup's choice of how, and what each means.
const MIX_STYLES: [(&str, MixStyle, &str); 4] = [
    ("Auto", MixStyle::Auto, "dj measures both tracks and picks Stems, EQ or Drop"),
    ("Blend", MixStyle::Stems, "Always a blend: the stems swap one by one, or an EQ blend without stems"),
    ("Drop", MixStyle::Drop, "No blend: the new track cuts in on the beat, from its drop cue if it has one"),
    ("Mashup", MixStyle::Mashup, "The new track's vocals over this track's music (its own vocals out) for 8 to 32 bars, then the new music comes in under them. Needs stems on both decks and keys that fit; otherwise as Auto"),
];

/// Mix with the faders: no Mix button. A waiting deck shows the style its fader mixes it in
/// with (a tap: the next one); a deck its fader started shows the mix; the playing deck, while
/// the mix waits for it, that its fader takes it out.
fn fader_mix_slot(ui: &mut Ui, core: &mut Core, d: usize, rect: Rect, font: egui::FontId) {
    if let Some(m) = core.automix.as_ref().filter(|m| m.plan.incoming == d) {
        let bars = ((m.plan.start_beat - core.mix_beat()) / 4.0).ceil();
        let label = if bars > 0.0 { format!("Mix in {bars:.0}") } else { "Mixing…".to_string() };
        button(ui, rect, &label, Style::Fill(GREEN), font)
            .on_hover_text("Its fader started it and is its volume; its stems or EQ bring it in over the mix. Fader to the bottom before it begins: called off");
        return;
    }
    if core.fader_mix_waiting_out() == Some(d) {
        let label = if rect.width() < 120.0 { "Fader ↓" } else { "Fader ↓: out" };
        button(ui, rect, label, Style::Outline(GREEN, true), font)
            .on_hover_text("Start this deck's fader down: its stems or EQ take the track out over 4 bars, then take the fader to the bottom (until then it plays on in full)");
        return;
    }
    let style = core.settings.mix_style;
    let i = MIX_STYLES.iter().position(|s| s.1 == style).unwrap_or(0);
    let (name, _, tip) = MIX_STYLES[i];
    let resp = button(ui, rect, name, Style::Outline(GREEN, true), font)
        .on_hover_text(format!("Push this deck's fader up: dj starts it on the next phrase line and mixes it in, as {name} ({tip}). Tap for the next style"));
    if resp.clicked() && !core.learn {
        core.set_mix_style(MIX_STYLES[(i + 1) % MIX_STYLES.len()].1);
    }
}

/// "in 3 bars · Stems · 16 bars · keys clash": when a plan starts and how it mixes.
pub(crate) fn plan_summary(core: &Core, plan: &MixPlan) -> String {
    let bars = ((plan.start_beat - core.snap.decks[plan.outgoing].beat) / 4.0).ceil().max(0.0);
    let mut parts = vec![if bars <= 1.0 { "on the next bar".to_string() } else { format!("in {bars:.0} bars") }, plan.how.name(core.external_routing()).to_string()];
    if plan.how != MixHow::Drop {
        parts.push(format!("{:.0} bars", plan.length_beats / 4.0));
    }
    if !plan.why_how.is_empty() {
        parts.push(plan.why_how.clone());
    }
    parts.join(" · ")
}

/// Mix: bring this deck (silent, looping at a cue) into the playing track. Asks "Now" or
/// "Best", each with what it would do, and how (Auto, Blend or Drop, remembered); in the
/// playing track's last minute it doesn't ask. While a mix waits or runs, shows its state and a
/// tap cancels it.
fn mix_button(ui: &mut Ui, core: &mut Core, st: &mut UiState, d: usize, rect: Rect, font: egui::FontId) {
    if core.fader_mix_on() {
        fader_mix_slot(ui, core, d, rect, font);
        return;
    }
    if let Some(m) = core.automix.as_ref().filter(|m| m.plan.incoming == d) {
        let bars = ((m.plan.start_beat - core.mix_beat()) / 4.0).ceil();
        let label = if bars > 0.0 { format!("Mix in {bars:.0}") } else { "Mixing…".to_string() };
        let resp = button(ui, rect, &label, Style::Fill(GREEN), font).on_hover_text("Tap to cancel");
        if resp.clicked() && !core.learn {
            core.tap_cancel_mix();
        }
        return;
    }
    let Some(out) = core.mix_target(d) else { return };
    let style = core.settings.mix_style;
    let how = MIX_STYLES.iter().find(|s| s.1 == style).map_or("Auto", |s| s.0);
    let label = if style == MixStyle::Auto { "Mix".to_string() } else { format!("Mix · {how}") };
    let resp = button(ui, rect, &label, Style::Outline(GREEN, true), font).on_hover_text(if core.external_routing() {
        format!("Drop in, in sync with deck {}; you fade on your mixer", out + 1)
    } else {
        format!("Mix into deck {} (syncs this deck)", out + 1)
    });
    if core.mix_is_urgent(out) {
        // The playing track's last minute: no question, the best moment left (no rush: the mix
        // may run past its end, which fades on its own).
        if resp.clicked() && !core.learn {
            let plan = core.plan_best(d, out, style);
            core.start_mix(plan);
        }
        return;
    }
    const W: f32 = 300.0;
    egui::Popup::from_toggle_button_response(&resp)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .width(W)
        .show(|ui| {
            let style = core.settings.mix_style;
            // What each would do (a Best plan searches the whole track: once a second).
            let stale = st.mix_preview.as_ref().is_none_or(|p| p.deck != d || p.style != style || p.at.elapsed().as_secs_f32() > 1.0);
            if stale {
                st.mix_preview = Some(MixPreview { deck: d, style, at: std::time::Instant::now(), now: core.plan_now(d, out, style), best: core.plan_best(d, out, style) });
            }
            let preview = st.mix_preview.as_ref().unwrap();
            let rows = [("Now", plan_summary(core, &preview.now)), ("Best", plan_summary(core, &preview.best))];
            for (title, summary) in rows {
                let (r, _) = ui.allocate_exact_size(Vec2::new(W, 60.0), Sense::hover());
                if button(ui, r, "", Style::Dark, bold(20.0)).clicked() {
                    let plan = if title == "Now" { core.plan_now(d, out, style) } else { core.plan_best(d, out, style) };
                    core.start_mix(plan);
                    ui.close();
                }
                let p = ui.painter();
                p.text(Pos2::new(r.left() + 14.0, r.center().y - 9.0), Align2::LEFT_CENTER, title, bold(20.0), TEXT);
                let summary = fit_text(ui, &summary, sans(12.0), W - 28.0);
                ui.painter().text(Pos2::new(r.left() + 14.0, r.center().y + 13.0), Align2::LEFT_CENTER, summary, sans(12.0), TEXT_DIM);
                ui.add_space(6.0);
            }
            // How: remembered, for Auto-DJ too.
            let (row, _) = ui.allocate_exact_size(Vec2::new(W, 30.0), Sense::hover());
            let n = MIX_STYLES.len() as f32;
            let w = (W - (n - 1.0) * 6.0) / n;
            for (i, (name, s, tip)) in MIX_STYLES.into_iter().enumerate() {
                let r = Rect::from_min_size(Pos2::new(row.left() + i as f32 * (w + 6.0), row.top()), Vec2::new(w, 30.0));
                if pill(ui, r, name, style == s).on_hover_text(tip).clicked() {
                    core.set_mix_style(s);
                }
            }
        });
}

/// Where a deck's tempo sits on its fader: a synced deck that follows the clock shows its actual
/// speed (its own fader isn't what it plays at).
fn shown_fader(v: &dj_engine::deck::DeckView, follower: bool) -> f64 {
    if follower {
        v.tempo_range.fader_for_speed(v.speed / v.speed_mult.max(0.01))
    } else {
        v.tempo_fader
    }
}
