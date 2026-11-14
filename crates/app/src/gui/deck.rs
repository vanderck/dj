//! Deck view: header, waveforms, transport, tempo, hot cues, loops; or the
//! sampler step grid when the deck holds a sampler.

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};

use collective_engine::deck::{Quantize, TempoRange};
use collective_engine::engine::{ClockOwner, DeckMode};
use collective_engine::sampler::{Direction, Rhythm, Step, LANES, STEPS};
use collective_library::key::Compat;

use super::widgets::*;
use super::{act, act_f, act_i, learnable, UiState};
use crate::core::{Core, SLIP_LOOPS};

pub fn show(ui: &mut Ui, core: &mut Core, st: &mut UiState, d: usize) {
    let color = DECK_COLORS[d];
    let frame = egui::Frame::group(ui.style()).fill(ui.visuals().panel_fill).inner_margin(6.0);
    frame.show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{}", d + 1)).strong().size(18.0).color(color));
            let mode = core.session.decks[d].mode;
            if ui.selectable_label(mode == DeckMode::Track, "TRACK").clicked() && mode != DeckMode::Track {
                core.set_deck_mode(d, DeckMode::Track);
            }
            if ui.selectable_label(mode == DeckMode::Sampler, "SAMPLER").clicked() && mode != DeckMode::Sampler {
                core.set_deck_mode(d, DeckMode::Sampler);
            }
            ui.separator();
            if mode == DeckMode::Track {
                track_header(ui, core, d);
            } else {
                ui.label(RichText::new(core.session.decks[d].sampler.name.clone()).strong());
            }
        });
        if core.session.decks[d].mode == DeckMode::Sampler {
            sampler(ui, core, d, color);
        } else {
            track(ui, core, st, d, color);
        }
    });
}

fn track_header(ui: &mut Ui, core: &mut Core, d: usize) {
    let v = core.snap.decks[d];
    let (title, artist, energy, grid_src, locked) = match &core.decks[d].info {
        Some(i) => (i.title(), i.tags.artist.clone().unwrap_or_default(), i.tags.energy, i.grid_source, i.locked),
        None => {
            let t = if let Some(p) = &core.decks[d].loading {
                format!("Loading {}…", p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default())
            } else if let Some(e) = &core.decks[d].error {
                format!("⚠ {e}")
            } else {
                "Drop a track or double-click one in the browser".into()
            };
            (t, String::new(), None, None, false)
        }
    };
    ui.label(RichText::new(title).strong());
    if !artist.is_empty() {
        ui.label(RichText::new(artist).weak());
    }
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if !v.loaded {
            return;
        }
        if ui.small_button("⏏").on_hover_text("Eject").clicked() {
            act_i(core, &format!("Deck/{}/Eject", d + 1), 1);
        }
        let lock_txt = if locked { "🔒" } else { "🔓" };
        if ui.small_button(lock_txt).on_hover_text("Lock track: cues set in Perform are not saved").clicked() {
            core.set_track_locked(d, !locked);
        }
        ui.label(RichText::new(format!("-{}", fmt_time(v.duration_secs - v.position_secs))).monospace());
        ui.label(RichText::new(fmt_time(v.position_secs)).monospace().weak());
        // Key with harmonic hint relative to the reference deck.
        let key = core.deck_key(d);
        let reference = core.reference_key();
        let (kcol, hint) = match (key, reference) {
            (Some(k), Some(r)) if !(v.playing && core.snap.clock.owner == ClockOwner::Deck(d as u8)) => match k.compat(r) {
                Compat::Same | Compat::Adjacent | Compat::Relative => (OK, "harmonic match"),
                Compat::Boost => (ACCENT, "energy boost (+2)"),
                Compat::Diagonal => (WARN, "diagonal"),
                Compat::Clash => (DANGER, "key clash"),
            },
            _ => (ui.visuals().text_color(), ""),
        };
        let ktxt = core.key_text(key);
        if !ktxt.is_empty() {
            let r = ui.label(RichText::new(ktxt).strong().color(kcol));
            if !hint.is_empty() {
                r.on_hover_text(hint);
            }
        }
        if let Some(e) = energy {
            ui.label(RichText::new(format!("E{e}")).small()).on_hover_text("Mixed In Key energy level");
        }
        if v.has_grid {
            let src = match grid_src {
                Some(collective_library::GridSource::Tag) => "grid from tag",
                Some(collective_library::GridSource::FirstCue) => "grid anchored on first cue",
                Some(collective_library::GridSource::Aligned) => "grid aligned to kicks (tag BPM)",
                Some(collective_library::GridSource::User) => "grid edited",
                None => "",
            };
            ui.label(RichText::new(format!("{:.2}", v.bpm)).monospace().strong())
                .on_hover_text(format!("Track BPM {:.2} ({src}), speed {:+.2}%", v.track_bpm, (v.speed - 1.0) * 100.0));
        } else {
            ui.label(RichText::new("no BPM").weak()).on_hover_text("No BPM tag: sync and quantize are unavailable");
        }
    });
}

fn track(ui: &mut Ui, core: &mut Core, _st: &mut UiState, d: usize, color: Color32) {
    let v = core.snap.decks[d];
    let n = d + 1;
    let a = |s: &str| format!("Deck/{n}/{s}");

    // ---- waveforms
    let width = ui.available_width() - 70.0;
    let zoom = core.session.decks[d].zoom as f64;
    let span = 12.0 / zoom;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 78.0), Sense::click_and_drag());
            if let Some(info) = &core.decks[d].info {
                let wf = info.waveform.clone();
                WaveView { wf: &wf, color }.draw_scrolling(ui, rect, v.position_secs, span);
                draw_grid_and_cues(ui, core, d, rect, v.position_secs, span);
            } else {
                ui.painter().rect_filled(rect, CornerRadius::same(3), Color32::from_gray(14));
            }
            // Drag to scrub (vinyl-style) / scroll to zoom.
            if resp.dragged() && v.loaded && v.duration_secs > 0.0 {
                let dt = -resp.drag_delta().x as f64 / rect.width() as f64 * span;
                act_f(core, &a("Seek"), ((v.position_secs + dt) / v.duration_secs) as f32);
            }
            if resp.hovered() {
                let s = ui.input(|i| i.smooth_scroll_delta.y);
                if s != 0.0 {
                    act_i(core, &a("WaveformZoom"), if s > 0.0 { 1 } else { -1 });
                }
            }
            let p = ui.painter();
            p.line_segment([Pos2::new(rect.center().x, rect.top()), Pos2::new(rect.center().x, rect.bottom())], Stroke::new(2.0, Color32::WHITE));

            let (orect, oresp) = ui.allocate_exact_size(Vec2::new(width, 26.0), Sense::click_and_drag());
            let prog = if v.duration_secs > 0.0 { (v.position_secs / v.duration_secs) as f32 } else { 0.0 };
            if let Some(info) = &core.decks[d].info {
                let wf = info.waveform.clone();
                WaveView { wf: &wf, color }.draw_overview(ui, orect, prog);
                for (i, c) in v.hot_cues.iter().enumerate() {
                    if let Some(t) = c {
                        let x = orect.left() + (*t / v.duration_secs) as f32 * orect.width();
                        let col = info.cues[i].as_ref().map(|c| rgb(c.color)).unwrap_or(color);
                        ui.painter().line_segment([Pos2::new(x, orect.top()), Pos2::new(x, orect.bottom())], Stroke::new(1.5, col));
                    }
                }
                if v.loop_on {
                    let x0 = orect.left() + (v.loop_start_secs / v.duration_secs) as f32 * orect.width();
                    let x1 = orect.left() + (v.loop_end_secs / v.duration_secs) as f32 * orect.width();
                    ui.painter().rect_filled(
                        Rect::from_min_max(Pos2::new(x0, orect.top()), Pos2::new(x1.max(x0 + 1.0), orect.bottom())),
                        CornerRadius::ZERO,
                        Color32::from_rgba_unmultiplied(61, 214, 198, 60),
                    );
                }
            } else {
                ui.painter().rect_filled(orect, CornerRadius::same(2), Color32::from_gray(14));
            }
            if (oresp.clicked() || oresp.dragged()) && v.loaded {
                if let Some(pos) = oresp.interact_pointer_pos() {
                    act_f(core, &a("WaveformSeek"), ((pos.x - orect.left()) / orect.width()).clamp(0.0, 1.0));
                }
            }
        });

        // ---- tempo rail
        ui.vertical(|ui| {
            let follower = v.sync && core.snap.clock.owner != ClockOwner::Deck(d as u8);
            let mut t = ((v.tempo_fader as f32) + 1.0) / 2.0;
            let r = vfader(ui, &mut t, 82.0, None, if follower { Color32::from_gray(90) } else { color })
                .on_hover_text(format!("Tempo {:+.2}% ({})", (v.speed - 1.0) * 100.0, v.tempo_range.label()));
            learnable(core, &r, &a("TempoSlider"));
            if r.changed() && !follower {
                act_f(core, &a("TempoSlider"), (1.0 - t) * 2.0 - 1.0);
            }
            if r.double_clicked() {
                act_f(core, &a("TempoSlider"), 0.0);
            }
            let range = core.session.decks[d].tempo_range;
            egui::ComboBox::from_id_salt(("range", d)).width(46.0).selected_text(range.label()).show_ui(ui, |ui| {
                for (i, r) in [TempoRange::Pct6, TempoRange::Pct10, TempoRange::Pct16, TempoRange::Pct50, TempoRange::Semitones12]
                    .iter()
                    .enumerate()
                {
                    if ui.selectable_label(range == *r, r.label()).clicked() {
                        act_i(core, &a("TempoRange"), i as i32);
                    }
                }
            });
        });
    });

    // ---- transport row
    ui.horizontal_wrapped(|ui| {
        let play_txt = if v.play_pending { "WAIT" } else if v.playing { "PAUSE" } else { "PLAY" };
        let r = lit(ui, v.playing || v.play_pending, play_txt, OK);
        learnable(core, &r, &a("PlayPause"));
        if r.clicked() {
            act_i(core, &a("PlayPause"), 1);
        }
        let r = ui.add(egui::Button::new("CUE").sense(Sense::click_and_drag()));
        learnable(core, &r, &a("Cue"));
        if r.is_pointer_button_down_on() && !core.learn {
            if r.drag_started() || ui.input(|i| i.pointer.primary_pressed()) && r.hovered() {
                act_i(core, &a("Cue"), 1);
            }
        }
        if r.drag_stopped() || r.clicked() {
            act_i(core, &a("Cue"), 0);
        }
        let r = lit(ui, v.sync, "SYNC", ACCENT);
        learnable(core, &r, &a("TempoSync"));
        if r.clicked() {
            act_i(core, &a("TempoSync"), 1);
        }
        let master = core.snap.clock.owner == ClockOwner::Deck(d as u8);
        let r = lit(ui, master, if master { "MASTER" } else { "FOLLOW" }, WARN);
        learnable(core, &r, &a("TempoMaster"));
        if r.clicked() {
            act_i(core, &a("TempoMaster"), 1);
        }
        for (txt, addr) in [("◀◀", "PhaseMinus"), ("▶▶", "PhasePlus")] {
            let r = ui.add(egui::Button::new(txt).small().sense(Sense::click_and_drag())).on_hover_text("Phase early / late (hold)");
            learnable(core, &r, &a(addr));
            if r.is_pointer_button_down_on() && !core.learn {
                act_i(core, &a(addr), 1);
            } else if r.drag_stopped() || r.clicked() {
                act_i(core, &a(addr), 0);
            }
        }
        if v.phase_offset.abs() > 0.001 {
            let r = ui.small_button(format!("φ{:+.2}", v.phase_offset)).on_hover_text("Phase offset (beats). Click to reset.");
            if r.clicked() {
                act_i(core, &a("PhaseReset"), 1);
            }
        }
        ui.separator();
        let r = lit(ui, v.keylock, "P", ACCENT).on_hover_text("Pitch preserve (key lock)");
        learnable(core, &r, &a("PitchPreserve"));
        if r.clicked() {
            act_i(core, &a("PitchPreserve"), !v.keylock as i32);
        }
        if ui.small_button("−").on_hover_text("Transpose −1 semitone").clicked() {
            act_i(core, &a("PitchDown"), 1);
        }
        ui.label(RichText::new(format!("{:+.0}", v.semitones)).monospace());
        if ui.small_button("+").on_hover_text("Transpose +1 semitone").clicked() {
            act_i(core, &a("PitchUp"), 1);
        }
        ui.separator();
        let r = lit(ui, v.slip, "SLIP", ACCENT);
        learnable(core, &r, &a("Slip"));
        if r.clicked() {
            act_i(core, &a("Slip"), !v.slip as i32);
        }
        let r = lit(ui, v.reverse, "⟲", ACCENT).on_hover_text("Reverse (Shift-hold: backspin)");
        learnable(core, &r, &a("Reverse"));
        if r.clicked() {
            if ui.input(|i| i.modifiers.shift) {
                act_i(core, &a("Backspin"), 1);
                act_i(core, &a("Backspin"), 0);
            } else {
                act_i(core, &a("Reverse"), !v.reverse as i32);
            }
        }
        for (txt, m) in [("½", 0.5), ("×2", 2.0)] {
            if lit(ui, v.speed_mult == m, txt, ACCENT).clicked() {
                act_i(core, &a("Speed"), if v.speed_mult == m { 1 } else if m < 1.0 { 0 } else { 2 });
            }
        }
        let q = core.session.decks[d].quantize;
        let qtxt = match q {
            Quantize::UseGlobal => "Q:G".to_string(),
            Quantize::Off => "Q:–".into(),
            Quantize::Beats(b) => format!("Q:{}", fmt_beats(b)),
        };
        egui::ComboBox::from_id_salt(("q", d)).width(52.0).selected_text(qtxt).show_ui(ui, |ui| {
            let labels = ["Use global", "None", "2 bars", "1 bar", "1/2", "1/4", "1/8", "1/16", "1/32"];
            for (i, l) in labels.iter().enumerate() {
                if ui.selectable_label(Quantize::from_index(i as i32) == q, *l).clicked() {
                    act_i(core, &a("Quantize"), i as i32);
                }
            }
        });
        if ui.small_button("⊢").on_hover_text("Grid start here (saved for this track)").clicked() {
            act_i(core, &a("GridStartHere"), 1);
        }
        if ui.small_button("‹").on_hover_text("Nudge grid −5 ms").clicked() {
            act_f(core, &a("GridNudge"), -5.0);
        }
        if ui.small_button("›").on_hover_text("Nudge grid +5 ms").clicked() {
            act_f(core, &a("GridNudge"), 5.0);
        }
    });

    // ---- hot cues + loops
    ui.horizontal(|ui| {
        let momentary = v.momentary_cues;
        for i in 0..8 {
            let set = v.hot_cues[i].is_some();
            let info_cue = core.decks[d].info.as_ref().and_then(|x| x.cues[i].clone());
            let col = info_cue.as_ref().map(|c| rgb(c.color)).unwrap_or(crate::core::CUE_COLORS[i].map(|x| x).into_color());
            let label = match &info_cue {
                Some(c) if !c.name.is_empty() => format!("{} {}", i + 1, truncate(&c.name, 8)),
                _ if v.hot_cue_is_loop[i] => format!("∞{}", i + 1),
                _ => format!("{}", i + 1),
            };
            let fill = if set { col } else { ui.visuals().widgets.inactive.weak_bg_fill };
            let txt = if set { RichText::new(label).color(Color32::BLACK).strong() } else { RichText::new(label).weak() };
            let r = ui.add(egui::Button::new(txt).fill(fill).min_size(Vec2::new(44.0, 26.0)).sense(Sense::click_and_drag()));
            let r = r.on_hover_text("Click: set / launch. Ctrl-click: delete. Alt-click: loop to next cue.");
            let addr = a(&format!("Cue{}", i + 1));
            learnable(core, &r, &addr);
            if core.learn {
                continue;
            }
            let mods = ui.input(|i| i.modifiers);
            if r.clicked() && mods.ctrl {
                act_i(core, &a("DeleteCue"), i as i32 + 1);
            } else if r.clicked() && mods.alt {
                act_i(core, &a("SetCueLoop"), i as i32 + 1);
            } else if momentary && set {
                if r.drag_started() || (r.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed())) {
                    act(core, &a("HotCue"), &[rosc::OscType::Int(i as i32 + 1), rosc::OscType::Int(1)]);
                }
                if r.drag_stopped() || r.clicked() {
                    act(core, &a("HotCue"), &[rosc::OscType::Int(i as i32 + 1), rosc::OscType::Int(0)]);
                }
            } else if r.clicked() {
                act_i(core, &a("HotCue"), i as i32 + 1);
            }
        }
        if lit(ui, momentary, "M", ACCENT).on_hover_text("Momentary cues (play while held)").clicked() {
            act_i(core, &a("CueMomentary"), !momentary as i32);
        }
    });
    ui.horizontal_wrapped(|ui| {
        let r = lit(ui, v.loop_on, &format!("LOOP {}", if v.loop_on { fmt_beats(v.loop_beats) } else { String::new() }), ACCENT);
        learnable(core, &r, &a("Loop"));
        if r.clicked() {
            act_i(core, &a("Loop"), 1);
        }
        for (txt, addr) in [("IN", "LoopIn"), ("OUT", "LoopOut"), ("½", "LoopHalve"), ("×2", "LoopDouble")] {
            let r = ui.button(txt);
            learnable(core, &r, &a(addr));
            if r.clicked() {
                act_i(core, &a(addr), 1);
            }
        }
        for b in [1.0, 4.0, 8.0, 16.0, 32.0] {
            if ui.small_button(fmt_beats(b)).on_hover_text("Loop this many beats").clicked() {
                act_f(core, &a("LoopBeats"), b as f32);
            }
        }
        ui.separator();
        let skip = core.settings.skip_beats;
        for (txt, addr) in [(format!("«{skip}"), "SkipBeatBack"), (format!("{skip}»"), "SkipBeatForward")] {
            let r = ui.button(txt.as_str()).on_hover_text(format!("Skip {} beats", skip));
            learnable(core, &r, &a(addr));
            if r.clicked() {
                act_i(core, &a(addr), 1);
            }
        }
        ui.separator();
        ui.label(RichText::new("SLIP").small().weak());
        let dm = core.session.decks[d].clone();
        for (name, beats) in SLIP_LOOPS.iter().skip(1).take(6) {
            let addr = a(&format!("SlipLoop/{name}"));
            let r = ui.add(egui::Button::new(fmt_beats(*beats)).small().sense(Sense::click_and_drag()));
            learnable(core, &r, &addr);
            if core.learn {
                continue;
            }
            if r.is_pointer_button_down_on() {
                act_i(core, &addr, 1);
            } else if r.drag_stopped() || r.clicked() {
                act_i(core, &addr, 0);
            }
        }
        if lit(ui, dm.slip_dotted, "D", ACCENT).clicked() {
            act_i(core, &a("SlipDotted"), !dm.slip_dotted as i32);
        }
        if lit(ui, dm.slip_triplet, "T", ACCENT).clicked() {
            act_i(core, &a("SlipTriplet"), !dm.slip_triplet as i32);
        }
    });
}

trait IntoColor {
    fn into_color(self) -> Color32;
}
impl IntoColor for [u8; 3] {
    fn into_color(self) -> Color32 {
        rgb(self)
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        s.chars().take(n).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

pub fn fmt_beats(b: f64) -> String {
    if b >= 4.0 && (b / 4.0).fract() == 0.0 {
        format!("{}bar", b / 4.0)
    } else if b >= 1.0 {
        format!("{b}")
    } else {
        format!("1/{}", (1.0 / b).round())
    }
}

fn draw_grid_and_cues(ui: &Ui, core: &Core, d: usize, rect: Rect, pos: f64, span: f64) {
    let v = core.snap.decks[d];
    let p = ui.painter_at(rect);
    let x_of = |t: f64| rect.left() + ((t - (pos - span / 2.0)) / span) as f32 * rect.width();
    if v.has_grid && v.track_bpm > 0.0 {
        let spb = 60.0 / v.track_bpm;
        let anchor = v.position_secs - v.beat * spb;
        let first = ((pos - span / 2.0 - anchor) / spb).floor() as i64;
        let last = ((pos + span / 2.0 - anchor) / spb).ceil() as i64;
        for b in first..=last {
            let t = anchor + b as f64 * spb;
            let x = x_of(t);
            let bar = b.rem_euclid(4) == 0;
            let (h, a) = if bar { (rect.height(), 120) } else { (10.0, 70) };
            p.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.top() + h)], Stroke::new(1.0, Color32::from_white_alpha(a)));
        }
    }
    if v.loop_on {
        let r = Rect::from_min_max(Pos2::new(x_of(v.loop_start_secs), rect.top()), Pos2::new(x_of(v.loop_end_secs), rect.bottom()));
        p.rect_filled(r, CornerRadius::ZERO, Color32::from_rgba_unmultiplied(61, 214, 198, 40));
    }
    if let Some(info) = &core.decks[d].info {
        for (i, c) in v.hot_cues.iter().enumerate() {
            if let Some(t) = c {
                let x = x_of(*t);
                let col = info.cues[i].as_ref().map(|c| rgb(c.color)).unwrap_or(ACCENT);
                p.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], Stroke::new(2.0, col));
                p.text(Pos2::new(x + 3.0, rect.top() + 2.0), egui::Align2::LEFT_TOP, format!("{}", i + 1), egui::FontId::monospace(10.0), col);
            }
        }
    }
}

// ---------------------------------------------------------------- sampler deck

fn sampler(ui: &mut Ui, core: &mut Core, d: usize, color: Color32) {
    let sv = core.snap.samplers[d];
    let n = d + 1;
    let sm = core.session.decks[d].sampler.clone();
    let sel = sm.selected_lane;
    ui.horizontal(|ui| {
        let r = lit(ui, sv.playing, if sv.playing { "PAUSE" } else { "PLAY" }, OK);
        learnable(core, &r, &format!("Deck/{n}/Sampler/PlayPause"));
        if r.clicked() {
            act_i(core, &format!("Deck/{n}/Sampler/PlayPause"), 1);
        }
        ui.label("Pattern");
        for p in 0..8 {
            if ui.selectable_label(sm.pattern == p, format!("{}", p + 1)).clicked() {
                act_i(core, &format!("Deck/{n}/Sampler/Pattern"), p as i32);
            }
        }
        ui.separator();
        let mut swing = sm.swing;
        if knob(ui, &mut swing, 0.0, 1.0, 0.0, "SWING", color).changed() {
            act_f(core, &format!("Deck/{n}/Sampler/Swing"), swing);
        }
        let mut gp = sm.global_pitch;
        if knob(ui, &mut gp, -12.0, 12.0, 0.0, "PITCH", color).changed() {
            act_f(core, &format!("Deck/{n}/Sampler/GlobalPitch"), gp.round());
        }
        for (txt, i) in [("½", 0), ("1×", 1), ("2×", 2)] {
            let on = [0.5, 1.0, 2.0][i] == sm.speed;
            if lit(ui, on, txt, ACCENT).clicked() {
                act_i(core, &format!("Deck/{n}/Sampler/Speed"), i as i32);
            }
        }
    });
    // Step grid.
    let pattern = core.session.decks[d].sampler.patterns[sm.pattern];
    for lane in 0..LANES {
        ui.horizontal(|ui| {
            let lp = sm.lanes[lane].params;
            let name = sm.lanes[lane]
                .sample
                .as_ref()
                .and_then(|p| p.file_stem())
                .map(|s| truncate(&s.to_string_lossy(), 10))
                .unwrap_or_else(|| format!("Lane {}", lane + 1));
            let peak = sv.lane_peak[lane];
            let r = ui.add(
                egui::Button::new(RichText::new(name).small().color(if lp.mute { Color32::from_gray(90) } else { ui.visuals().text_color() }))
                    .min_size(Vec2::new(90.0, 18.0))
                    .fill(if sel == lane { dim(color, 0.5) } else { dim(color, 0.15 + peak.min(1.0) * 0.5) }),
            );
            if r.clicked() {
                if ui.input(|i| i.modifiers.ctrl) {
                    core.set_lane(d, lane, |p| p.mute = !p.mute);
                } else {
                    core.session.decks[d].sampler.selected_lane = lane;
                }
            }
            if r.secondary_clicked() {
                core.route(&format!("Deck/{n}/Sampler/Trigger"), &[rosc::OscType::Int(lane as i32 + 1)]);
            }
            for s in 0..STEPS {
                let step = pattern[lane][s];
                let active = s < lp.steps as usize;
                let playing = sv.playing && sv.current_step[lane] as usize == s;
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(18.0, 18.0), Sense::click());
                let base = if s % 4 == 0 { Color32::from_gray(58) } else { Color32::from_gray(44) };
                let fill = if step.on {
                    dim(color, 0.4 + 0.6 * step.velocity)
                } else if active {
                    base
                } else {
                    Color32::from_gray(26)
                };
                ui.painter().rect_filled(rect.shrink(1.0), CornerRadius::same(2), fill);
                if playing {
                    ui.painter().rect_stroke(rect.shrink(1.0), CornerRadius::same(2), Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Inside);
                }
                if resp.clicked() {
                    core.set_step(d, lane, s, Step { on: !step.on, velocity: step.velocity });
                }
                if resp.hovered() {
                    let sc = ui.input(|i| i.smooth_scroll_delta.y);
                    if sc != 0.0 && step.on {
                        let vel = (step.velocity + sc / 400.0).clamp(0.05, 1.0);
                        core.set_step(d, lane, s, Step { on: true, velocity: vel });
                    }
                }
            }
            if sv.sample_progress[lane] >= 0.0 {
                ui.label(RichText::new("●").color(color).small());
            }
        });
    }
    // Selected lane controls.
    let lp = sm.lanes[sel].params;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(format!("Lane {}", sel + 1)).strong());
        if ui.button("Load sample…").clicked() {
            if let Some(p) = core.browser_selected.and_then(|i| core.library.get(i)).map(|e| e.path.clone()) {
                core.load_sample(d, sel, &p);
            } else {
                core.log("Select a file in the browser, then press Load sample");
            }
        }
        if ui.button("Clear").clicked() {
            core.clear_sample(d, sel);
        }
        let mut vals = [lp.attack, lp.hold, lp.release, lp.curve, lp.volume];
        let names = ["ATTACK", "HOLD", "RELEASE", "CURVE", "VOLUME"];
        let defaults = [0.0, 1.0, 0.1, 0.5, 0.75];
        for i in 0..5 {
            if knob(ui, &mut vals[i], 0.0, 1.0, defaults[i], names[i], color).changed() {
                core.route(&format!("Deck/{n}/Sampler/{}", names[i]), &[rosc::OscType::Float(vals[i])]);
            }
        }
        let mut pitch = lp.pitch;
        if knob(ui, &mut pitch, -24.0, 24.0, 0.0, "PITCH", color).changed() {
            core.set_lane(d, sel, |p| p.pitch = if ui.input(|i| i.modifiers.shift) { pitch } else { pitch.round() });
        }
        let mut steps = lp.steps as f32;
        if knob(ui, &mut steps, 1.0, 16.0, 16.0, "STEPS", color).changed() {
            core.set_lane(d, sel, |p| p.steps = steps.round() as u8);
        }
        let dir = match lp.direction {
            Direction::Forward => "→",
            Direction::Reverse => "←",
            Direction::PingPong => "↔",
            Direction::Random => "?",
        };
        if ui.button(dir).on_hover_text("Direction").clicked() {
            core.route(&format!("Deck/{n}/Sampler/LaneSet"), &[rosc::OscType::Int(sel as i32 + 1), rosc::OscType::String("Direction".into()), rosc::OscType::Float(1.0)]);
        }
        let rh = match lp.rhythm {
            Rhythm::Normal => "N",
            Rhythm::Dotted => "D",
            Rhythm::Triplet => "T",
        };
        if ui.button(rh).on_hover_text("Rate: normal / dotted / triplet sixteenths").clicked() {
            core.route(&format!("Deck/{n}/Sampler/LaneSet"), &[rosc::OscType::Int(sel as i32 + 1), rosc::OscType::String("Rhythm".into()), rosc::OscType::Float(1.0)]);
        }
        if ui.selectable_label(lp.mono, if lp.mono { "MONO" } else { "POLY" }).clicked() {
            core.set_lane(d, sel, |p| p.mono = !p.mono);
        }
    });
    let _ = Rect::NOTHING;
}
