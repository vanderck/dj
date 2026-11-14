//! Mixer channel strips drawn into fixed rects.

use eframe::egui::{self, Align2, Color32, CornerRadius, Pos2, Rect, Sense, Ui, UiBuilder, Vec2};

use collective_engine::engine::NUM_DECKS;
use collective_engine::fx::SlotHealth;
use collective_engine::mixer::{MixMode, Output, XfAssign};

use super::skin::*;
use super::{act_f, act_i, learnable, UiState};
use crate::core::Core;

pub fn channel(ui: &mut Ui, core: &mut Core, st: &mut UiState, ch: usize, rect: Rect) {
    panel(ui, rect, PANEL);
    let n = ch + 1;
    let a = |s: &str| format!("Mixer/CH{n}/{s}");
    let cm = core.session.channels[ch].clone();
    let p = cm.params;
    let view = core.snap.channels[ch];
    let cx = rect.center().x;
    let h = rect.height();
    // Vertical rhythm scales with the available height.
    let y = |f: f32| rect.top() + h * f;

    rule_title(ui, Rect::from_min_max(Pos2::new(rect.left(), y(0.02)), Pos2::new(rect.right(), y(0.09))), &format!("CHANNEL {n}"));

    // Big volume knob with trim ring and VU dots.
    let r_big = (h * 0.15).clamp(22.0, 36.0);
    let kc = Pos2::new(cx, y(0.30));
    let mut vol = p.volume;
    let mut trim = p.trim_db / 12.0 + 0.5;
    let vu = {
        let pk = view.post_peak[0].max(view.post_peak[1]);
        let db = 20.0 * pk.max(1e-5).log10();
        ((db + 48.0) / 51.0).clamp(0.0, 1.0)
    };
    let (resp, trim_changed, f_clicked) = volume_knob(ui, kc, r_big, &mut vol, &mut trim, vu, cm.fader_start, DECK[ch]);
    let resp = resp.on_hover_text(format!(
        "Volume {:.0}% ({})\nCtrl/Cmd-drag: trim {:+.1} dB · double-click: reset",
        p.volume * 100.0,
        core.settings.fader_curve.label(),
        p.trim_db
    ));
    learnable(core, &resp, &a("Volume"));
    if resp.changed() && vol != p.volume {
        act_f(core, &a("Volume"), vol);
    }
    if trim_changed {
        act_f(core, &a("Trim"), trim);
    }
    if f_clicked {
        act_i(core, &a("FaderStart"), !cm.fader_start as i32);
    }

    // FILTER / EQ section.
    let title_r = Rect::from_min_max(Pos2::new(rect.left(), y(0.50)), Pos2::new(rect.right(), y(0.57)));
    let tr = ui.allocate_rect(title_r, Sense::click()).on_hover_text("Click to switch between filter and EQ");
    rule_title(ui, title_r, if cm.show_eq { "EQ" } else { "FILTER" });
    if tr.clicked() {
        core.session.channels[ch].show_eq = !cm.show_eq;
    }
    let kr = (h * 0.065).clamp(10.0, 15.0);
    let ky = y(0.66);
    let xs = [rect.left() + rect.width() * 0.2, cx, rect.left() + rect.width() * 0.8];
    let items: [(String, &str, f32, f32, bool); 3] = if cm.show_eq {
        [
            (a("EQ/Low"), "LOW", p.eq[0], 0.5, true),
            (a("EQ/Mid"), "MID", p.eq[1], 0.5, true),
            (a("EQ/High"), "HI", p.eq[2], 0.5, true),
        ]
    } else {
        [
            (a("Filter1"), "CUTOFF", p.cutoff, 0.5, true),
            (a("Filter2"), "RES", p.res, 0.5, false),
            (a("Filter3"), "-WIDTH+", p.width, 1.0, false),
        ]
    };
    for (i, (addr, label, val, def, bip)) in items.iter().enumerate() {
        let mut v = *val;
        let killed = cm.show_eq && v <= 0.001;
        let resp = small_knob(ui, Pos2::new(xs[i], ky), kr, &mut v, *def, *bip, RED);
        learnable(core, &resp, addr);
        if resp.changed() {
            act_f(core, addr, v);
        }
        let lr = Rect::from_center_size(Pos2::new(xs[i], ky + kr + 9.0), Vec2::new(rect.width() / 3.0, 12.0));
        let lresp = ui.allocate_rect(lr, Sense::click());
        ui.painter().text(lr.center(), Align2::CENTER_CENTER, *label, font(8.5), if killed { RED } else { TEXT_DIM });
        if lresp.clicked() {
            // EQ: click the label to kill / restore. Filter: reset.
            let nv = if cm.show_eq { if killed { 0.5 } else { 0.0 } } else { *def };
            act_f(core, addr, nv);
        }
    }

    // CUE MUTE SOLO FX
    let row_y = y(0.80);
    let bh = (h * 0.085).clamp(16.0, 22.0);
    let bw = (rect.width() - 10.0 - 6.0) / 4.0;
    let any_solo = (0..NUM_DECKS).any(|c| core.session.channels[c].params.solo);
    for (i, (lbl, addr, on)) in [("CUE", "Cue", p.cue), ("MUTE", "Mute", p.mute), ("SOLO", "Solo", p.solo), ("FX", "FX", false)].iter().enumerate() {
        let br = Rect::from_min_size(Pos2::new(rect.left() + 5.0 + i as f32 * (bw + 2.0), row_y), Vec2::new(bw, bh));
        let suppressed = *addr == "Mute" && any_solo && !p.solo && !p.mute;
        let look = match *addr {
            "FX" => {
                let has = cm.fx.slots.iter().any(|s| s.is_some());
                if has && !cm.fx.bypass_all {
                    Look::On(SILVER)
                } else {
                    Look::Normal
                }
            }
            _ if *on => Look::On(if *addr == "Cue" { Color32::from_rgb(0xf0, 0x8c, 0x2e) } else if *addr == "Mute" { RED } else { SILVER }),
            _ if suppressed => Look::On(dim(RED, 0.45)),
            _ => Look::Normal,
        };
        let resp = btn_sized(ui, br, lbl, look, 9.5);
        learnable(core, &resp, &a(addr));
        if *addr == "FX" {
            // Underline: one segment per slot (lit running, dim bypassed, red failed/missing).
            let seg_w = (br.width() - 6.0) / 4.0;
            for s in 0..4 {
                let col = match (&cm.fx.slots[s], view.fx[s]) {
                    (None, _) => continue,
                    (Some(m), _) if m.missing => RED,
                    (Some(_), Some((_, SlotHealth::Failed, _))) => RED,
                    (Some(_), Some((true, _, _))) => Color32::from_rgb(0x5f, 0xd0, 0x8c),
                    _ => Color32::from_gray(90),
                };
                ui.painter().rect_filled(
                    Rect::from_min_size(Pos2::new(br.left() + 3.0 + s as f32 * seg_w, br.bottom() - 3.0), Vec2::new(seg_w - 1.0, 2.0)),
                    CornerRadius::ZERO,
                    col,
                );
            }
            if resp.clicked() && !core.learn {
                st.fx_open[ch] = !st.fx_open[ch];
            }
            if resp.secondary_clicked() {
                act_i(core, &a("FX"), cm.fx.bypass_all as i32);
            }
            continue;
        }
        if resp.clicked() {
            if suppressed {
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

    // Output destination.
    let out_r = Rect::from_min_max(Pos2::new(rect.left() + 5.0, y(0.80) + bh + 5.0), Pos2::new(rect.right() - 5.0, (y(0.80) + bh * 2.0 + 5.0).min(rect.bottom() - 3.0)));
    let out_txt = match p.output {
        Output::Master => "MASTER".to_string(),
        Output::None => "NONE".into(),
        Output::Pair(i) => format!("OUT {}/{}", i * 2 + 1, i * 2 + 2),
    };
    let resp = btn_sized(ui, out_r, &format!("{out_txt}  ▾"), Look::On(Color32::from_gray(30)), 9.5);
    egui::Popup::menu(&resp).show(|ui| {
        if ui.selectable_label(p.output == Output::Master, "MASTER").clicked() {
            act_i(core, &a("Output"), 0);
        }
        if ui.selectable_label(p.output == Output::None, "NONE").clicked() {
            core.set_channel(ch, |p| p.output = Output::None);
        }
        for i in 0..4u8 {
            if ui.selectable_label(p.output == Output::Pair(i), format!("Direct {}/{}", i * 2 + 1, i * 2 + 2)).clicked() {
                act_i(core, &a("Output"), i as i32 + 1);
            }
        }
        if matches!(core.session.mix_mode, MixMode::Crossfader { .. }) {
            ui.separator();
            ui.label("Crossfader side");
            for (t, asg, i) in [("A", XfAssign::A, 0), ("Thru", XfAssign::Thru, 1), ("B", XfAssign::B, 2)] {
                if ui.selectable_label(cm.xf_assign == asg, t).clicked() {
                    act_i(core, &a("XFader"), i);
                }
            }
        }
    });
}

/// Crossfader or vector pad in the mixer column's spare space.
pub fn mix_tools(ui: &mut Ui, core: &mut Core, rect: Rect) {
    let mut child = ui.new_child(UiBuilder::new().max_rect(rect.shrink(4.0)));
    let ui = &mut child;
    let mode = core.session.mix_mode;
    ui.horizontal(|ui| {
        for (lbl, m) in [("OFF", MixMode::Off), ("X-FADER", MixMode::Crossfader { pos: 0.0, sharp: false }), ("VECTOR", MixMode::Vector { x: 0.5, y: 0.5 })] {
            let on = std::mem::discriminant(&mode) == std::mem::discriminant(&m);
            if ui.selectable_label(on, lbl).clicked() && !on {
                core.session.mix_mode = m;
                core.apply_mix_mode();
            }
        }
    });
    match mode {
        MixMode::Crossfader { .. } => {
            let (r, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::click_and_drag());
            let p = ui.painter();
            p.rect_filled(Rect::from_center_size(r.center(), Vec2::new(r.width(), 4.0)), CornerRadius::same(2), Color32::from_gray(10));
            let t = (core.session.crossfader + 1.0) / 2.0;
            let x = r.left() + t * r.width();
            p.rect_filled(Rect::from_center_size(Pos2::new(x, r.center().y), Vec2::new(12.0, 18.0)), CornerRadius::same(2), Color32::from_gray(160));
            learnable(core, &resp, "Header/Crossfader");
            if (resp.dragged() || resp.clicked()) && !core.learn {
                if let Some(pos) = resp.interact_pointer_pos() {
                    act_f(core, "Header/Crossfader", ((pos.x - r.left()) / r.width()).clamp(0.0, 1.0));
                }
            }
            if resp.double_clicked() {
                act_f(core, "Header/Crossfader", 0.5);
            }
        }
        MixMode::Vector { .. } => {
            let side = ui.available_height().min(ui.available_width()) - 4.0;
            let (r, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), side.max(30.0)), Sense::click_and_drag());
            let p = ui.painter();
            p.rect_filled(r, CornerRadius::same(3), WAVE_BG);
            for (i, c) in [r.left_top(), r.right_top(), r.left_bottom(), r.right_bottom()].iter().enumerate() {
                let off = Vec2::new(if c.x > r.center().x { -9.0 } else { 9.0 }, if c.y > r.center().y { -8.0 } else { 8.0 });
                p.text(*c + off, Align2::CENTER_CENTER, format!("{}", i + 1), font(11.0), DECK[i]);
            }
            if (resp.dragged() || resp.clicked()) && !core.learn {
                if let Some(pos) = resp.interact_pointer_pos() {
                    core.session.vector = (((pos.x - r.left()) / r.width()).clamp(0.0, 1.0), ((pos.y - r.top()) / r.height()).clamp(0.0, 1.0));
                    core.apply_mix_mode();
                }
            }
            if resp.double_clicked() {
                core.session.vector = (0.5, 0.5);
                core.apply_mix_mode();
            }
            let (x, y) = core.session.vector;
            ui.painter().circle_filled(Pos2::new(r.left() + x * r.width(), r.top() + y * r.height()), 5.0, RED);
        }
        _ => {}
    }
}
