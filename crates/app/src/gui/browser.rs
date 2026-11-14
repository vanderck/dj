//! Library browser with harmonic hints from your Mixed In Key tags.

use eframe::egui::{self, Color32, RichText, Ui};

use collective_library::key::Compat;

use super::widgets::*;
use super::UiState;
use crate::core::Core;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Sort {
    #[default]
    Artist,
    Title,
    Bpm,
    Key,
    Energy,
}

pub fn show(ui: &mut Ui, core: &mut Core, _st: &mut UiState) {
    let sort_id = egui::Id::unique("browser-sort");
    let (mut sort, mut asc): (Sort, bool) = ui.data_mut(|d| d.get_temp(sort_id).unwrap_or((Sort::Artist, true)));
    let mut only_matches: bool = ui.data_mut(|d| d.get_temp(egui::Id::unique("browser-harm")).unwrap_or(false));
    ui.horizontal(|ui| {
        ui.label(RichText::new("BROWSER").strong());
        ui.label("🔍");
        ui.add(egui::TextEdit::singleline(&mut core.browser_query).desired_width(220.0).hint_text("title, artist, 8A, Am…"));
        ui.checkbox(&mut only_matches, "Harmonic only").on_hover_text("Only keys compatible with the master deck");
        if core.library_scanning {
            ui.spinner();
            ui.label("Scanning…");
        } else if ui.button("Rescan").clicked() {
            core.rescan_library();
        }
        let sel = core.browser_selected.and_then(|i| core.library.get(i)).map(|e| e.path.clone());
        if let Some(p) = sel {
            ui.separator();
            ui.label("Load to:");
            for d in 0..core.deck_count() {
                if ui.add(egui::Button::new(RichText::new(format!("{}", d + 1)).color(Color32::BLACK)).fill(DECK_COLORS[d])).clicked() {
                    core.load_track(d, &p);
                }
            }
        }
    });
    ui.data_mut(|d| d.insert_temp(egui::Id::unique("browser-harm"), only_matches));
    let reference = core.reference_key();
    let mut rows = core.filtered_library();
    if only_matches {
        if let Some(r) = reference {
            rows.retain(|&i| core.library[i].key.map(|k| k.compat(r).is_good()).unwrap_or(false));
        }
    }
    {
        let lib = &core.library;
        rows.sort_by(|&a, &b| {
            let (x, y) = (&lib[a], &lib[b]);
            let o = match sort {
                Sort::Artist => x.artist.to_lowercase().cmp(&y.artist.to_lowercase()).then(x.title.cmp(&y.title)),
                Sort::Title => x.title.to_lowercase().cmp(&y.title.to_lowercase()),
                Sort::Bpm => x.bpm.unwrap_or(0.0).total_cmp(&y.bpm.unwrap_or(0.0)),
                Sort::Key => x.key.map(|k| (k.camelot_number(), k.minor)).cmp(&y.key.map(|k| (k.camelot_number(), k.minor))),
                Sort::Energy => x.energy.cmp(&y.energy),
            };
            if asc {
                o
            } else {
                o.reverse()
            }
        });
    }
    let header = |ui: &mut Ui, label: &str, s: Sort, sort: &mut Sort, asc: &mut bool| {
        let arrow = if *sort == s { if *asc { " ▲" } else { " ▼" } } else { "" };
        if ui.add(egui::Label::new(RichText::new(format!("{label}{arrow}")).strong()).sense(egui::Sense::click())).clicked() {
            if *sort == s {
                *asc = !*asc;
            } else {
                *sort = s;
                *asc = true;
            }
        }
    };
    let row_h = 20.0;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_h, rows.len() + 1, |ui, range| {
        egui::Grid::new("lib").num_columns(6).striped(true).min_col_width(40.0).show(ui, |ui| {
            if range.start == 0 {
                header(ui, "Title", Sort::Title, &mut sort, &mut asc);
                header(ui, "Artist", Sort::Artist, &mut sort, &mut asc);
                header(ui, "BPM", Sort::Bpm, &mut sort, &mut asc);
                header(ui, "Key", Sort::Key, &mut sort, &mut asc);
                header(ui, "Energy", Sort::Energy, &mut sort, &mut asc);
                ui.label(RichText::new("Cues").strong());
                ui.end_row();
            }
            for r in range.start.max(1)..range.end {
                let i = rows[r - 1];
                let e = core.library[i].clone();
                let selected = core.browser_selected == Some(i);
                let title = ui.selectable_label(selected, &e.title);
                if title.clicked() {
                    core.browser_selected = Some(i);
                }
                if title.double_clicked() {
                    // Load to the first stopped deck.
                    let d = (0..core.deck_count()).find(|&d| !core.snap.decks[d].playing).unwrap_or(0);
                    core.load_track(d, &e.path);
                }
                title.context_menu(|ui| {
                    for d in 0..core.deck_count() {
                        if ui.button(format!("Load to deck {}", d + 1)).clicked() {
                            core.load_track(d, &e.path);
                            ui.close();
                        }
                    }
                    ui.separator();
                    for d in 0..core.deck_count() {
                        if core.session.decks[d].mode == collective_engine::engine::DeckMode::Sampler {
                            let lane = core.session.decks[d].sampler.selected_lane;
                            if ui.button(format!("Load into sampler {} lane {}", d + 1, lane + 1)).clicked() {
                                core.load_sample(d, lane, &e.path);
                                ui.close();
                            }
                        }
                    }
                });
                ui.label(&e.artist);
                ui.label(e.bpm.map(|b| format!("{b:.1}")).unwrap_or_default());
                let kt = core.key_text(e.key);
                let col = match (e.key, reference) {
                    (Some(k), Some(r)) => match k.compat(r) {
                        Compat::Same | Compat::Adjacent | Compat::Relative => OK,
                        Compat::Boost => ACCENT,
                        _ => ui.visuals().text_color(),
                    },
                    _ => ui.visuals().text_color(),
                };
                ui.label(RichText::new(kt).color(col));
                ui.label(e.energy.map(|x| x.to_string()).unwrap_or_default());
                ui.label(if e.cue_count > 0 { e.cue_count.to_string() } else { String::new() });
                ui.end_row();
            }
        });
    });
    if core.library.is_empty() && !core.library_scanning {
        ui.label(RichText::new("No tracks. Add your music folders in Settings → Library.").weak());
    }
    ui.data_mut(|d| d.insert_temp(sort_id, (sort, asc)));
}
