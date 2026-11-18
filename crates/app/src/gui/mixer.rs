//! Centre mixer: tempo bar, the fader-function toggles, vertical channel strips.

use std::time::Instant;

use eframe::egui::{self, Align2, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, Ui, UiBuilder, Vec2};

use collective_engine::dsp::gain_to_db;
use collective_engine::engine::{ClockOwner, NUM_DECKS};
use collective_engine::fx::SlotHealth;
use collective_engine::mixer::Output;

use super::skin::*;
use super::{act_f, act_i, learnable, UiState};
use crate::core::Core;
use crate::model::{MasterModel, QUANTIZE_VALUES};

fn db_text(g: f32) -> String {
    if g <= 0.0 {
        return "-∞".into();
    }
    let db = gain_to_db(g);
    if db.abs() >= 10.0 {
        format!("{db:.0}")
    } else {
        format!("{db:.1}")
    }
}

pub fn show(ui: &mut Ui, core: &mut Core, st: &mut UiState, rect: Rect) {
    card(ui, rect);
    let r = rect.shrink(16.0);
    if core.external_routing() {
        return external_panel(ui, core, st, r);
    }
    top_bar(ui, core, st, Rect::from_min_size(r.min, Vec2::new(r.width(), 38.0)), false);
    mode_row(ui, st, Rect::from_min_size(Pos2::new(r.left(), r.top() + 50.0), Vec2::new(r.width(), 70.0)));

    let strips = Rect::from_min_max(Pos2::new(r.left(), r.top() + 132.0), r.right_bottom());
    let order: Vec<Option<usize>> = if core.deck_count() >= 4 { vec![Some(2), Some(0), None, Some(1), Some(3)] } else { vec![Some(0), None, Some(1)] };
    let gap = 10.0;
    let sw = (strips.width() - gap * (order.len() as f32 - 1.0)) / order.len() as f32;
    for (i, ch) in order.iter().enumerate() {
        let sr = Rect::from_min_size(Pos2::new(strips.left() + i as f32 * (sw + gap), strips.top()), Vec2::new(sw, strips.height()));
        match ch {
            Some(c) => strip(ui, core, st, *c, sr),
            None => master_strip(ui, core, sr),
        }
    }
}

/// BPM, beat dots, Tap, Quantize, Rec and Menu. `stacked` lays them out in three short rows for
/// the slim column used in external-mixer mode.
fn top_bar(ui: &mut Ui, core: &mut Core, st: &mut UiState, r: Rect, stacked: bool) {
    let s = core.snap;
    // BPM: drag to change, double-click to type.
    let bpm_r = Rect::from_min_size(r.min, Vec2::new(104.0, if stacked { 38.0 } else { r.height() }));
    if let Some(text) = &mut st.bpm_edit {
        let mut child = ui.new_child(UiBuilder::new().max_rect(bpm_r.shrink2(Vec2::new(0.0, 4.0))));
        let resp = child.add(egui::TextEdit::singleline(text).font(mono(20.0)).desired_width(100.0));
        if resp.lost_focus() {
            if child.input(|i| i.key_pressed(egui::Key::Enter)) {
                if let Ok(v) = text.trim().parse::<f32>() {
                    act_f(core, "Header/BPM", v);
                }
            }
            st.bpm_edit = None;
        } else {
            resp.request_focus();
        }
    } else {
        let resp = ui.allocate_rect(bpm_r, Sense::click_and_drag());
        let owner = match s.clock.owner {
            ClockOwner::Header => "the header".to_string(),
            ClockOwner::Deck(d) => format!("deck {}", d + 1),
        };
        let resp = resp.on_hover_text(format!("Master tempo (owned by {owner}). Drag (Shift: fine), double-click to type."));
        ui.painter().text(bpm_r.left_center(), Align2::LEFT_CENTER, format!("{:.2}", s.clock.bpm), mono(26.0), TEXT);
        learnable(core, &resp, "Header/BPM");
        if resp.dragged() {
            let fine = ui.input(|i| i.modifiers.shift);
            let dv = resp.drag_delta().x as f64 * if fine { 0.01 } else { 0.1 };
            if dv != 0.0 {
                act_f(core, "Header/BPM", (s.clock.bpm + dv) as f32);
            }
        }
        if resp.double_clicked() {
            st.bpm_edit = Some(format!("{:.2}", s.clock.bpm));
        }
    }
    // Beat dots.
    let beat = (s.clock.beat.floor() as i64).rem_euclid(4) as usize;
    for i in 0..4 {
        let c = Pos2::new(bpm_r.right() + 4.0 + i as f32 * 12.0, bpm_r.center().y);
        ui.painter().circle_filled(c, 4.5, if i == beat { RED } else { Color32::from_gray(70) });
    }

    let h = 30.0;
    let rec_label = match &core.recorder {
        Some(rec) => format!("● {}", super::widgets::fmt_time(rec.started.elapsed().as_secs_f64())),
        None => "● Rec".into(),
    };
    let q_label = format!("Quantize {}", QUANTIZE_VALUES[core.session.quantize].1);
    let (tap_r, q_r, rec_r, menu_r) = if stacked {
        // Two rows under the BPM: Tap · Quantize, then Rec · Menu.
        let y1 = bpm_r.bottom() + 8.0;
        let y2 = y1 + h + 8.0;
        let tap_w = pill_width(ui, "Tap", 12.0);
        let rec_w = (r.width() - 8.0) / 2.0;
        (
            Rect::from_min_size(Pos2::new(r.left(), y1), Vec2::new(tap_w, h)),
            Rect::from_min_max(Pos2::new(r.left() + tap_w + 8.0, y1), Pos2::new(r.right(), y1 + h)),
            Rect::from_min_size(Pos2::new(r.left(), y2), Vec2::new(rec_w, h)),
            Rect::from_min_max(Pos2::new(r.left() + rec_w + 8.0, y2), Pos2::new(r.right(), y2 + h)),
        )
    } else {
        // One row, from the right: Menu, Rec, Quantize, Tap.
        let y = r.center().y - h / 2.0;
        let mut x = r.right();
        let mut place = |ui: &Ui, label: &str| {
            let w = pill_width(ui, label, 12.0);
            x -= w;
            let rr = Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h));
            x -= 8.0;
            rr
        };
        let menu_r = place(ui, "Menu");
        let rec_r = place(ui, &rec_label);
        let q_r = place(ui, &q_label);
        let tap_r = place(ui, "Tap");
        (tap_r, q_r, rec_r, menu_r)
    };

    let resp = pill(ui, tap_r, "Tap", false).on_hover_text("Tap tempo");
    if resp.clicked() {
        let now = Instant::now();
        st.taps.retain(|t| now.duration_since(*t).as_secs_f32() < 2.5);
        st.taps.push(now);
        if st.taps.len() >= 3 {
            let n = st.taps.len();
            let span = st.taps[n - 1].duration_since(st.taps[0]).as_secs_f64();
            let bpm = 60.0 * (n - 1) as f64 / span;
            act_f(core, "Header/BPM", bpm as f32);
        }
    }
    let q_on = core.session.quantize_on;
    let resp = pill(ui, q_r, &q_label, q_on).on_hover_text("Click: on/off · right-click: value");
    learnable(core, &resp, "Header/QuantizeOn");
    if resp.clicked() {
        act_i(core, "Header/QuantizeOn", !q_on as i32);
    }
    resp.context_menu(|ui| {
        for (i, (_, l)) in QUANTIZE_VALUES.iter().enumerate() {
            if ui.selectable_label(core.session.quantize == i, format!("{l} bar")).clicked() {
                act_i(core, "Header/Quantize", i as i32 + 1);
                ui.close();
            }
        }
    });
    let rec = core.recorder.is_some();
    let resp = ui.allocate_rect(rec_r, Sense::click()).on_hover_text("Record the master output to WAV");
    let p = ui.painter();
    let rad = CornerRadius::same((h / 2.0) as u8);
    p.rect_filled(rec_r, rad, if rec { dim(RED, 0.8) } else if resp.hovered() { CTRL_HOVER } else { CTRL });
    let label = rec_label.trim_start_matches('●').trim().to_string();
    p.circle_filled(Pos2::new(rec_r.left() + 15.0, rec_r.center().y), 4.5, if rec { Color32::WHITE } else { RED });
    p.text(Pos2::new(rec_r.left() + 24.0, rec_r.center().y), Align2::LEFT_CENTER, label, bold(12.0), TEXT);
    learnable(core, &resp, "Header/Record");
    if resp.clicked() {
        core.toggle_recording();
    }
    let resp = pill(ui, menu_r, "Menu", false);
    egui::Popup::menu(&resp).show(|ui| menu(ui, core, st));
}

/// External-mixer mode: no channel strips, just the clock controls and where each deck goes.
fn external_panel(ui: &mut Ui, core: &mut Core, st: &mut UiState, r: Rect) {
    top_bar(ui, core, st, Rect::from_min_size(r.min, Vec2::new(r.width(), 114.0)), true);
    let mut y = r.top() + 136.0;
    let p = ui.painter();
    p.text(Pos2::new(r.left(), y), Align2::LEFT_TOP, "OUTPUTS", mono(11.0), TEXT_DIM);
    y += 22.0;
    for d in 0..core.deck_count() {
        let pair = core.external_pair(d);
        let row = Rect::from_min_size(Pos2::new(r.left(), y), Vec2::new(r.width(), 34.0));
        p.rect_filled(row, CornerRadius::same(8), WELL);
        p.circle_filled(Pos2::new(row.left() + 16.0, row.center().y), 6.0, DECK[d]);
        p.text(Pos2::new(row.left() + 30.0, row.center().y), Align2::LEFT_CENTER, format!("Deck {}", d + 1), bold(13.0), TEXT);
        p.text(Pos2::new(row.right() - 12.0, row.center().y), Align2::RIGHT_CENTER, format!("Out {}/{}", pair * 2 + 1, pair * 2 + 2), mono(13.0), TEXT);
        y += 40.0;
    }
    y += 6.0;
    let note = format!(
        "{} outputs on the audio device. Volume, EQ and filter happen on your mixer; Collective sends each deck unprocessed.",
        core.audio_channels
    );
    let galley = p.layout(note, sans(11.5), TEXT_DIM, r.width());
    let gh = galley.size().y;
    p.galley(Pos2::new(r.left(), y), galley, TEXT_DIM);
    y += gh + 14.0;
    let br = Rect::from_min_size(Pos2::new(r.left(), y), Vec2::new(r.width(), 32.0));
    if button(ui, br, "Use internal mixer", Style::Outline(Color32::from_gray(110), false), bold(12.5)).clicked() {
        core.set_external_mixer(false);
    }
}

fn menu(ui: &mut Ui, core: &mut Core, st: &mut UiState) {
    if ui.button("Settings…").clicked() {
        st.settings_open = true;
        ui.close();
    }
    if ui.button("MIDI & keyboard mapping…").clicked() {
        st.mapping_open = true;
        ui.close();
    }
    let ext = core.settings.external_mixer;
    if ui
        .selectable_label(ext, "External mixer")
        .on_hover_text("Each deck to its own output, mixer section hidden (needs a device with 4+ outputs)")
        .clicked()
    {
        core.set_external_mixer(!ext);
        ui.close();
    }
    let learn = core.learn;
    if ui.selectable_label(learn, "MIDI learn").clicked() {
        core.learn = !learn;
        core.last_touched = None;
        ui.close();
    }
    ui.separator();
    let met = core.session.master.metronome;
    if ui.selectable_label(met, "Metronome").clicked() {
        act_i(core, "Header/Metronome", !met as i32);
    }
    let header_owns = core.snap.clock.owner == ClockOwner::Header;
    if ui.selectable_label(header_owns, "Header owns the clock").on_hover_text("Click to hand the clock to a playing synced deck / take it back").clicked() {
        act_i(core, "Header/Master", 1);
    }
    let wc = core.session.wallclock;
    if ui.selectable_label(wc, "Wallclock (tempo stays put)").clicked() {
        act_i(core, "Header/Wallclock", !wc as i32);
    }
    ui.separator();
    ui.horizontal(|ui| {
        ui.label("Decks");
        for n in [2usize, 4] {
            if ui.selectable_label(core.settings.deck_count == n, n.to_string()).clicked() {
                core.settings.deck_count = n;
                st.focus = [0, 1];
            }
        }
    });
    ui.menu_button("Insert FX ▸", |ui| {
        for c in 0..core.deck_count() {
            if ui.button(format!("Channel {}", c + 1)).clicked() {
                st.fx_open[c] = true;
                ui.close();
            }
        }
    });
}

fn strip_frame(ui: &Ui, rect: Rect, color: Color32, dashed: bool) {
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(10), Color32::from_rgb(0x15, 0x15, 0x19));
    if dashed {
        // Draw a dashed rounded outline (straight edges only).
        let s = Stroke::new(1.0, Color32::from_gray(70));
        for (a, b) in [
            (rect.left_top() + Vec2::new(10.0, 0.0), rect.right_top() - Vec2::new(10.0, 0.0)),
            (rect.left_bottom() + Vec2::new(10.0, 0.0), rect.right_bottom() - Vec2::new(10.0, 0.0)),
            (rect.left_top() + Vec2::new(0.0, 10.0), rect.left_bottom() - Vec2::new(0.0, 10.0)),
            (rect.right_top() + Vec2::new(0.0, 10.0), rect.right_bottom() - Vec2::new(0.0, 10.0)),
        ] {
            p.extend(egui::Shape::dashed_line(&[a, b], s, 5.0, 4.0));
        }
    } else {
        p.rect_stroke(rect, CornerRadius::same(10), Stroke::new(1.5, color), egui::StrokeKind::Inside);
    }
}

/// Vertical fader inside `track`. Returns the drag response.
/// Vertical fader. `centred` draws a detent at 0.5 and fills from it (EQ / filter).
fn vfader(ui: &mut Ui, track: Rect, value: f32, color: Color32, meter: f32, centred: bool) -> (egui::Response, Option<f32>) {
    let resp = ui.allocate_rect(track, Sense::click_and_drag());
    let p = ui.painter();
    p.rect_filled(track, CornerRadius::same(8), WELL);
    let inner = track.shrink2(Vec2::new(0.0, 10.0));
    let y = inner.bottom() - value.clamp(0.0, 1.0) * inner.height();
    if centred {
        let cy = inner.center().y;
        p.rect_filled(Rect::from_min_max(Pos2::new(track.left() + 3.0, y.min(cy)), Pos2::new(track.right() - 3.0, y.max(cy))), CornerRadius::same(3), alpha(color, 0.28));
        p.line_segment([Pos2::new(track.left() + 4.0, cy), Pos2::new(track.right() - 4.0, cy)], Stroke::new(1.0, Color32::from_gray(90)));
    } else {
        p.rect_filled(Rect::from_min_max(Pos2::new(track.left(), y), track.right_bottom()), CornerRadius::same(8), alpha(color, 0.28));
    }
    // Live meter along the right edge.
    if meter > 0.0 {
        let db = 20.0 * meter.max(1e-5).log10();
        let t = ((db + 48.0) / 51.0).clamp(0.0, 1.0);
        let my = inner.bottom() - t * inner.height();
        p.rect_filled(
            Rect::from_min_max(Pos2::new(track.right() - 5.0, my), Pos2::new(track.right() - 2.0, inner.bottom())),
            CornerRadius::same(1),
            if db > -0.5 { RED } else { lighten(color, 0.2) },
        );
    }
    let hw = track.width() * 0.72;
    let handle = Rect::from_center_size(Pos2::new(track.center().x, y), Vec2::new(hw, 18.0));
    p.rect_filled(handle, CornerRadius::same(5), color);
    p.line_segment([Pos2::new(handle.left() + 10.0, y), Pos2::new(handle.right() - 10.0, y)], Stroke::new(2.0, on(color)));
    let mut new = None;
    if resp.dragged() {
        let fine = ui.input(|i| i.modifiers.shift);
        new = Some((value - resp.drag_delta().y / inner.height() * if fine { 0.2 } else { 1.0 }).clamp(0.0, 1.0));
    }
    (resp, new)
}

fn strip(ui: &mut Ui, core: &mut Core, st: &mut UiState, ch: usize, rect: Rect) {
    let n = ch + 1;
    let a = |s: &str| format!("Mixer/CH{n}/{s}");
    let cm = core.session.channels[ch].clone();
    let p = cm.params;
    let view = core.snap.channels[ch];
    let color = DECK[ch];
    let loaded = core.snap.decks[ch].loaded || core.session.decks[ch].mode == collective_engine::engine::DeckMode::Sampler;
    strip_frame(ui, rect, color, !loaded);
    let r = rect.shrink2(Vec2::new(8.0, 12.0));
    let title_r = Rect::from_min_size(r.min, Vec2::new(r.width(), 16.0));
    let tr = ui.allocate_rect(title_r, Sense::click()).on_hover_text("Right-click: effects, mute, solo, trim, output…");
    ui.painter().text(title_r.center(), Align2::CENTER_CENTER, if p.mute { format!("CH {n} · MUTE") } else { format!("CH {n}") }, mono(12.0), if p.mute { RED } else { TEXT_DIM });
    // Status dots: EQ/filter away from neutral, effects running.
    let eq_moved = p.eq.iter().any(|v| (v - 0.5).abs() > 0.01) || (p.cutoff - 0.5).abs() > 0.01;
    let fx_on = cm.fx.slots.iter().any(|s| s.is_some()) && !cm.fx.bypass_all;
    for (i, (show, col)) in [(eq_moved, LIGHT), (fx_on, GREEN)].into_iter().enumerate() {
        if show {
            ui.painter().circle_filled(Pos2::new(title_r.right() - 4.0 - i as f32 * 8.0, title_r.center().y), 2.5, col);
        }
    }
    tr.context_menu(|ui| channel_menu(ui, core, st, ch));

    let cue_r = Rect::from_min_size(Pos2::new(r.left() + 2.0, title_r.bottom() + 10.0), Vec2::new(r.width() - 4.0, 32.0));
    let cue_style = if p.cue { Style::Outline(ORANGE, true) } else { Style::Outline(Color32::from_gray(64), false) };
    let resp = button(ui, cue_r, "CUE", cue_style, mono(12.5));
    if p.cue {
        ui.painter().rect_filled(cue_r, CornerRadius::same(6), alpha(ORANGE, 0.16));
    }
    learnable(core, &resp, &a("Cue"));
    if resp.clicked() {
        act_i(core, &a("Cue"), !p.cue as i32);
    }

    // ON/OFF under the fader: switches what the fader controls fully off and back.
    let mode = st.fader_mode;
    let t = mode.target(core, ch);
    let on_r = Rect::from_min_max(Pos2::new(r.left() + 2.0, r.bottom() - 36.0), Pos2::new(r.right() - 2.0, r.bottom()));
    let is_on = (t.value - t.off).abs() > 0.005;
    let mode_color = mode.color(ch);
    let on_style = if is_on { Style::Fill(mode_color) } else { Style::Outline(Color32::from_gray(70), false) };
    let resp = button(ui, on_r, if is_on { "ON" } else { "OFF" }, on_style, bold(13.0))
        .on_hover_text(format!("{}: switch fully off, then back to where it was", mode.name()));
    learnable(core, &resp, &t.addr);
    if resp.clicked() && !core.learn {
        if is_on {
            st.fader_restore.insert((ch, mode), t.value);
            act_f(core, &t.addr, t.off);
        } else {
            let back = st.fader_restore.remove(&(ch, mode)).filter(|v| (v - t.off).abs() > 0.005).unwrap_or(t.full);
            act_f(core, &t.addr, back);
        }
    }

    // The fader: volume, an EQ band, the filter, a stem or the deck's tempo.
    let track = Rect::from_min_max(Pos2::new(r.left() + 4.0, cue_r.bottom() + 12.0), Pos2::new(r.right() - 4.0, on_r.top() - 10.0));
    let level = mode == FaderMode::Level;
    let meter = if level { view.post_peak[0].max(view.post_peak[1]) } else { 0.0 };
    let fader_color = if p.mute && level { Color32::from_gray(90) } else { mode_color };
    let (resp, new) = vfader(ui, track, t.value, fader_color, meter, t.centred);
    if !level {
        ui.painter().text(Pos2::new(track.center().x, track.top() + 14.0), Align2::CENTER_CENTER, mode.short(), bold(11.0), mode_color);
    }
    let hover = match mode {
        FaderMode::Level => format!("Volume · {} · Ctrl-drag: trim ({:+.1} dB) · double-click: unity", core.settings.fader_curve.label(), p.trim_db),
        FaderMode::Drums | FaderMode::Bass | FaderMode::Vocals | FaderMode::Other if !core.snap.decks[ch].stems_ready => {
            format!("{} level · applies once the stems are separated · double-click: full", mode.name())
        }
        _ => format!("{} · double-click: {}", mode.name(), if t.centred { "centre" } else { "full" }),
    };
    let resp = resp.on_hover_text(hover);
    learnable(core, &resp, &t.addr);
    if !core.learn {
        if let Some(v) = new {
            if level && ui.input(|i| i.modifiers.command) {
                let tr = (p.trim_db / 12.0 + 0.5 + (v - p.volume)).clamp(0.0, 1.0);
                act_f(core, &a("Trim"), tr);
            } else {
                act_f(core, &t.addr, v);
            }
        }
        if resp.double_clicked() {
            act_f(core, &t.addr, t.full);
        }
    }
}

/// The toggle rows above the channel strips: what every channel fader controls. Mixer
/// functions on the first row, stems and tempo on the second.
fn mode_row(ui: &mut Ui, st: &mut UiState, r: Rect) {
    let gap = 6.0;
    let rh = (r.height() - gap) / 2.0;
    for (row, modes) in FaderMode::ALL.chunks(5).enumerate() {
        let w = (r.width() - gap * 4.0) / 5.0;
        let long = modes.iter().all(|m| pill_width(ui, m.name(), 12.0) <= w + 6.0);
        for (i, m) in modes.iter().enumerate() {
            let br = Rect::from_min_size(Pos2::new(r.left() + i as f32 * (w + gap), r.top() + row as f32 * (rh + gap)), Vec2::new(w, rh));
            let selected = st.fader_mode == *m;
            let col = m.color(0);
            let style = if selected { Style::Fill(if m.is_stem() { col } else { LIGHT }) } else { Style::Dark };
            let label = if long { m.name() } else { m.short() };
            let resp = button(ui, br, label, style, bold(12.0)).on_hover_text(format!("Channel faders control: {}", m.name()));
            if m.is_stem() && !selected {
                let mark = Rect::from_min_size(Pos2::new(br.left() + 8.0, br.bottom() - 4.0), Vec2::new(br.width() - 16.0, 2.0));
                ui.painter().rect_filled(mark, CornerRadius::same(1), col);
            }
            if resp.clicked() {
                st.fader_mode = *m;
            }
        }
    }
}

/// What the channel faders control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FaderMode {
    Level,
    High,
    Mid,
    Low,
    Filter,
    Drums,
    Bass,
    Vocals,
    Other,
    Tempo,
}

/// Where a fader mode points for one channel.
pub struct FaderTarget {
    pub value: f32,
    pub addr: String,
    /// Double-click / ON value.
    pub full: f32,
    /// OFF value.
    pub off: f32,
    /// Bipolar control drawn from the centre (EQ, filter, tempo).
    pub centred: bool,
}

impl FaderMode {
    pub const ALL: [FaderMode; 10] = [
        FaderMode::Level,
        FaderMode::High,
        FaderMode::Mid,
        FaderMode::Low,
        FaderMode::Filter,
        FaderMode::Drums,
        FaderMode::Bass,
        FaderMode::Vocals,
        FaderMode::Other,
        FaderMode::Tempo,
    ];

    pub fn name(self) -> &'static str {
        match self {
            FaderMode::Level => "Volume",
            FaderMode::High => "High",
            FaderMode::Mid => "Mid",
            FaderMode::Low => "Low",
            FaderMode::Filter => "Filter",
            FaderMode::Drums => "Drums",
            FaderMode::Bass => "Bass",
            FaderMode::Vocals => "Vocals",
            FaderMode::Other => "Other",
            FaderMode::Tempo => "Tempo",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            FaderMode::Level => "VOL",
            FaderMode::High => "HI",
            FaderMode::Mid => "MID",
            FaderMode::Low => "LOW",
            FaderMode::Filter => "FLT",
            FaderMode::Drums => "DRM",
            FaderMode::Bass => "BAS",
            FaderMode::Vocals => "VOX",
            FaderMode::Other => "OTH",
            FaderMode::Tempo => "TMP",
        }
    }

    fn stem(self) -> Option<usize> {
        match self {
            FaderMode::Drums => Some(0),
            FaderMode::Bass => Some(1),
            FaderMode::Vocals => Some(2),
            FaderMode::Other => Some(3),
            _ => None,
        }
    }

    pub fn is_stem(self) -> bool {
        self.stem().is_some()
    }

    /// Fader colour on channel `ch`: the deck's for volume and tempo, the stem's for stems.
    pub fn color(self, ch: usize) -> Color32 {
        match self {
            FaderMode::Level | FaderMode::Tempo => DECK[ch],
            _ => self.stem().map_or(LIGHT, |i| STEM[i]),
        }
    }

    pub fn target(self, core: &Core, ch: usize) -> FaderTarget {
        let n = ch + 1;
        let mixer = |s: &str| format!("Mixer/CH{n}/{s}");
        let p = &core.session.channels[ch].params;
        let t = |value, addr, full, off, centred| FaderTarget { value, addr, full, off, centred };
        match self {
            FaderMode::Level => t(p.volume, mixer("Volume"), 1.0, 0.0, false),
            FaderMode::High => t(p.eq[2], mixer("EQ/High"), 0.5, 0.0, true),
            FaderMode::Mid => t(p.eq[1], mixer("EQ/Mid"), 0.5, 0.0, true),
            FaderMode::Low => t(p.eq[0], mixer("EQ/Low"), 0.5, 0.0, true),
            FaderMode::Filter => t(p.cutoff, mixer("Filter1"), 0.5, 0.5, true),
            FaderMode::Tempo => t((core.snap.decks[ch].tempo_fader as f32 + 1.0) / 2.0, format!("Deck/{n}/Tempo"), 0.5, 0.5, true),
            _ => {
                let i = self.stem().unwrap();
                let name = ["Drums", "Bass", "Vocals", "Other"][i];
                t(core.snap.decks[ch].stem_gains[i], format!("Deck/{n}/Stem{name}Level"), 1.0, 0.0, false)
            }
        }
    }
}

/// Effects for a channel (in the channel's right-click menu).
fn effects_menu(ui: &mut Ui, core: &mut Core, st: &mut UiState, ch: usize) {
    let n = ch + 1;
    let a = |s: &str| format!("Mixer/CH{n}/{s}");
    let cm = core.session.channels[ch].clone();
    let view = core.snap.channels[ch];
    let slots = cm.fx.slots.iter().filter(|s| s.is_some()).count();
    let open = ui.selectable_label(st.fx_open[ch], format!("Effects ({slots})…"));
    learnable(core, &open, &a("FX"));
    if open.clicked() && !core.learn {
        st.fx_open[ch] = !st.fx_open[ch];
        ui.close();
    }
    if slots > 0 {
        if ui.selectable_label(cm.fx.bypass_all, "Bypass effects").clicked() {
            act_i(core, &a("FX"), cm.fx.bypass_all as i32);
        }
        ui.horizontal(|ui| {
            // One dot per occupied slot: green running, red failed/missing, grey bypassed.
            for s in 0..cm.fx.slots.len().min(4) {
                let col = match (&cm.fx.slots[s], view.fx[s]) {
                    (None, _) => continue,
                    (Some(m), _) if m.missing => RED,
                    (Some(_), Some((_, SlotHealth::Failed, _))) => RED,
                    (Some(_), Some((true, _, _))) => GREEN,
                    _ => Color32::from_gray(110),
                };
                let (dot, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                ui.painter().circle_filled(dot.center(), 3.0, col);
            }
        });
    }
}

fn channel_menu(ui: &mut Ui, core: &mut Core, st: &mut UiState, ch: usize) {
    let n = ch + 1;
    let a = |s: &str| format!("Mixer/CH{n}/{s}");
    effects_menu(ui, core, st, ch);
    ui.separator();
    let p = core.session.channels[ch].params;
    if ui.selectable_label(p.mute, "Mute").clicked() {
        act_i(core, &a("Mute"), !p.mute as i32);
    }
    if ui.selectable_label(p.solo, "Solo").clicked() {
        act_i(core, &a("Solo"), !p.solo as i32);
    }
    let fs = core.session.channels[ch].fader_start;
    if ui.selectable_label(fs, "Fader start").clicked() {
        act_i(core, &a("FaderStart"), !fs as i32);
    }
    ui.separator();
    let mut trim = p.trim_db;
    if ui.add(egui::Slider::new(&mut trim, -6.0..=6.0).text("Trim dB")).changed() {
        act_f(core, &a("Trim"), trim / 12.0 + 0.5);
    }
    let mut res = p.res;
    if ui.add(egui::Slider::new(&mut res, 0.0..=1.0).text("Filter resonance")).changed() {
        act_f(core, &a("Filter2"), res);
    }
    let mut width = p.width;
    if ui.add(egui::Slider::new(&mut width, 0.0..=1.0).text("Filter width")).changed() {
        act_f(core, &a("Filter3"), width);
    }
    ui.separator();
    ui.label("Output");
    if ui.selectable_label(p.output == Output::Master, "Master").clicked() {
        act_i(core, &a("Output"), 0);
    }
    for i in 0..4u8 {
        if ui.selectable_label(p.output == Output::Pair(i), format!("Direct {}/{}", i * 2 + 1, i * 2 + 2)).clicked() {
            act_i(core, &a("Output"), i as i32 + 1);
        }
    }
}

fn master_strip(ui: &mut Ui, core: &mut Core, rect: Rect) {
    strip_frame(ui, rect, Color32::from_gray(150), false);
    let r = rect.shrink2(Vec2::new(8.0, 12.0));
    let m = core.session.master.clone();
    let s = core.snap;
    let title_r = Rect::from_min_size(r.min, Vec2::new(r.width(), 16.0));
    ui.painter().text(title_r.center(), Align2::CENTER_CENTER, "MASTER", mono(12.0), TEXT_DIM);
    let lim_r = Rect::from_min_size(Pos2::new(r.left() + 2.0, title_r.bottom() + 10.0), Vec2::new(r.width() - 4.0, 28.0));
    let style = if m.limiter { Style::Outline(LIGHT, true) } else { Style::Outline(Color32::from_gray(64), false) };
    let resp = button(ui, lim_r, "LIMIT", style, mono(12.0)).on_hover_text(format!("Master limiter · reduction {:.1} dB", s.limiter_db));
    learnable(core, &resp, "Header/Limiter");
    if resp.clicked() {
        act_i(core, "Header/Limiter", !m.limiter as i32);
    }
    let track = Rect::from_min_max(Pos2::new(r.left() + 4.0, lim_r.bottom() + 14.0), Pos2::new(r.right() - 4.0, r.bottom()));
    let (resp, new) = vfader(ui, track, m.volume, Color32::from_gray(200), s.master.peak[0].max(s.master.peak[1]), false);
    let resp = resp.on_hover_text(format!("Master · {} · double-click: reset", db_text(MasterModel::knob_gain(m.volume))));
    learnable(core, &resp, "Header/MasterVolume");
    if let (Some(v), false) = (new, core.learn) {
        act_f(core, "Header/MasterVolume", v);
    }
    if resp.double_clicked() {
        act_f(core, "Header/MasterVolume", 0.85);
    }
    let _ = NUM_DECKS;
}

