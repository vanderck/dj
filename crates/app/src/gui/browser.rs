//! Library browser.

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
const FITS_BPM: f64 = 5.0;

/// Key fit, best first; `None` = does not mix.
fn key_rank(c: Compat) -> Option<u8> {
    match c {
        Compat::Same => Some(0),
        Compat::Relative | Compat::Adjacent => Some(1),
        Compat::Boost => Some(2),
        Compat::Diagonal | Compat::Clash => None,
    }
}

/// Lower is better: the key fit first, then the tempo distance (each key step outweighs the
/// whole ±5 BPM window).
fn fit_score(e: &Entry, reference: dj_library::key::Key, ref_bpm: Option<f64>) -> f64 {
    let rank = e.key.and_then(|k| key_rank(k.compat(reference))).unwrap_or(9) as f64;
    let bpm = match (ref_bpm, e.bpm) {
        (Some(r), Some(b)) => (b - r).abs(),
        _ => 0.0,
    };
    rank * 2.0 * FITS_BPM + bpm
}

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
    let resp = ui.allocate_rect(fr, Sense::click()).on_hover_text(format!("Tracks that mix with the master deck: a compatible key and within ±{FITS_BPM:.0} BPM, best match first"));
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
    let pills_left = super::mixer::global_pills(ui, core, st, ir.left() - 8.0, y, bar_h);
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
                (Some(r), Some(b)) => (b - r).abs() <= FITS_BPM,
                (Some(_), None) => false,
                (None, _) => true,
            };
            key_ok && bpm_ok && !loaded.contains(&e.path)
        });
        rows.sort_by(|&a, &b| fit_score(&lib[a], rk, ref_bpm).total_cmp(&fit_score(&lib[b], rk, ref_bpm)));
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
            let mut x = rr.left();
            let cell = |i: usize, x: f32| Rect::from_min_size(Pos2::new(x, rr.top()), Vec2::new(widths[i] - 10.0, rr.height()));
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
                    if ui.allocate_rect(b, Sense::click()).clicked() {
                        if core.session.decks[d].mode == DeckMode::Sampler {
                            let lane = core.session.decks[d].sampler.selected_lane;
                            core.load_sample(d, lane, &e.path);
                        } else {
                            core.load_track(d, &e.path);
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
                core.load_track(d, &e.path);
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
        let same_far = fit_score(&entry("8A", 128.0), r, Some(124.0));
        let same_near = fit_score(&entry("8A", 124.5), r, Some(124.0));
        let adjacent_exact = fit_score(&entry("9A", 124.0), r, Some(124.0));
        let boost = fit_score(&entry("10A", 124.0), r, Some(124.0));
        assert!(same_near < same_far && same_far < adjacent_exact && adjacent_exact < boost);
        assert_eq!(key_rank(Key::parse("8B").unwrap().compat(r)), Some(1)); // relative
        assert_eq!(key_rank(Key::parse("3A").unwrap().compat(r)), None); // clash
    }
}
