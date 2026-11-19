//! Channel insert windows (User Guide §7.4, rebuilt).

use eframe::egui::{self, Color32, RichText, Ui, Vec2};

use dj_engine::fx::{InsertPos, SlotHealth, MAX_SLOTS, NUM_MACROS};

use super::widgets::*;
use super::{act_f, learnable, InsertBrowser, UiState};
use crate::core::Core;
use crate::model::FxChainModel;

pub fn windows(ctx: &egui::Context, core: &mut Core, st: &mut UiState) {
    for ch in 0..4 {
        if !st.fx_open[ch] {
            continue;
        }
        let mut open = true;
        egui::Window::new(format!("CH {} · Insert FX", ch + 1))
            .id(egui::Id::unique(("fxwin", ch)))
            .open(&mut open)
            .default_width(560.0)
            .resizable(true)
            .show(ctx, |ui| chain(ui, core, st, ch));
        st.fx_open[ch] = open;
    }
    insert_browser(ctx, core, st);
}

fn chain(ui: &mut Ui, core: &mut Core, st: &mut UiState, ch: usize) {
    let color = DECK_COLORS[ch];
    let view = core.snap.channels[ch];

    // ---- presets
    ui.horizontal(|ui| {
        let name = &mut st.preset_name[ch];
        if name.is_empty() {
            *name = core.session.channels[ch].fx.name.clone();
        }
        ui.label("Preset");
        ui.add(egui::TextEdit::singleline(name).desired_width(140.0).hint_text("name"));
        if ui.button("Save").clicked() {
            let n = if name.trim().is_empty() { format!("CH{} chain", ch + 1) } else { name.trim().to_string() };
            core.save_fx_preset(ch, &n);
        }
        ui.menu_button("Load ▾", |ui| {
            let presets = FxChainModel::list_presets();
            if presets.is_empty() {
                ui.label(RichText::new("No presets yet").weak());
            }
            for (n, p) in presets {
                if ui.button(&n).clicked() {
                    core.load_fx_preset(ch, &p);
                    st.preset_name[ch] = n.clone();
                    ui.close();
                }
            }
            ui.separator();
            if ui.button("Open preset folder").clicked() {
                let dir = FxChainModel::presets_dir();
                let _ = std::fs::create_dir_all(&dir);
                open_path(&dir);
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let bypass = core.session.channels[ch].fx.bypass_all;
            let r = lit(ui, bypass, "BYPASS ALL", WARN);
            learnable(core, &r, &format!("Mixer/CH{}/FX", ch + 1));
            if r.clicked() {
                core.set_bypass_all(ch, !bypass);
            }
        });
    });

    // ---- signal flow line
    let fx = core.session.channels[ch].fx.clone();
    let names = |pos: InsertPos| -> Vec<String> {
        fx.slots.iter().flatten().filter(|s| s.pos == pos).map(|s| s.name.clone()).collect()
    };
    let pre = names(InsertPos::Pre);
    let post = names(InsertPos::Post);
    let mut flow = String::from("TRIM › EQ › FILTER");
    for n in &pre {
        flow.push_str(&format!(" › [{n}]"));
    }
    flow.push_str(" › (CUE) › VOLUME");
    for n in &post {
        flow.push_str(&format!(" › [{n}]"));
    }
    flow.push_str(" › OUT");
    ui.label(RichText::new(flow).monospace().small().color(ui.visuals().weak_text_color()));
    let (pre_l, post_l) = view.fx_latency;
    if pre_l + post_l > 0 {
        let ms = |s: u32| s as f32 / core.sample_rate * 1000.0;
        ui.label(
            RichText::new(format!(
                "Latency: PRE {:.1} ms + POST {:.1} ms — compensated across channels ({:.1} ms total)",
                ms(pre_l),
                ms(post_l),
                ms(core.snap.compensation_latency)
            ))
            .small()
            .color(WARN),
        );
    }
    ui.separator();

    // ---- slots
    let mut move_req: Option<(usize, usize)> = None;
    for slot in 0..MAX_SLOTS {
        let model = core.session.channels[ch].fx.slots[slot].clone();
        let health = view.fx[slot];
        let row = ui.horizontal(|ui| {
            match &model {
                None => {
                    ui.label(RichText::new(format!("{}", slot + 1)).weak());
                    if ui.button("+ ADD INSERT").clicked() {
                        st.insert_browser = Some(InsertBrowser { ch, replace: None, pos: InsertPos::Pre, ..Default::default() });
                    }
                }
                Some(m) => {
                    let id = egui::Id::unique(("slotdrag", ch, slot));
                    ui.dnd_drag_source(id, slot, |ui| {
                        ui.label(RichText::new("⠿").size(16.0)).on_hover_text("Drag to reorder");
                    });
                    let (dot, tip) = if m.missing {
                        (DANGER, "Effect not installed — replace or remove it")
                    } else {
                        match health {
                            Some((_, SlotHealth::Failed, _)) => (DANGER, "Effect failed and was bypassed"),
                            Some((true, _, _)) => (OK, "Running"),
                            _ => (Color32::from_gray(90), "Bypassed"),
                        }
                    };
                    ui.label(RichText::new("●").color(dot)).on_hover_text(tip);
                    ui.label(RichText::new(format!("{} {}", slot + 1, m.name)).strong());
                    ui.label(RichText::new(&m.vendor).small().weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.menu_button("⋯", |ui| {
                            if ui.button("Replace…").clicked() {
                                st.insert_browser = Some(InsertBrowser { ch, replace: Some(slot), pos: m.pos, ..Default::default() });
                                ui.close();
                            }
                            if slot > 0 && ui.button("Move up").clicked() {
                                move_req = Some((slot, slot - 1));
                                ui.close();
                            }
                            if slot + 1 < MAX_SLOTS && ui.button("Move down").clicked() {
                                move_req = Some((slot, slot + 1));
                                ui.close();
                            }
                            if matches!(health, Some((_, SlotHealth::Failed, _))) && ui.button("Re-enable").clicked() {
                                core.rearm_insert(ch, slot);
                                ui.close();
                            }
                            if ui.button("Remove").clicked() {
                                core.remove_insert(ch, slot);
                                st.fx_edit[ch][slot] = false;
                                ui.close();
                            }
                        });
                        let edit = st.fx_edit[ch][slot];
                        if lit(ui, edit, "EDIT", ACCENT).clicked() {
                            st.fx_edit[ch][slot] = !edit;
                        }
                        let mut mix = m.mix;
                        let mix_label = format!("MIX {:.0}%", mix * 100.0);
                        let r = knob(ui, &mut mix, 0.0, 1.0, 1.0, &mix_label, color);
                        learnable(core, &r, &format!("Mixer/CH{}/FX/Slot/{}/Mix", ch + 1, slot + 1));
                        if r.changed() {
                            core.set_slot(ch, slot, |s| s.mix = mix);
                        }
                        let r = lit(ui, m.on, "ON", OK);
                        learnable(core, &r, &format!("Mixer/CH{}/FX/Slot/{}/On", ch + 1, slot + 1));
                        if r.clicked() {
                            core.set_slot(ch, slot, |s| s.on = !s.on);
                        }
                        let is_post = m.pos == InsertPos::Post;
                        if ui
                            .selectable_label(is_post, if is_post { "POST" } else { "PRE" })
                            .on_hover_text("PRE: after EQ/filter, before the fader (cue hears it). POST: after the fader.")
                            .clicked()
                        {
                            core.set_slot(ch, slot, |s| s.pos = if is_post { InsertPos::Pre } else { InsertPos::Post });
                        }
                        if let Some((_, _, peak)) = health {
                            let (r, _) = ui.allocate_exact_size(Vec2::new(8.0, 20.0), egui::Sense::hover());
                            draw_meter(ui, r, [peak, peak]);
                        }
                    });
                }
            }
        });
        // Drop target: dropping a slot onto this row moves it here.
        if let Some(from) = row.response.dnd_release_payload::<usize>() {
            move_req = Some((*from, slot));
        }
        if model.is_some() && st.fx_edit[ch][slot] {
            param_editor(ui, core, st, ch, slot);
        }
        ui.separator();
    }
    if let Some((from, to)) = move_req {
        core.move_insert(ch, from, to);
        let e = st.fx_edit[ch];
        let mut ne = e;
        if from < to {
            ne[from..=to].rotate_left(1);
        } else {
            ne[to..=from].rotate_right(1);
        }
        st.fx_edit[ch] = ne;
    }

    // ---- macros
    ui.label(RichText::new(format!("CH {} MACROS", ch + 1)).strong());
    if let Some((mc, mi)) = st.map_learn {
        if mc == ch {
            ui.label(RichText::new(format!("MAP: move or click a parameter in any insert to assign it to Macro {}", mi + 1)).color(ACCENT));
        }
    }
    ui.horizontal_wrapped(|ui| {
        for i in 0..NUM_MACROS {
            ui.vertical(|ui| {
                let m = core.session.channels[ch].fx.macros[i].clone();
                let mut v = m.value;
                let assigned = !m.targets.is_empty();
                let r = knob(ui, &mut v, 0.0, 1.0, 0.0, &format!("M{}", i + 1), if assigned { color } else { Color32::from_gray(90) });
                let addr = format!("Mixer/CH{}/FX/Macro/{}", ch + 1, i + 1);
                learnable(core, &r, &addr);
                if r.changed() && assigned {
                    act_f(core, &addr, v);
                }
                let label = core.macro_label(ch, i);
                let lbl = ui.add(egui::Button::new(RichText::new(label).small()).small());
                if (lbl.clicked() && !assigned) || r.secondary_clicked() {
                    st.map_learn = Some((ch, i));
                }
                lbl.context_menu(|ui| macro_menu(ui, core, st, ch, i));
                if lbl.clicked() && assigned {
                    // Left click on an assigned macro opens the same menu.
                }
            });
        }
    });
}

fn macro_menu(ui: &mut Ui, core: &mut Core, st: &mut UiState, ch: usize, i: usize) {
    if ui.button("MAP (learn from an insert)").clicked() {
        st.map_learn = Some((ch, i));
        ui.close();
    }
    ui.menu_button("Assign parameter ▸", |ui| {
        for slot in 0..MAX_SLOTS {
            let Some(live) = core.fx_live[ch][slot].clone() else { continue };
            let name = core.session.channels[ch].fx.slots[slot].as_ref().map(|s| s.name.clone()).unwrap_or_default();
            ui.menu_button(format!("{} {}", slot + 1, name), |ui| {
                egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                    for p in 0..live.param_count() {
                        if ui.button(live.param_name(p)).clicked() {
                            core.assign_macro(ch, i, slot, p);
                            ui.close();
                        }
                    }
                });
            });
        }
    });
    let targets = core.session.channels[ch].fx.macros[i].targets.clone();
    if !targets.is_empty() {
        ui.separator();
        ui.label(RichText::new("Targets (range per parameter)").small());
        for (ti, t) in targets.iter().enumerate() {
            let pname = core.fx_live[ch][t.slot as usize].as_ref().map(|l| l.param_name(t.param as usize)).unwrap_or_default();
            ui.horizontal(|ui| {
                ui.label(format!("{}·{}", t.slot + 1, pname));
                let mut lo = t.min;
                let mut hi = t.max;
                let c1 = ui.add(egui::DragValue::new(&mut lo).range(0.0..=1.0).speed(0.01).prefix("min "));
                let c2 = ui.add(egui::DragValue::new(&mut hi).range(0.0..=1.0).speed(0.01).prefix("max "));
                if c1.changed() || c2.changed() {
                    core.set_macro_target(ch, i, ti, |t| {
                        t.min = lo;
                        t.max = hi;
                    });
                }
                if ui.small_button("⇅").on_hover_text("Invert range").clicked() {
                    core.set_macro_target(ch, i, ti, |t| std::mem::swap(&mut t.min, &mut t.max));
                }
                if ui.small_button("✕").clicked() {
                    core.clear_macro(ch, i, Some(ti));
                }
            });
        }
        if ui.button("Clear macro").clicked() {
            core.clear_macro(ch, i, None);
            ui.close();
        }
    }
}

fn param_editor(ui: &mut Ui, core: &mut Core, st: &mut UiState, ch: usize, slot: usize) {
    let Some(live) = core.fx_live[ch][slot].clone() else {
        ui.label(RichText::new("Effect not loaded").color(DANGER));
        return;
    };
    let params = core.session.channels[ch].fx.slots[slot].as_ref().map(|s| s.params.clone()).unwrap_or_default();
    let filter_id = egui::Id::unique(("pfilter", ch, slot));
    let mut filter: String = ui.data_mut(|d| d.get_temp(filter_id).unwrap_or_default());
    egui::Frame::group(ui.style()).show(ui, |ui| {
        if live.param_count() > 12 {
            ui.horizontal(|ui| {
                ui.label("🔍");
                ui.text_edit_singleline(&mut filter);
            });
        }
        egui::ScrollArea::vertical().max_height(260.0).id_salt(("pscroll", ch, slot)).show(ui, |ui| {
            egui::Grid::new(("pgrid", ch, slot)).num_columns(3).striped(true).show(ui, |ui| {
                for p in 0..live.param_count() {
                    let name = live.param_name(p);
                    if !filter.is_empty() && !name.to_lowercase().contains(&filter.to_lowercase()) {
                        continue;
                    }
                    let mut v = params.get(p).copied().unwrap_or(0.0);
                    let mapping = st.map_learn.filter(|(c, _)| *c == ch);
                    let name_resp = ui.add(egui::Label::new(&name).sense(egui::Sense::click()));
                    let resp = match live.param_steps(p) {
                        Some(steps) if steps <= 12 && steps > 1 => {
                            let idx = (v * (steps - 1) as f32).round() as usize;
                            let mut new_idx = idx;
                            let r = egui::ComboBox::from_id_salt(("pc", ch, slot, p))
                                .selected_text(live.display(p, v))
                                .show_ui(ui, |ui| {
                                    for k in 0..steps {
                                        let kv = k as f32 / (steps - 1) as f32;
                                        if ui.selectable_label(k == idx, live.display(p, kv)).clicked() {
                                            new_idx = k;
                                        }
                                    }
                                })
                                .response;
                            if new_idx != idx {
                                v = new_idx as f32 / (steps - 1) as f32;
                                core.set_fx_param(ch, slot, p, v);
                            }
                            r
                        }
                        _ => {
                            let r = ui.add(egui::Slider::new(&mut v, 0.0..=1.0).show_value(false));
                            if r.changed() {
                                core.set_fx_param(ch, slot, p, v);
                            }
                            r
                        }
                    };
                    learnable(core, &resp, &format!("Mixer/CH{}/FX/Slot/{}/Param/{}", ch + 1, slot + 1, p + 1));
                    if let Some((_, mi)) = mapping {
                        if resp.changed() || resp.clicked() || name_resp.clicked() {
                            core.assign_macro(ch, mi, slot, p);
                            st.map_learn = None;
                            core.log(format!("Macro {} → {} {}", mi + 1, slot + 1, name));
                        }
                    }
                    ui.label(RichText::new(live.display(p, v)).monospace().small());
                    ui.end_row();
                }
            });
        });
        if live.is_vst3() {
            ui.label(
                RichText::new("Generic editor. Plugin GUIs are not embedded in this version; all automatable parameters are shown.")
                    .small()
                    .weak(),
            );
        }
    });
    ui.data_mut(|d| d.insert_temp(filter_id, filter));
}

fn insert_browser(ctx: &egui::Context, core: &mut Core, st: &mut UiState) {
    let Some(b) = st.insert_browser.as_mut() else { return };
    let mut open = true;
    let mut chosen: Option<String> = None;
    let title = match b.replace {
        Some(s) => format!("Replace CH {} slot {}", b.ch + 1, s + 1),
        None => format!("Add insert · CH {}", b.ch + 1),
    };
    egui::Window::new(title).open(&mut open).default_size([420.0, 480.0]).show(ctx, |ui| {
        ui.horizontal(|ui| {
            if b.replace.is_none() {
                ui.selectable_value(&mut b.pos, InsertPos::Pre, "PRE");
                ui.selectable_value(&mut b.pos, InsertPos::Post, "POST");
                ui.separator();
            }
            ui.label("🔍");
            ui.text_edit_singleline(&mut b.query);
        });
        let mut cats: Vec<String> =
            core.catalog_native.iter().chain(core.catalog_vst3.iter()).map(|e| e.category.clone()).collect();
        cats.sort();
        cats.dedup();
        ui.horizontal_wrapped(|ui| {
            if ui.selectable_label(b.category.is_none(), "All").clicked() {
                b.category = None;
            }
            for c in cats {
                if ui.selectable_label(b.category.as_deref() == Some(c.as_str()), &c).clicked() {
                    b.category = Some(c);
                }
            }
        });
        ui.separator();
        let q = b.query.to_lowercase();
        let entries: Vec<_> = core
            .catalog_native
            .iter()
            .chain(core.catalog_vst3.iter())
            .filter(|e| q.is_empty() || e.name.to_lowercase().contains(&q) || e.vendor.to_lowercase().contains(&q))
            .filter(|e| b.category.as_ref().map(|c| &e.category == c).unwrap_or(true))
            .cloned()
            .collect();
        egui::ScrollArea::vertical().max_height(340.0).show(ui, |ui| {
            let mut vendor = String::new();
            for e in entries {
                if e.vendor != vendor {
                    vendor = e.vendor.clone();
                    ui.label(RichText::new(&vendor).strong().color(ACCENT));
                }
                ui.horizontal(|ui| {
                    if ui.button(&e.name).clicked() {
                        chosen = Some(e.id.clone());
                    }
                    ui.label(RichText::new(&e.category).small().weak());
                    if e.vst3.is_some() {
                        ui.label(RichText::new("VST3").small().weak());
                    }
                });
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            if !core.settings.vst3_enabled {
                ui.label(RichText::new("VST3 support is off (Settings)").weak());
            } else if core.vst3_scanning {
                ui.spinner();
                ui.label("Scanning VST3 plugins…");
            } else {
                ui.label(RichText::new(format!("{} VST3 effects", core.catalog_vst3.len())).weak());
                if ui.button("Rescan").clicked() {
                    core.rescan_vst3();
                }
            }
            if !core.vst3_errors.is_empty() {
                ui.label(RichText::new(format!("{} failed", core.vst3_errors.len())).color(WARN)).on_hover_ui(|ui| {
                    for (p, e) in &core.vst3_errors {
                        ui.label(format!("{}: {e}", p.display()));
                    }
                });
            }
        });
    });
    if let Some(id) = chosen {
        let b = st.insert_browser.take().unwrap();
        let res = match b.replace {
            Some(slot) => core.replace_insert(b.ch, slot, &id).map(|_| slot),
            None => core.add_insert(b.ch, &id, b.pos),
        };
        match res {
            Ok(slot) => {
                st.fx_edit[b.ch][slot] = true;
                st.fx_open[b.ch] = true;
            }
            Err(e) => core.log(format!("Could not load effect: {e}")),
        }
    } else if !open {
        st.insert_browser = None;
    }
}

pub fn open_path(p: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = "xdg-open";
    let _ = std::process::Command::new(cmd).arg(p).spawn();
}
