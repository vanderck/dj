//! Mixer: four channel strips, crossfader / vector pad.

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};

use collective_engine::engine::NUM_DECKS;
use collective_engine::fx::SlotHealth;
use collective_engine::mixer::{MixMode, Output, XfAssign};

use super::widgets::*;
use super::{act_f, act_i, learnable, UiState};
use crate::core::Core;

pub fn show(ui: &mut Ui, core: &mut Core, st: &mut UiState) {
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        for ch in 0..core.deck_count() {
            strip(ui, core, st, ch);
            if ch + 1 < core.deck_count() {
                ui.separator();
            }
        }
    });
    ui.separator();
    mix_tools(ui, core);
}

fn strip(ui: &mut Ui, core: &mut Core, st: &mut UiState, ch: usize) {
    let color = DECK_COLORS[ch];
    let n = ch + 1;
    let a = |s: &str| format!("Mixer/CH{n}/{s}");
    let cm = core.session.channels[ch].clone();
    let p = cm.params;
    let view = core.snap.channels[ch];
    ui.vertical(|ui| {
        ui.set_width(84.0);
        ui.label(RichText::new(format!("CH {n}")).strong().color(color));

        // FILTER / EQ title toggles the knob row; both keep their settings.
        let title = if cm.show_eq { "EQ" } else { "FILTER" };
        if ui.small_button(title).on_hover_text("Switch between EQ and filter knobs").clicked() {
            core.session.channels[ch].show_eq = !cm.show_eq;
        }
        if cm.show_eq {
            for (b, name) in [(2usize, "HI"), (1, "MID"), (0, "LOW")] {
                let addr = a(&format!("EQ/{}", ["Low", "Mid", "High"][b]));
                ui.horizontal(|ui| {
                    let mut v = p.eq[b];
                    let killed = v <= 0.001;
                    let r = knob(ui, &mut v, 0.0, 1.0, 0.5, "", if killed { DANGER } else { color });
                    learnable(core, &r, &addr);
                    if r.changed() {
                        act_f(core, &addr, v);
                    }
                    // Click the label to kill / restore the band.
                    let lbl = ui.add(egui::Label::new(RichText::new(name).small().color(if killed { DANGER } else { ui.visuals().text_color() })).sense(Sense::click()));
                    if lbl.on_hover_text("Click: full cut / restore").clicked() {
                        act_f(core, &addr, if killed { 0.5 } else { 0.0 });
                    }
                });
            }
        } else {
            for (addr, name, val, def) in [("Filter1", "CUTOFF", p.cutoff, 0.5), ("Filter2", "RES", p.res, 0.5), ("Filter3", "WIDTH", p.width, 1.0)] {
                ui.horizontal(|ui| {
                    let mut v = val;
                    let r = knob(ui, &mut v, 0.0, 1.0, def, "", color);
                    learnable(core, &r, &a(addr));
                    if r.changed() {
                        act_f(core, &a(addr), v);
                    }
                    let lbl = ui.add(egui::Label::new(RichText::new(name).small()).sense(Sense::click()));
                    if lbl.on_hover_text("Click to reset").clicked() {
                        act_f(core, &a(addr), def);
                    }
                });
            }
        }

        // Trim (Shift-drag on the fader area, or this small knob).
        ui.horizontal(|ui| {
            let mut t = p.trim_db / 12.0 + 0.5;
            let r = knob(ui, &mut t, 0.0, 1.0, 0.5, &format!("{:+.1}dB", p.trim_db), Color32::from_gray(160));
            learnable(core, &r, &a("Trim"));
            if r.changed() {
                act_f(core, &a("Trim"), t);
            }
            ui.vertical(|ui| {
                let f = cm.fader_start;
                let r = lit(ui, f, "F", ACCENT).on_hover_text("Fader start: raising the volume starts the deck (quantized), closing pauses");
                learnable(core, &r, &a("FaderStart"));
                if r.clicked() {
                    act_i(core, &a("FaderStart"), !f as i32);
                }
            });
        });

        ui.horizontal(|ui| {
            let mut v = p.volume;
            let r = vfader(ui, &mut v, 130.0, Some(view.post_peak), color).on_hover_text(format!(
                "Volume {:.0}% · {}",
                p.volume * 100.0,
                core.settings.fader_curve.label()
            ));
            learnable(core, &r, &a("Volume"));
            if r.changed() {
                act_f(core, &a("Volume"), v);
            }
            ui.vertical(|ui| {
                for (txt, addr, on, col) in [
                    ("CUE", "Cue", p.cue, WARN),
                    ("M", "Mute", p.mute, DANGER),
                    ("S", "Solo", p.solo, ACCENT),
                ] {
                    let solo_suppressed = addr == "Mute" && !p.solo && (0..NUM_DECKS).any(|c| core.session.channels[c].params.solo);
                    let r = lit(ui, on || solo_suppressed, txt, if solo_suppressed && !on { dim(col, 0.5) } else { col });
                    learnable(core, &r, &a(addr));
                    if r.clicked() {
                        if solo_suppressed && !on {
                            // Clicking a suppressed MUTE clears solos and unmutes this channel.
                            for c in 0..NUM_DECKS {
                                if core.session.channels[c].params.solo {
                                    act_i(core, &format!("Mixer/CH{}/Solo", c + 1), 0);
                                }
                            }
                        } else {
                            act_i(core, &a(addr), !on as i32);
                        }
                    }
                }
                // FX: lit when inserts exist; underline segments show slot state.
                let has = cm.fx.slots.iter().any(|s| s.is_some());
                let running = !cm.fx.bypass_all;
                let r = lit(ui, has && running, "FX", ACCENT).on_hover_text("Open the channel's insert window. Right-click: bypass all");
                learnable(core, &r, &a("FX"));
                if r.clicked() && !core.learn {
                    st.fx_open[ch] = !st.fx_open[ch];
                }
                if r.secondary_clicked() {
                    act_i(core, &a("FX"), cm.fx.bypass_all as i32);
                }
                let (seg, _) = ui.allocate_exact_size(Vec2::new(r.rect.width(), 3.0), Sense::hover());
                let w = seg.width() / 4.0;
                for s in 0..4 {
                    let col = match (&cm.fx.slots[s], view.fx[s]) {
                        (None, _) => Color32::TRANSPARENT,
                        (Some(m), _) if m.missing => DANGER,
                        (Some(_), Some((_, SlotHealth::Failed, _))) => DANGER,
                        (Some(_), Some((true, _, _))) => ACCENT,
                        _ => Color32::from_gray(80),
                    };
                    ui.painter().rect_filled(
                        Rect::from_min_size(Pos2::new(seg.left() + s as f32 * w + 1.0, seg.top()), Vec2::new(w - 2.0, 3.0)),
                        CornerRadius::ZERO,
                        col,
                    );
                }
            });
        });

        // Output destination.
        let out_txt = match p.output {
            Output::Master => "MASTER".to_string(),
            Output::None => "NONE".into(),
            Output::Pair(i) => format!("{}/{}", i * 2 + 1, i * 2 + 2),
        };
        egui::ComboBox::from_id_salt(("out", ch)).width(72.0).selected_text(out_txt).show_ui(ui, |ui| {
            if ui.selectable_label(p.output == Output::Master, "MASTER").clicked() {
                act_i(core, &a("Output"), 0);
            }
            if ui.selectable_label(p.output == Output::None, "NONE").clicked() {
                core.set_channel(ch, |p| p.output = Output::None);
            }
            for i in 0..4u8 {
                let lbl = format!("Direct {}/{}", i * 2 + 1, i * 2 + 2);
                if ui.selectable_label(p.output == Output::Pair(i), lbl).clicked() {
                    act_i(core, &a("Output"), i as i32 + 1);
                }
            }
        });
        if matches!(core.session.mix_mode, MixMode::Crossfader { .. }) {
            ui.horizontal(|ui| {
                for (t, asg, i) in [("A", XfAssign::A, 0), ("·", XfAssign::Thru, 1), ("B", XfAssign::B, 2)] {
                    if ui.selectable_label(cm.xf_assign == asg, t).clicked() {
                        act_i(core, &a("XFader"), i);
                    }
                }
            });
        }
    });
}

fn mix_tools(ui: &mut Ui, core: &mut Core) {
    let mode = core.session.mix_mode;
    ui.horizontal(|ui| {
        ui.label(RichText::new("MIX").small().weak());
        if ui.selectable_label(mode == MixMode::Off, "Off").clicked() {
            core.session.mix_mode = MixMode::Off;
            core.apply_mix_mode();
        }
        if ui.selectable_label(matches!(mode, MixMode::Crossfader { .. }), "Crossfader").clicked() {
            core.session.mix_mode = MixMode::Crossfader { pos: 0.0, sharp: false };
            core.apply_mix_mode();
        }
        if ui.selectable_label(matches!(mode, MixMode::Vector { .. }), "Vector").clicked() {
            core.session.mix_mode = MixMode::Vector { x: 0.5, y: 0.5 };
            core.apply_mix_mode();
        }
    });
    match mode {
        MixMode::Crossfader { .. } => {
            ui.horizontal(|ui| {
                ui.label("A");
                let mut v = (core.session.crossfader + 1.0) / 2.0;
                let r = hslider(ui, &mut v, 0.0, 1.0, 230.0, ACCENT);
                learnable(core, &r, "Header/Crossfader");
                if r.changed() {
                    act_f(core, "Header/Crossfader", v);
                }
                if r.double_clicked() {
                    act_f(core, "Header/Crossfader", 0.5);
                }
                ui.label("B");
                let sharp = core.session.crossfader_sharp;
                if ui.selectable_label(sharp, "Cut").on_hover_text("Sharp cut curve (scratch)").clicked() {
                    core.session.crossfader_sharp = !sharp;
                    core.apply_mix_mode();
                }
            });
        }
        MixMode::Vector { .. } => {
            let size = Vec2::new(150.0, 110.0);
            let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
            let p = ui.painter();
            p.rect_filled(rect, CornerRadius::same(4), Color32::from_gray(18));
            for (i, c) in [rect.left_top(), rect.right_top(), rect.left_bottom(), rect.right_bottom()].iter().enumerate() {
                let off = Vec2::new(if c.x > rect.center().x { -12.0 } else { 12.0 }, if c.y > rect.center().y { -10.0 } else { 10.0 });
                p.text(*c + off, egui::Align2::CENTER_CENTER, format!("{}", i + 1), egui::FontId::proportional(13.0), DECK_COLORS[i]);
            }
            if (resp.dragged() || resp.clicked()) && !core.learn {
                if let Some(pos) = resp.interact_pointer_pos() {
                    let x = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
                    let y = ((pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
                    core.session.vector = (x, y);
                    core.apply_mix_mode();
                }
            }
            if resp.double_clicked() {
                core.session.vector = (0.5, 0.5);
                core.apply_mix_mode();
            }
            learnable(core, &resp, "Header/VectorX");
            let (x, y) = core.session.vector;
            let pt = Pos2::new(rect.left() + x * rect.width(), rect.top() + y * rect.height());
            ui.painter().circle_filled(pt, 6.0, ACCENT);
            ui.painter().circle_stroke(pt, 9.0, Stroke::new(1.0, Color32::WHITE));
        }
        MixMode::Off | MixMode::Delta { .. } => {}
    }
}
