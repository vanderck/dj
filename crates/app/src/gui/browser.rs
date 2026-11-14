//! Library browser: sidebar, search, sortable table, drag rows onto decks.

use std::path::PathBuf;

use eframe::egui::{self, Align2, Color32, CornerRadius, Pos2, Rect, Sense, Ui, UiBuilder, Vec2};

use collective_library::scan::Entry;

use super::skin::*;
use super::UiState;
use crate::core::Core;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Col {
    #[default]
    Artist,
    Title,
    Album,
    Label,
    Bpm,
    Duration,
    Key,
    Genre,
    Plays,
    Added,
}

const COLS: [(Col, &str, f32); 10] = [
    (Col::Artist, "ARTIST", 0.13),
    (Col::Title, "TITLE", 0.15),
    (Col::Album, "ALBUM", 0.13),
    (Col::Label, "LABEL", 0.10),
    (Col::Bpm, "BPM", 0.06),
    (Col::Duration, "DURATION", 0.06),
    (Col::Key, "KEY", 0.05),
    (Col::Genre, "GENRE", 0.09),
    (Col::Plays, "PLAY COUNT", 0.08),
    (Col::Added, "DATE ADDED", 0.15),
];

/// Unix seconds → "YYYY-MM-DD HH:MM:SS" (UTC), no date library needed.
pub fn fmt_date(t: u64) -> String {
    if t == 0 {
        return String::new();
    }
    let days = (t / 86400) as i64;
    let secs = t % 86400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

fn mmss(s: Option<f64>) -> String {
    s.map(|s| format!("{}:{:02}", (s / 60.0) as u32, (s % 60.0) as u32)).unwrap_or_default()
}

pub fn show(ui: &mut Ui, core: &mut Core, st: &mut UiState, rect: Rect) {
    // Claim the whole panel so the (resizable) panel keeps its size.
    ui.allocate_rect(rect, Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::ZERO, BG);

    // ---- sidebar
    let side = Rect::from_min_size(rect.left_top() + Vec2::new(4.0, 4.0), Vec2::new(52.0, rect.height() - 8.0));
    panel(ui, side, PANEL);
    for (i, (lbl, hist)) in [("ALL", false), ("HIST", true)].iter().enumerate() {
        let r = Rect::from_min_size(side.left_top() + Vec2::new(4.0, 4.0 + i as f32 * 46.0), Vec2::new(side.width() - 8.0, 42.0));
        let on = st.browser_hist == *hist;
        let resp = btn_sized(ui, r, lbl, if on { Look::On(RED) } else { Look::On(PANEL) }, 9.5);
        if resp.clicked() {
            st.browser_hist = *hist;
        }
    }

    // ---- top bar
    let main = Rect::from_min_max(Pos2::new(side.right() + 6.0, rect.top() + 4.0), rect.right_bottom() - Vec2::new(4.0, 4.0));
    let bar = Rect::from_min_size(main.left_top(), Vec2::new(main.width(), 32.0));
    panel(ui, bar, PANEL);
    ui.painter().text(bar.left_center() + Vec2::new(14.0, 0.0), Align2::LEFT_CENTER, "SEARCH:", font(11.0), TEXT);
    let search_r = Rect::from_min_size(Pos2::new(bar.left() + 80.0, bar.top() + 5.0), Vec2::new((bar.width() * 0.3).min(360.0), 22.0));
    let mut child = ui.new_child(UiBuilder::new().max_rect(search_r));
    child.add_sized(search_r.size(), egui::TextEdit::singleline(&mut core.browser_query).hint_text("artist, title, label, 8A, Am…"));
    let clear_r = Rect::from_min_size(Pos2::new(search_r.right() + 6.0, bar.top() + 4.0), Vec2::new(64.0, 24.0));
    if btn(ui, clear_r, "CLEAR", Look::On(RED)).clicked() {
        core.browser_query.clear();
    }
    let harm_r = Rect::from_min_size(Pos2::new(clear_r.right() + 6.0, bar.top() + 4.0), Vec2::new(86.0, 24.0));
    if btn_sized(ui, harm_r, "HARMONIC", if st.browser_harmonic { Look::On(SILVER) } else { Look::Normal }, 9.5)
        .on_hover_text("Show only keys compatible with the master deck")
        .clicked()
    {
        st.browser_harmonic = !st.browser_harmonic;
    }
    if core.library_scanning {
        ui.painter().text(harm_r.right_center() + Vec2::new(10.0, 0.0), Align2::LEFT_CENTER, "SCANNING…", font(9.5), TEXT_DIM);
    }
    // Zoom slider and IMPORT on the right.
    let zoom_r = Rect::from_min_size(Pos2::new(bar.right() - 150.0, bar.top() + 3.0), Vec2::new(140.0, 26.0));
    ui.painter().text(zoom_r.left_top(), Align2::LEFT_TOP, "ZOOM", font(8.0), TEXT_DIM);
    let track = Rect::from_min_max(Pos2::new(zoom_r.left(), zoom_r.bottom() - 9.0), Pos2::new(zoom_r.right(), zoom_r.bottom() - 6.0));
    let zr = ui.allocate_rect(Rect::from_min_max(Pos2::new(zoom_r.left(), zoom_r.top() + 8.0), zoom_r.right_bottom()), Sense::click_and_drag());
    if zr.dragged() || zr.clicked() {
        if let Some(pos) = zr.interact_pointer_pos() {
            st.browser_zoom = (0.8 + (pos.x - track.left()) / track.width() * 0.8).clamp(0.8, 1.6);
        }
    }
    let p = ui.painter();
    p.rect_filled(track, CornerRadius::same(1), Color32::from_gray(50));
    let kx = track.left() + (st.browser_zoom - 0.8) / 0.8 * track.width();
    p.rect_filled(Rect::from_center_size(Pos2::new(kx, track.center().y), Vec2::new(8.0, 14.0)), CornerRadius::same(2), Color32::from_gray(160));
    let import_r = Rect::from_min_size(Pos2::new(zoom_r.left() - 96.0, bar.top() + 4.0), Vec2::new(84.0, 24.0));
    if btn(ui, import_r, "IMPORT", Look::On(RED)).on_hover_text("Add a music folder (Settings → Library) and rescan").clicked() {
        st.settings_open = true;
        core.rescan_library();
    }

    // ---- rows
    let mut rows: Vec<usize> = if st.browser_hist {
        core.history.iter().rev().filter_map(|p| core.library.iter().position(|e| &e.path == p)).collect()
    } else {
        core.filtered_library()
    };
    if st.browser_harmonic {
        if let Some(r) = core.reference_key() {
            rows.retain(|&i| core.library[i].key.map(|k| k.compat(r).is_good()).unwrap_or(false));
        }
    }
    if !st.browser_hist {
        let lib = &core.library;
        let plays = &core.trackdb.play_counts;
        let pc = |e: &Entry| plays.get(&crate::settings::TrackDb::key(&e.path)).copied().unwrap_or(0);
        let (col, asc) = (st.browser_sort, st.browser_asc);
        rows.sort_by(|&a, &b| {
            let (x, y) = (&lib[a], &lib[b]);
            let o = match col {
                Col::Artist => x.artist.to_lowercase().cmp(&y.artist.to_lowercase()).then(x.title.cmp(&y.title)),
                Col::Title => x.title.to_lowercase().cmp(&y.title.to_lowercase()),
                Col::Album => x.album.to_lowercase().cmp(&y.album.to_lowercase()),
                Col::Label => x.label.to_lowercase().cmp(&y.label.to_lowercase()),
                Col::Bpm => x.bpm.unwrap_or(0.0).total_cmp(&y.bpm.unwrap_or(0.0)),
                Col::Duration => x.duration_secs.unwrap_or(0.0).total_cmp(&y.duration_secs.unwrap_or(0.0)),
                Col::Key => x.key.map(|k| (k.camelot_number(), k.minor)).cmp(&y.key.map(|k| (k.camelot_number(), k.minor))),
                Col::Genre => x.genre.to_lowercase().cmp(&y.genre.to_lowercase()),
                Col::Plays => pc(x).cmp(&pc(y)),
                Col::Added => x.added.cmp(&y.added),
            };
            if asc {
                o
            } else {
                o.reverse()
            }
        });
    }

    let table = Rect::from_min_max(Pos2::new(main.left(), bar.bottom() + 6.0), main.right_bottom());
    let header = Rect::from_min_size(table.left_top(), Vec2::new(table.width(), 24.0));
    let widths: Vec<f32> = COLS.iter().map(|c| c.2 * (table.width() - 12.0)).collect();
    let mut x = header.left() + 6.0;
    for (i, (col, name, _)) in COLS.iter().enumerate() {
        let r = Rect::from_min_size(Pos2::new(x, header.top()), Vec2::new(widths[i], header.height()));
        let resp = ui.allocate_rect(r, Sense::click());
        let arrow = if st.browser_sort == *col && !st.browser_hist { if st.browser_asc { " ▲" } else { " ▼" } } else { "" };
        ui.painter().text(r.left_center(), Align2::LEFT_CENTER, format!("{name}{arrow}"), font(10.0), TEXT);
        if resp.clicked() {
            if st.browser_sort == *col {
                st.browser_asc = !st.browser_asc;
            } else {
                st.browser_sort = *col;
                st.browser_asc = true;
            }
        }
        x += widths[i];
    }
    ui.painter().line_segment([header.left_bottom(), header.right_bottom()], egui::Stroke::new(1.0, BORDER));

    let body = Rect::from_min_max(Pos2::new(table.left(), header.bottom() + 1.0), table.right_bottom());
    let row_h = (19.0 * st.browser_zoom).round();
    let text_size = 10.5 * st.browser_zoom.min(1.3);
    let reference = core.reference_key();
    let mut child = ui.new_child(UiBuilder::new().max_rect(body));
    let ctx = ui.ctx().clone();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(&mut child, row_h, rows.len(), |ui, range| {
        for ri in range {
            let i = rows[ri];
            let e = core.library[i].clone();
            let e = &e;
            let (rr, resp) = ui.allocate_exact_size(Vec2::new(body.width(), row_h), Sense::click_and_drag());
            let selected = core.browser_selected == Some(i);
            let p = ui.painter();
            let fill = if selected {
                Color32::from_gray(52)
            } else if resp.hovered() {
                Color32::from_gray(34)
            } else if ri % 2 == 0 {
                Color32::from_gray(20)
            } else {
                Color32::from_gray(16)
            };
            p.rect_filled(rr, CornerRadius::ZERO, fill);
            let plays = core.trackdb.play_counts.get(&crate::settings::TrackDb::key(&e.path)).copied().unwrap_or(0);
            let key_txt = core.key_text(e.key);
            let cells: [(String, Color32); 10] = [
                (e.artist.clone(), TEXT),
                (e.title.clone(), TEXT),
                (e.album.clone(), TEXT),
                (e.label.clone(), TEXT),
                (e.bpm.map(|b| format!("{b:.2}")).unwrap_or_default(), RED),
                (mmss(e.duration_secs), TEXT),
                (
                    key_txt,
                    match (e.key, reference) {
                        (Some(k), _) => key_color(k),
                        _ => TEXT,
                    },
                ),
                (e.genre.clone(), TEXT),
                (plays.to_string(), TEXT),
                (fmt_date(e.added), TEXT),
            ];
            let mut x = rr.left() + 6.0;
            for (ci, (txt, col)) in cells.iter().enumerate() {
                let cell = Rect::from_min_size(Pos2::new(x, rr.top()), Vec2::new(widths[ci] - 8.0, rr.height()));
                ui.painter_at(cell).text(cell.left_center(), Align2::LEFT_CENTER, txt, font(text_size), *col);
                x += widths[ci];
            }
            if resp.clicked() {
                core.browser_selected = Some(i);
            }
            if resp.double_clicked() {
                let d = (0..core.deck_count()).find(|&d| !core.snap.decks[d].playing && core.session.decks[d].mode == collective_engine::engine::DeckMode::Track).unwrap_or(0);
                let p = e.path.clone();
                core.load_track(d, &p);
            }
            if resp.drag_started() {
                egui::DragAndDrop::set_payload::<PathBuf>(&ctx, e.path.clone());
                core.browser_selected = Some(i);
            }
            let path = e.path.clone();
            resp.context_menu(|ui| {
                for d in 0..core.deck_count() {
                    if ui.button(format!("Load to deck {}", d + 1)).clicked() {
                        core.load_track(d, &path);
                        ui.close();
                    }
                }
                ui.separator();
                for d in 0..core.deck_count() {
                    if core.session.decks[d].mode == collective_engine::engine::DeckMode::Sampler {
                        let lane = core.session.decks[d].sampler.selected_lane;
                        if ui.button(format!("Sampler {} lane {}", d + 1, lane + 1)).clicked() {
                            core.load_sample(d, lane, &path);
                            ui.close();
                        }
                    }
                }
            });
        }
    });
    if let Some(p) = egui::DragAndDrop::payload::<PathBuf>(&ctx) {
        if let Some(pos) = ctx.pointer_hover_pos() {
            let name = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            ctx.debug_painter().text(pos + Vec2::new(12.0, 0.0), Align2::LEFT_CENTER, name, font(11.0), TEXT);
        }
    }
    if rows.is_empty() && !core.library_scanning {
        let msg = if st.browser_hist { "Tracks you load appear here." } else { "No tracks. Press IMPORT to add your music folder." };
        ui.painter().text(body.center(), Align2::CENTER_CENTER, msg, font(11.0), TEXT_DIM);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates() {
        assert_eq!(super::fmt_date(0), "");
        assert_eq!(super::fmt_date(1_793_443_522), "2026-10-31 10:45:22");
        assert_eq!(super::fmt_date(951_782_400), "2000-02-29 00:00:00");
    }
}
