//! Library browser.

use std::collections::HashMap;
use std::path::PathBuf;

use eframe::egui::{self, Align2, Color32, CornerRadius, Pos2, Rect, Sense, Ui, UiBuilder, Vec2};

use dj_engine::engine::DeckMode;
use dj_library::key::Compat;
use dj_library::scan::Entry;

use super::skin::*;
use super::UiState;
use crate::core::Core;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Col {
    #[default]
    Artist,
    Title,
    Bpm,
    Key,
    Energy,
    Time,
    Genre,
}

/// (column, title, width fraction, right-aligned)
const COLS: [(Option<Col>, &str, f32, bool); 10] = [
    (None, "", 0.025, false),
    (Some(Col::Title), "Title", 0.27, false),
    (Some(Col::Artist), "Artist", 0.215, false),
    (Some(Col::Bpm), "BPM", 0.06, true),
    (Some(Col::Key), "Key", 0.085, false),
    (None, "", 0.05, false),
    (Some(Col::Energy), "Energy", 0.09, false),
    (Some(Col::Time), "Time", 0.04, true),
    (Some(Col::Genre), "Genre", 0.1, false),
    (None, "Load to", 0.065, false),
];

/// Fits: tempo window around the reference deck's BPM.
use crate::core::automix::{fit_score, key_rank, mix_bonus, FITS_PCT};

fn mmss(s: Option<f64>) -> String {
    s.map(|s| format!("{}:{:02}", (s / 60.0) as u32, (s % 60.0) as u32)).unwrap_or_default()
}

pub fn show(ui: &mut Ui, core: &mut Core, st: &mut UiState, rect: Rect) {
    ui.allocate_rect(rect, Sense::hover());
    card(ui, rect);
    let r = rect.shrink2(Vec2::new(16.0, 12.0));

    // ---------------------------------------------------------- top bar
    let bar_h = 34.0;
    let y = r.top();
    ui.painter().text(Pos2::new(r.left(), y + bar_h / 2.0), Align2::LEFT_CENTER, "Search", sans(13.0), TEXT_DIM);
    let search_r = Rect::from_min_size(Pos2::new(r.left() + 58.0, y), Vec2::new((r.width() * 0.32).min(440.0), bar_h));
    ui.painter().rect_filled(search_r, CornerRadius::same((bar_h / 2.0) as u8), WELL);
    ui.painter().rect_stroke(search_r, CornerRadius::same((bar_h / 2.0) as u8), egui::Stroke::new(1.0, BORDER), egui::StrokeKind::Inside);
    let mut child = ui.new_child(UiBuilder::new().max_rect(search_r.shrink2(Vec2::new(14.0, 7.0))));
    child.add(
        egui::TextEdit::singleline(&mut core.browser_query)
            .frame(egui::Frame::NONE)
            .font(sans(14.0))
            .hint_text("Artist, title, label, 8A, Am")
            .desired_width(search_r.width() - 28.0),
    );
    let mut x = search_r.right() + 12.0;
    for (label, hist) in [("All tracks", false), ("History", true)] {
        let w = pill_width(ui, label, 13.0);
        let pr = Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, bar_h));
        if pill(ui, pr, label, st.browser_hist == hist).clicked() {
            st.browser_hist = hist;
            // All tracks means all of them: a search left in the field would hide most.
            if !hist {
                core.browser_query.clear();
            }
        }
        x += w + 8.0;
    }
    // Harmonic filter against the master / playing deck.
    let reference = core.reference_key();
    let ref_deck = match core.snap.clock.owner {
        dj_engine::engine::ClockOwner::Deck(d) => Some(d as usize),
        _ => (0..core.deck_count()).find(|&d| core.snap.decks[d].playing),
    };
    // The reference tempo: the deck's current BPM (what a new track has to match).
    let ref_bpm = ref_deck.map(|d| core.snap.decks[d].bpm).filter(|b| *b > 0.0);
    let fits_label = match (ref_deck, reference) {
        (Some(d), Some(k)) => match ref_bpm {
            Some(b) => format!("Fits deck {} · {} · {:.0} BPM", d + 1, core.key_text(Some(k)), b),
            None => format!("Fits deck {} · {}", d + 1, core.key_text(Some(k))),
        },
        _ => "Fits —".into(),
    };
    let w = pill_width(ui, &fits_label, 13.0) + 14.0;
    let fr = Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, bar_h));
    let resp = ui.allocate_rect(fr, Sense::click()).on_hover_text(format!(
        "What to play next: tracks in a compatible key within {FITS_PCT:.0} % of the master deck's tempo, best first: the key, the tempo, the energy (level or a step up), and for prepared tracks how well dj would actually mix them in (hover a track for how)"
    ));
    let p = ui.painter();
    let rad = CornerRadius::same((bar_h / 2.0) as u8);
    p.rect_filled(fr, rad, if st.browser_harmonic { LIGHT } else if resp.hovered() { CTRL_HOVER } else { CTRL });
    p.circle_filled(Pos2::new(fr.left() + 16.0, fr.center().y), 4.5, GREEN);
    p.text(Pos2::new(fr.left() + 27.0, fr.center().y), Align2::LEFT_CENTER, &fits_label, bold(13.0), if st.browser_harmonic { on(LIGHT) } else { TEXT });
    if resp.clicked() && reference.is_some() {
        st.browser_harmonic = !st.browser_harmonic;
    }

    let imp_w = pill_width(ui, "Import folder", 13.0);
    let ir = Rect::from_min_size(Pos2::new(r.right() - imp_w, y), Vec2::new(imp_w, bar_h));
    if pill(ui, ir, "Import folder", false).on_hover_text("Choose a music folder (all its subfolders are included)").clicked() {
        // The system folder picker (on macOS this also grants access to an external drive).
        match rfd::FileDialog::new().set_title("Import music folder").pick_folder() {
            Some(dir) => core.add_library_folder(dir),
            None if core.settings.library_folders.is_empty() => st.settings_open = true,
            None => {}
        }
    }
    let info = match core.status.back() {
        Some((t, m)) if t.elapsed().as_secs() < 6 => m.clone(),
        _ => format!(
            "{} tracks · {} Hz{}",
            core.library.len(),
            core.sample_rate as u32,
            if core.library_scanning { " · scanning…" } else { "" }
        ),
    };
    // Prepare: the tracks listed, ready to mix (stems, sections, mix profiles) in the background.
    let prep = core.preparer.as_ref().map(|p| p.state());
    let prep_label = match &prep {
        Some(p) => format!("Preparing {}/{}", (p.done + 1).min(p.total), p.total),
        None => "Prepare".into(),
    };
    let pw = pill_width(ui, &prep_label, 13.0);
    let prr = Rect::from_min_size(Pos2::new(ir.left() - 8.0 - pw, y), Vec2::new(pw, bar_h));
    let hover = match &prep {
        Some(p) => format!("{} ({:.0} %) · click to stop", p.current, p.fraction * 100.0),
        None => "Get the tracks listed ready to mix, in the background: their stems (HT-Demucs), sections and mix profiles, so they load ready and Fits can score how they'd mix. Already prepared tracks are skipped.".into(),
    };
    if pill(ui, prr, &prep_label, prep.is_some()).on_hover_text(hover).clicked() {
        if prep.is_some() {
            core.stop_preparing();
        } else {
            let paths: Vec<PathBuf> = if st.browser_hist {
                core.history.iter().rev().cloned().collect()
            } else {
                core.filtered_library().into_iter().map(|i| core.library[i].path.clone()).collect()
            };
            core.prepare_tracks(paths);
        }
    }
    if prep.is_some() {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }
    // Auto-DJ: the tracks listed, played as a set on decks 1 and 2.
    let auto_label = match &core.autodj {
        Some(a) => format!("Auto-DJ · {} left", a.left()),
        None => "Auto-DJ".into(),
    };
    let aw = pill_width(ui, &auto_label, 13.0);
    let ar = Rect::from_min_size(Pos2::new(prr.left() - 8.0 - aw, y), Vec2::new(aw, bar_h));
    let auto_hover = if core.autodj.is_some() {
        "Click to stop (the playing track plays on)"
    } else {
        "Play the tracks listed as a set on decks 1 and 2: dj picks each next track (key, tempo, energy, and how well it would mix in: as Fits) and mixes it in at the best moment. Prepare them first so each is ready at once."
    };
    if pill(ui, ar, &auto_label, core.autodj.is_some()).on_hover_text(auto_hover).clicked() {
        if core.autodj.is_some() {
            core.set_autodj(None);
        } else {
            let paths: Vec<PathBuf> = if st.browser_hist {
                core.history.iter().rev().cloned().collect()
            } else {
                core.filtered_library().into_iter().map(|i| core.library[i].path.clone()).collect()
            };
            core.set_autodj(Some(paths));
        }
    }
    let pills_left = super::mixer::global_pills(ui, core, st, ar.left() - 8.0, y, bar_h);
    let info_r = Rect::from_min_max(Pos2::new(fr.right() + 12.0, y), Pos2::new(pills_left - 12.0, y + bar_h));
    ui.painter_at(info_r).text(Pos2::new(info_r.right(), info_r.center().y), Align2::RIGHT_CENTER, info, sans(12.5), TEXT_DIM);

    // ---------------------------------------------------------- rows
    let mut rows: Vec<usize> = if st.browser_hist {
        core.history.iter().rev().filter_map(|p| core.library.iter().position(|e| &e.path == p)).collect()
    } else {
        core.filtered_library()
    };
    // Fits: compatible key and close tempo, best match first; tracks already on a deck are left out.
    let fits = st.browser_harmonic && reference.is_some();
    if let (true, Some(rk)) = (fits, reference) {
        let loaded: Vec<PathBuf> = (0..core.deck_count()).filter_map(|d| core.decks[d].info.as_ref().map(|i| i.path.clone())).collect();
        let lib = &core.library;
        rows.retain(|&i| {
            let e = &lib[i];
            let key_ok = e.key.is_some_and(|k| key_rank(k.compat(rk)).is_some());
            let bpm_ok = match (ref_bpm, e.bpm) {
                (Some(r), Some(b)) => 100.0 * (b / r - 1.0).abs() <= FITS_PCT,
                (Some(_), None) => false,
                (None, _) => true,
            };
            key_ok && bpm_ok && !loaded.contains(&e.path)
        });
        let ref_energy = ref_deck.and_then(|d| core.deck_energy(d));
        let base: HashMap<usize, f64> = rows.iter().map(|&i| (i, fit_score(&core.library[i], rk, ref_bpm, ref_energy))).collect();
        rows.sort_by(|a, b| base[a].total_cmp(&base[b]));
        // The best few by key, tempo and energy: how well they'd actually mix (prepared tracks),
        // a few per frame so the list never stalls.
        if let Some(d) = ref_deck {
            let started = std::time::Instant::now();
            let mut pending = false;
            for &i in rows.iter().take(40) {
                let path = core.library[i].path.clone();
                if core.mix_score_known(d, &path).is_none() {
                    if started.elapsed().as_millis() > 6 {
                        pending = true;
                        break;
                    }
                    core.mix_score(d, &path);
                }
            }
            if pending {
                ui.ctx().request_repaint();
            }
            let fit = |i: usize| base[&i] - core.mix_score_known(d, &core.library[i].path).flatten().map_or(0.0, |s| mix_bonus(s.score));
            rows.sort_by(|&a, &b| fit(a).total_cmp(&fit(b)));
        }
    }
    // Focus the best match whenever it changes (Fits switched on, another reference, ...).
    let best = if fits { rows.first().copied() } else { None };
    let refocus = best.is_some() && best != st.fits_focus;
    st.fits_focus = best;
    if refocus {
        core.browser_selected = best;
    }
    if !st.browser_hist && !fits {
        let lib = &core.library;
        let (col, asc) = (st.browser_sort, st.browser_asc);
        rows.sort_by(|&a, &b| {
            let (x, y): (&Entry, &Entry) = (&lib[a], &lib[b]);
            let o = match col {
                Col::Artist => x.artist.to_lowercase().cmp(&y.artist.to_lowercase()).then(x.title.cmp(&y.title)),
                Col::Title => x.title.to_lowercase().cmp(&y.title.to_lowercase()),
                Col::Bpm => x.bpm.unwrap_or(0.0).total_cmp(&y.bpm.unwrap_or(0.0)),
                Col::Key => x.key.map(|k| (k.camelot_number(), k.minor)).cmp(&y.key.map(|k| (k.camelot_number(), k.minor))),
                Col::Energy => x.energy.cmp(&y.energy),
                Col::Time => x.duration_secs.unwrap_or(0.0).total_cmp(&y.duration_secs.unwrap_or(0.0)),
                Col::Genre => x.genre.to_lowercase().cmp(&y.genre.to_lowercase()),
            };
            if asc {
                o
            } else {
                o.reverse()
            }
        });
    }

    let table = Rect::from_min_max(Pos2::new(r.left(), y + bar_h + 14.0), r.right_bottom());
    let widths: Vec<f32> = COLS.iter().map(|c| c.2 * table.width()).collect();
    let head = Rect::from_min_size(table.min, Vec2::new(table.width(), 22.0));
    let mut x = head.left();
    for (i, (col, title, _, right)) in COLS.iter().enumerate() {
        let cr = Rect::from_min_size(Pos2::new(x, head.top()), Vec2::new(widths[i] - 10.0, head.height()));
        x += widths[i];
        if title.is_empty() {
            continue;
        }
        let arrow = match col {
            Some(c) if *c == st.browser_sort && !st.browser_hist => if st.browser_asc { " ↑" } else { " ↓" },
            _ => "",
        };
        let (pos, align) = if *right { (cr.right_center(), Align2::RIGHT_CENTER) } else { (cr.left_center(), Align2::LEFT_CENTER) };
        ui.painter().text(pos, align, format!("{title}{arrow}"), sans(12.5), TEXT_DIM);
        if let Some(c) = col {
            if ui.allocate_rect(cr, Sense::click()).clicked() {
                if st.browser_sort == *c {
                    st.browser_asc = !st.browser_asc;
                } else {
                    st.browser_sort = *c;
                    st.browser_asc = true;
                }
            }
        }
    }
    ui.painter().line_segment([head.left_bottom() + Vec2::new(0.0, 3.0), head.right_bottom() + Vec2::new(0.0, 3.0)], egui::Stroke::new(1.0, BORDER));

    let body = Rect::from_min_max(Pos2::new(table.left(), head.bottom() + 6.0), table.right_bottom());
    let row_h = 30.0;
    // Which deck holds which track.
    let on_deck: Vec<(usize, PathBuf)> = (0..core.deck_count()).filter_map(|d| core.decks[d].info.as_ref().map(|i| (d, i.path.clone()))).collect();
    let mut child = ui.new_child(UiBuilder::new().max_rect(body));
    let ctx = ui.ctx().clone();
    let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
    if refocus {
        scroll = scroll.vertical_scroll_offset(0.0);
    }
    scroll.show_rows(&mut child, row_h, rows.len(), |ui, range| {
        for ri in range {
            let i = rows[ri];
            let e = core.library[i].clone();
            let (rr, resp) = ui.allocate_exact_size(Vec2::new(body.width(), row_h), Sense::click_and_drag());
            let selected = core.browser_selected == Some(i);
            let hovered = resp.hovered() || resp.contains_pointer();
            if selected || hovered {
                ui.painter().rect_filled(rr, CornerRadius::same(8), if selected { Color32::from_rgb(0x2a, 0x2a, 0x31) } else { Color32::from_rgb(0x22, 0x22, 0x28) });
            }
            if best == Some(i) {
                ui.painter().rect_stroke(rr, CornerRadius::same(8), egui::Stroke::new(1.5, GREEN), egui::StrokeKind::Inside);
            }
            // Fits: how dj would mix a prepared track in, on hover; a green dot marks those.
            let mix = if fits { ref_deck.and_then(|d| core.mix_score_known(d, &e.path).flatten()) } else { None };
            let resp = match &mix {
                Some(m) => resp.on_hover_text(format!("Mixes in well: {}", m.why)),
                None => resp,
            };
            let mut x = rr.left();
            let cell = |i: usize, x: f32| Rect::from_min_size(Pos2::new(x, rr.top()), Vec2::new(widths[i] - 10.0, rr.height()));
            if mix.is_some() && !on_deck.iter().any(|(_, p)| *p == e.path) {
                ui.painter().circle_filled(Pos2::new(x + 12.0, rr.center().y), 4.0, GREEN);
            }
            // Deck badge.
            if let Some((d, _)) = on_deck.iter().find(|(_, p)| *p == e.path) {
                let b = Rect::from_center_size(Pos2::new(x + 12.0, rr.center().y), Vec2::splat(20.0));
                deck_badge(ui, b, *d, 11.0);
            }
            x += widths[0];
            let painter = ui.painter().clone();
            let text = |c: Rect, s: &str, font: egui::FontId, col: Color32, right: bool| {
                let p = painter.with_clip_rect(c);
                if right {
                    p.text(c.right_center(), Align2::RIGHT_CENTER, s, font, col);
                } else {
                    p.text(c.left_center(), Align2::LEFT_CENTER, s, font, col);
                }
            };
            text(cell(1, x), &e.title, bold(14.0), TEXT, false);
            x += widths[1];
            text(cell(2, x), &e.artist, sans(14.0), TEXT, false);
            x += widths[2];
            text(cell(3, x), &e.bpm.map(|b| format!("{b:.2}")).unwrap_or_default(), mono(14.0), TEXT, true);
            x += widths[3];
            if let Some(k) = e.key {
                let c = cell(4, x);
                text(c, &k.camelot(), mono(14.0), TEXT, false);
                text(Rect::from_min_max(Pos2::new(c.left() + 44.0, c.top()), c.right_bottom()), &k.musical(), sans(13.5), TEXT_DIM, false);
            }
            x += widths[4];
            if let (Some(k), Some(rk)) = (e.key, reference) {
                let tag = match k.compat(rk) {
                    Compat::Same | Compat::Adjacent | Compat::Relative => Some("Match"),
                    Compat::Boost => Some("Boost"),
                    _ => None,
                };
                if let Some(t) = tag {
                    text(cell(5, x), t, bold(12.0), GREEN, false);
                }
            }
            x += widths[5];
            if let Some(en) = e.energy {
                let c = cell(6, x);
                let bar = Rect::from_min_size(Pos2::new(c.left(), c.center().y - 3.0), Vec2::new((c.width() - 26.0).max(20.0), 6.0));
                painter.rect_filled(bar, CornerRadius::same(3), Color32::from_gray(50));
                painter.rect_filled(Rect::from_min_size(bar.min, Vec2::new(bar.width() * en as f32 / 10.0, 6.0)), CornerRadius::same(3), Color32::from_gray(200));
                text(Rect::from_min_max(Pos2::new(bar.right() + 8.0, c.top()), c.right_bottom()), &en.to_string(), mono(13.0), TEXT, false);
            }
            x += widths[6];
            text(cell(7, x), &mmss(e.duration_secs), mono(14.0), TEXT, true);
            x += widths[7];
            text(cell(8, x), &e.genre, sans(14.0), TEXT, false);
            x += widths[8];
            // Load-to badges on hover.
            if hovered || selected {
                let n = core.deck_count();
                for d in 0..n {
                    let b = Rect::from_center_size(Pos2::new(rr.right() - 18.0 - (n - 1 - d) as f32 * 30.0, rr.center().y), Vec2::splat(24.0));
                    deck_badge(ui, b, d, 12.0);
                    // Clickable without taking layout space: allocating here moved the rows below
                    // (and their ids) whenever a row was hovered, so the list jittered under the
                    // cursor.
                    if ui.interact(b, ui.scope_id().with(("load-to", i, d)), Sense::click()).clicked() {
                        if core.session.decks[d].mode == DeckMode::Sampler {
                            let lane = core.session.decks[d].sampler.selected_lane;
                            core.load_sample(d, lane, &e.path);
                        } else {
                            core.request_load(d, &e.path);
                        }
                    }
                }
            }
            let _ = x;
            if resp.clicked() {
                core.browser_selected = Some(i);
            }
            if resp.double_clicked() {
                let d = (0..core.deck_count())
                    .find(|&d| !core.snap.decks[d].playing && core.session.decks[d].mode == DeckMode::Track)
                    .unwrap_or(0);
                core.request_load(d, &e.path);
            }
            if resp.drag_started() {
                egui::DragAndDrop::set_payload::<PathBuf>(&ctx, e.path.clone());
                core.browser_selected = Some(i);
            }
        }
    });
    if let Some(p) = egui::DragAndDrop::payload::<PathBuf>(&ctx) {
        if let Some(pos) = ctx.pointer_hover_pos() {
            let name = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            ctx.debug_painter().text(pos + Vec2::new(14.0, 0.0), Align2::LEFT_CENTER, name, bold(13.0), TEXT);
        }
    }
    if rows.is_empty() && !core.library_scanning {
        let msg = if st.browser_hist { "Tracks you load appear here." } else { "No tracks yet — press Import folder." };
        ui.painter().text(body.center(), Align2::CENTER_CENTER, msg, sans(13.0), TEXT_DIM);
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use dj_library::key::Key;

    fn entry(key: &str, bpm: f64) -> Entry {
        Entry { key: Key::parse(key), bpm: Some(bpm), ..Default::default() }
    }

    #[test]
    fn fits_ranks_key_first_then_tempo() {
        let r = Key::parse("8A").unwrap();
        let same_far = fit_score(&entry("8A", 128.0), r, Some(124.0), None);
        let same_near = fit_score(&entry("8A", 124.5), r, Some(124.0), None);
        let adjacent_exact = fit_score(&entry("9A", 124.0), r, Some(124.0), None);
        let boost = fit_score(&entry("10A", 124.0), r, Some(124.0), None);
        assert!(same_near < same_far && same_far < adjacent_exact && adjacent_exact < boost);
        assert_eq!(key_rank(Key::parse("8B").unwrap().compat(r)), Some(1)); // relative
        assert_eq!(key_rank(Key::parse("3A").unwrap().compat(r)), None); // clash
    }

    #[test]
    fn fits_prefers_energy_level_or_a_step_up_and_tracks_that_mix_well() {
        let r = Key::parse("8A").unwrap();
        let with = |energy: u8| fit_score(&Entry { energy: Some(energy), ..entry("8A", 124.0) }, r, Some(124.0), Some(6));
        assert_eq!(with(6), with(7), "level or one up");
        assert!(with(7) < with(5) && with(5) < with(9), "down a little, then a jump");
        // A prepared track that mixes in well comes ahead of one a little closer in tempo.
        let near = fit_score(&entry("8A", 124.0), r, Some(124.0), None);
        let prepared = fit_score(&entry("8A", 125.5), r, Some(124.0), None) - mix_bonus(15.0);
        assert!(prepared < near, "{prepared} vs {near}");
        assert_eq!(mix_bonus(-3.0), 0.0);
    }
}
