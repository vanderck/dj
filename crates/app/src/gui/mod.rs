//! egui front end: header, 2×2 deck grid with the mixer in the middle,
//! browser, footer.

mod browser;
pub(crate) mod deck;
mod fx;
mod keyboard;
mod mixer;
mod settings_ui;
pub mod skin;
mod waves;
pub mod widgets;

use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, CornerRadius, Pos2, Rect, Ui, Vec2};

use dj_engine::fx::InsertPos;
use rosc::OscType;

use crate::core::Core;
use skin::*;

#[derive(Default)]
pub struct InsertBrowser {
    pub ch: usize,
    pub replace: Option<usize>,
    pub pos: InsertPos,
    pub query: String,
    pub category: Option<String>,
}

pub struct UiState {
    pub fx_open: [bool; 4],
    /// What each channel fader controls (set with the toggle rows for the selected fader, or
    /// by MIDI changing one of its functions).
    pub fader_mode: [mixer::FaderMode; 4],
    /// Group settings a fader popup's "turn off" switch saved, to restore them.
    pub group_restore: std::collections::HashMap<(usize, mixer::Group), Vec<f32>>,
    /// The track each channel's saved Stems levels belong to (a new track starts with all stems on,
    /// so the old track's levels aren't restored onto it).
    pub group_track: [u64; 4],
    /// Controller layout this frame (see `Settings::layout`): slim cue pads, four full decks.
    pub dense: bool,
    /// Unsnapped tempo fader position during a drag (snapping would swallow small moves).
    pub tempo_drag: Option<f32>,
    /// The channel whose fader function MIDI just changed, outlined for a moment: (mode, channel, since).
    pub fader_popup: Option<(mixer::FaderMode, usize, Instant)>,
    /// The `Core::midi_touched` time already handled.
    pub midi_seen: Option<Instant>,
    pub fx_edit: [[bool; 4]; 4],
    pub insert_browser: Option<InsertBrowser>,
    pub preset_name: [String; 4],
    pub settings_open: bool,
    pub mapping_open: bool,
    pub map_learn: Option<(usize, usize)>,
    pub new_folder: String,
    pub new_vst_path: String,
    pub browser_hist: bool,
    /// The name typed in the Crates menu (a new crate, or a new name for the one shown).
    pub crate_name: String,
    pub browser_harmonic: bool,
    /// The library entry Fits last focused (its best match), to refocus when that changes.
    pub fits_focus: Option<usize>,
    pub browser_sort: browser::Col,
    pub browser_asc: bool,
    /// Which deck is shown large on the left (0 or 2) and right (1 or 3).
    pub focus: [usize; 2],
    pub lane_zoom: f32,
    /// What Now and Best would do, shown in the Mix popup (worked out again every second).
    pub mix_preview: Option<deck::MixPreview>,
    /// The large browser on top of the app, and the touch keyboard in it.
    pub browser_big: bool,
    pub keyboard_open: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            fx_open: [false; 4],
            fader_mode: [mixer::FaderMode::Level; 4],
            group_restore: Default::default(),
            group_track: [0; 4],
            dense: false,
            tempo_drag: None,
            fader_popup: None,
            midi_seen: None,
            fx_edit: [[false; 4]; 4],
            insert_browser: None,
            preset_name: Default::default(),
            settings_open: false,
            mapping_open: false,
            map_learn: None,
            new_folder: String::new(),
            new_vst_path: String::new(),
            browser_hist: false,
            crate_name: String::new(),
            browser_harmonic: false,
            fits_focus: None,
            browser_sort: browser::Col::Artist,
            browser_asc: true,
            focus: [0, 1],
            lane_zoom: 1.0,
            mix_preview: None,
            browser_big: false,
            keyboard_open: false,
        }
    }
}

pub struct App {
    core: Core,
    _host: crate::audio::AudioHost,
    ui: UiState,
    last_save: Instant,
}

pub fn run() {
    // Prefer wgpu; fall back to OpenGL (glow) where no wgpu surface can be
    // created (old GPUs/drivers, VMs, remote desktops). Override with
    // DJ_RENDERER=glow|wgpu.
    let order = match std::env::var("DJ_RENDERER").as_deref() {
        Ok("glow") => vec![eframe::Renderer::Glow],
        Ok("wgpu") => vec![eframe::Renderer::Wgpu],
        _ => vec![eframe::Renderer::Wgpu, eframe::Renderer::Glow],
    };
    let mut booted: Option<(crate::audio::AudioHost, Core)> = None;
    for renderer in order {
        let opts = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1600.0, 1000.0])
                .with_min_inner_size([1100.0, 700.0])
                .with_title("dj")
                .with_icon(std::sync::Arc::new(
                    eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon-256.png")).unwrap_or_default(),
                )),
            renderer,
            ..Default::default()
        };
        let slot = &mut booted;
        let res = eframe::run_native(
            "dj",
            opts,
            Box::new(move |cc| {
                let (host, core) = slot.take().unwrap_or_else(crate::boot);
                install_fonts(&cc.egui_ctx);
                set_visuals(&cc.egui_ctx);
                Ok(Box::new(App { core, _host: host, ui: UiState::default(), last_save: Instant::now() }))
            }),
        );
        match res {
            Ok(()) => return,
            Err(e) => eprintln!("GUI ({renderer:?}) failed: {e}"),
        }
    }
    eprintln!("No usable graphics backend. Use --headless for OSC/MIDI-only operation.");
}

/// Register a control for MIDI learn: clicking it while learning selects it.
pub fn learnable(core: &mut Core, resp: &egui::Response, addr: &str) {
    if core.learn {
        let selected = core.last_touched.as_deref() == Some(addr);
        let c = if selected { RED } else { Color32::from_rgb(0x3b, 0x7b, 0xe6) };
        resp.ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("learn"))).rect_stroke(
            resp.rect.expand(1.0),
            egui::CornerRadius::same(3),
            egui::Stroke::new(if selected { 2.0 } else { 1.0 }, c),
            egui::StrokeKind::Outside,
        );
        if resp.clicked() || resp.drag_started() {
            core.touch(addr.to_string());
        }
    }
}

/// Route a GUI action through the same path as MIDI/OSC.
pub fn act(core: &mut Core, addr: &str, args: &[OscType]) {
    if !core.learn {
        core.route(addr, args);
    }
}

pub fn act_f(core: &mut Core, addr: &str, v: f32) {
    act(core, addr, &[OscType::Float(v)]);
}

pub fn act_i(core: &mut Core, addr: &str, v: i32) {
    act(core, addr, &[OscType::Int(v)]);
}

fn key_name(k: egui::Key) -> String {
    format!("{k:?}")
}

impl App {
    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let events = ctx.input(|i| i.events.clone());
        for e in events {
            if let egui::Event::Key { key, pressed, repeat: false, modifiers, .. } = e {
                if modifiers.command {
                    continue;
                }
                self.core.handle_key(&key_name(key), pressed);
            }
        }
    }

    /// Fit the interface to the window: the layout is designed for 1600×1000 points.
    fn autoscale(&self, ctx: &egui::Context) {
        let (size, native) = ctx.input(|i| (i.viewport_rect().size() * i.pixels_per_point(), i.viewport().native_pixels_per_point.unwrap_or(1.0)));
        if size.x <= 0.0 || size.y <= 0.0 {
            return;
        }
        let fit = (size.x / native / 1600.0).min(size.y / native / 1000.0).clamp(0.6, 2.0);
        let want = (native * fit).clamp(0.5, 3.0);
        if (ctx.pixels_per_point() - want).abs() > 0.01 {
            ctx.set_pixels_per_point(want);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let frame = Instant::now();
        self.core.tick();
        let tick_ms = frame.elapsed().as_millis();
        let ctx = ui.ctx().clone();
        self.autoscale(&ctx);
        self.handle_keys(&ctx);
        // Keep focus valid for the deck count.
        if self.core.deck_count() < 4 {
            self.ui.focus = [0, 1];
        }

        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(BG)).show(ui, |ui| {
            let area = ui.max_rect().shrink(12.0);
            let gap = 12.0;
            let h = area.height();
            let lanes_h = (h * 0.25).clamp(120.0, 360.0);
            let browser_h = (h * 0.26).clamp(150.0, 420.0);
            let lanes = Rect::from_min_size(area.min, Vec2::new(area.width(), lanes_h));
            self.ui.dense = match self.core.settings.layout {
                crate::settings::LayoutMode::Controller => true,
                crate::settings::LayoutMode::Touch => false,
                crate::settings::LayoutMode::Auto => self.core.midi.has_controller(),
            };
            waves::show(ui, &mut self.core, &mut self.ui, lanes);

            let mid = Rect::from_min_max(Pos2::new(area.left(), lanes.bottom() + gap), Pos2::new(area.right(), area.bottom() - browser_h - gap));
            // With an external mixer the centre column only holds the clock and routing.
            let mixer_w = if self.core.external_routing() { 240.0 } else { (area.width() * 0.31).clamp(360.0, 640.0) };
            let deck_w = (area.width() - mixer_w - 2.0 * gap) / 2.0;
            let four = self.core.deck_count() >= 4;
            // One row: badge, title, key, Swap, Cue, Play, Sync.
            let compact_h = 62.0;
            for side in 0..2 {
                let x = if side == 0 { mid.left() } else { mid.right() - deck_w };
                if four && self.ui.dense {
                    // Controller layout: both decks of this side in full, stacked (1 over 3, 2 over 4).
                    let dh = (mid.height() - gap) / 2.0;
                    for (row, d) in [side, side + 2].into_iter().enumerate() {
                        let dr = Rect::from_min_size(Pos2::new(x, mid.top() + row as f32 * (dh + gap)), Vec2::new(deck_w, dh));
                        deck::big(ui, &mut self.core, &mut self.ui, d, dr);
                    }
                    continue;
                }
                let big_d = self.ui.focus[side];
                let big_h = if four { mid.height() - compact_h - gap } else { mid.height() };
                deck::big(ui, &mut self.core, &mut self.ui, big_d, Rect::from_min_size(Pos2::new(x, mid.top()), Vec2::new(deck_w, big_h)));
                if four {
                    let other = if big_d == side { side + 2 } else { side };
                    deck::compact(
                        ui,
                        &mut self.core,
                        &mut self.ui,
                        other,
                        Rect::from_min_size(Pos2::new(x, mid.bottom() - compact_h), Vec2::new(deck_w, compact_h)),
                    );
                }
            }
            let mixer = Rect::from_min_size(Pos2::new(mid.left() + deck_w + gap, mid.top()), Vec2::new(mixer_w, mid.height()));
            mixer::show(ui, &mut self.core, &mut self.ui, mixer);

            let br = Rect::from_min_max(Pos2::new(area.left(), area.bottom() - browser_h), area.right_bottom());
            if self.ui.browser_big {
                // (Shown large, on top: see `browser_overlay`.)
                skin::card(ui, br);
            } else {
                browser::show(ui, &mut self.core, &mut self.ui, br, false);
            }

            // MIDI learn hint.
            if self.core.learn {
                let txt = match &self.core.last_touched {
                    Some(t) => format!("MIDI learn · {t} · move a controller"),
                    None => "MIDI learn · click a control, then move a controller".into(),
                };
                let p = ui.painter();
                let w = 420.0;
                let r = Rect::from_center_size(Pos2::new(area.center().x, lanes.bottom() + gap / 2.0), Vec2::new(w, 24.0));
                p.rect_filled(r, CornerRadius::same(12), Color32::from_rgb(0x2b, 0x4d, 0x8c));
                p.text(r.center(), Align2::CENTER_CENTER, txt, bold(12.0), Color32::WHITE);
            }
        });

        if self.ui.browser_big {
            browser_overlay(&ctx, &mut self.core, &mut self.ui);
        }
        fx::windows(&ctx, &mut self.core, &mut self.ui);
        settings_ui::windows(&ctx, &mut self.core, &mut self.ui);
        load_alert(&ctx, &mut self.core);

        if self.last_save.elapsed() > Duration::from_secs(60) {
            self.last_save = Instant::now();
            self.core.save_all();
        }
        ctx.request_repaint_after(Duration::from_millis(16));
        // A frame that held up the screen (the beach ball): say how long, and how much of it was
        // dj's own work (the rest is drawing), so it can be found.
        let ms = frame.elapsed().as_millis();
        if ms > 250 {
            eprintln!("slow frame: {ms} ms (dj's work {tick_ms} ms)");
        }
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.core.save_all();
    }

    /// Quitting: save, finish a recording, and leave at once (see `quit_now`).
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.core.prepare_to_quit();
        crate::quit_now();
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.core.save_all();
        crate::settings::flush_writes();
    }
}

/// Loading onto a deck that's playing: asked first (Return loads, Escape or a click outside keeps
/// the deck as it is; on a controller, LOAD again for the same track loads it).
fn load_alert(ctx: &egui::Context, core: &mut Core) {
    let Some((d, path)) = core.pending_load.clone() else { return };
    let playing = core.decks[d].info.as_ref().map(|i| i.title()).unwrap_or_default();
    let new = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut answer = None;
    let modal = egui::Modal::new(egui::Id::unique("load-alert")).show(ctx, |ui| {
        ui.set_width(380.0);
        ui.label(egui::RichText::new(format!("Deck {} is playing", d + 1)).strong().size(16.0).color(skin::DECK[d]));
        ui.add_space(6.0);
        ui.label(format!("Load \"{new}\" onto it? \"{playing}\" stops."));
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button(egui::RichText::new("Load").strong()).clicked() {
                answer = Some(true);
            }
            if ui.button("Cancel").clicked() {
                answer = Some(false);
            }
        });
    });
    if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
        answer = Some(true);
    }
    if modal.should_close() {
        answer = answer.or(Some(false));
    }
    match answer {
        Some(true) => core.confirm_load(),
        Some(false) => core.cancel_load(),
        None => {}
    }
}

/// The browser large, on top of the app: as high as you drag its top edge (a share of the
/// window), its text as large as you set it (A− / A+), the touch keyboard under it when the
/// search field was tapped (Settings). Closes with its button or Esc.
fn browser_overlay(ctx: &egui::Context, core: &mut Core, st: &mut UiState) {
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        st.browser_big = false;
        st.keyboard_open = false;
        return;
    }
    let screen = ctx.content_rect();
    let s = core.settings.browser_big_scale.clamp(1.0, 2.5);
    let frac = core.settings.browser_big_height.clamp(0.3, 1.0);
    let h = screen.height() * frac;
    let id = egui::Id::unique("browser_big");
    let layer = egui::LayerId::new(egui::Order::Foreground, id);
    // Drawn at its size divided by the text size, then scaled up (input too).
    ctx.set_transform_layer(layer, egui::emath::TSTransform::new(Vec2::new(screen.left(), screen.bottom() - h), s));
    let size = Vec2::new(screen.width(), h) / s;
    egui::Area::new(id).order(egui::Order::Foreground).fixed_pos(Pos2::ZERO).constrain(false).show(ctx, |ui| {
        let full = Rect::from_min_size(Pos2::ZERO, size);
        ui.set_clip_rect(full);
        ui.allocate_rect(full, egui::Sense::hover());
        ui.painter().rect_filled(full, CornerRadius::ZERO, BG);
        ui.painter().line_segment([full.left_top(), full.right_top()], egui::Stroke::new(2.0, BORDER));
        // Its top: drag to resize; text size; close.
        let head = Rect::from_min_size(full.min, Vec2::new(size.x, 28.0));
        let bw = 44.0;
        let close = Rect::from_min_size(Pos2::new(head.right() - 8.0 - 70.0, head.top() + 3.0), Vec2::new(70.0, 22.0));
        let plus = Rect::from_min_size(Pos2::new(close.left() - 8.0 - bw, close.top()), Vec2::new(bw, 22.0));
        let minus = Rect::from_min_size(Pos2::new(plus.left() - 6.0 - bw, close.top()), Vec2::new(bw, 22.0));
        let grip = Rect::from_min_max(head.min, Pos2::new(minus.left() - 8.0, head.bottom()));
        let drag = ui.allocate_rect(grip, egui::Sense::drag()).on_hover_cursor(egui::CursorIcon::ResizeVertical);
        if drag.dragged() {
            let dy = drag.drag_delta().y * s;
            core.settings.browser_big_height = ((h - dy) / screen.height()).clamp(0.3, 1.0);
        }
        let p = ui.painter();
        p.rect_filled(Rect::from_center_size(Pos2::new(grip.center().x, grip.center().y), Vec2::new(60.0, 4.0)), CornerRadius::same(2), TEXT_DIM);
        p.text(Pos2::new(grip.left() + 12.0, grip.center().y), Align2::LEFT_CENTER, "Drag to resize", sans(11.0), TEXT_DIM);
        if skin::button(ui, minus, "A−", skin::Style::Dark, bold(12.0)).clicked() {
            core.settings.browser_big_scale = (s - 0.1).max(1.0);
        }
        if skin::button(ui, plus, "A+", skin::Style::Dark, bold(12.0)).clicked() {
            core.settings.browser_big_scale = (s + 0.1).min(2.5);
        }
        if skin::button(ui, close, "Close", skin::Style::Light, bold(12.0)).clicked() {
            st.browser_big = false;
            st.keyboard_open = false;
        }
        // The touch keyboard (Settings), when the search field was tapped.
        let kb_h = if st.keyboard_open && core.settings.touch_keyboard { (size.y * 0.42).min(260.0) } else { 0.0 };
        let body = Rect::from_min_max(Pos2::new(full.left(), head.bottom()), Pos2::new(full.right(), full.bottom() - kb_h));
        browser::show(ui, core, st, body.shrink(6.0), true);
        if kb_h > 0.0 {
            let kr = Rect::from_min_max(Pos2::new(full.left(), full.bottom() - kb_h), full.max).shrink(8.0);
            let layout = keyboard::layout(core.settings.keyboard_layout);
            if keyboard::show(ui, kr, &mut core.browser_query, layout) == keyboard::Action::Hide {
                st.keyboard_open = false;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use eframe::egui;

    fn frame(ctx: &egui::Context, c: &mut crate::core::Core, key: Option<egui::Key>) {
        let mut input = egui::RawInput::default();
        if let Some(key) = key {
            input.events.push(egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() });
        }
        let mut out = ctx.run_ui(input, |ui| super::load_alert(ui.ctx(), c));
        out.textures_delta.clear(); // (no GPU here to upload them to)
    }

    /// A frame of the large browser in a 1280×800 window, with these events.
    fn overlay_frame(ctx: &egui::Context, c: &mut crate::core::Core, st: &mut super::UiState, events: Vec<egui::Event>) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0))),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| super::browser_overlay(ui.ctx(), c, st));
        out.textures_delta.clear();
    }

    /// A tap at `pos` (window coordinates).
    fn tap(ctx: &egui::Context, c: &mut crate::core::Core, st: &mut super::UiState, pos: egui::Pos2) {
        let press = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        overlay_frame(ctx, c, st, vec![egui::Event::PointerMoved(pos)]);
        overlay_frame(ctx, c, st, vec![press(true)]);
        overlay_frame(ctx, c, st, vec![press(false)]);
        overlay_frame(ctx, c, st, vec![]);
    }

    /// Where a point of the large browser (its own coordinates, before the text size scales it)
    /// is in the window: 1280×800, 70 % high, text ×1.5 (the defaults).
    fn on_screen(p: egui::Pos2) -> egui::Pos2 {
        egui::pos2(p.x * 1.5, 800.0 - 560.0 + p.y * 1.5)
    }

    #[test]
    fn the_large_browser_closes_with_its_button() {
        let (_e, mut c, _config) = crate::core::tests::core();
        let ctx = egui::Context::default();
        super::skin::install_fonts(&ctx);
        let mut st = super::UiState { browser_big: true, ..Default::default() };
        // (A window's first frame only sizes it.)
        overlay_frame(&ctx, &mut c, &mut st, vec![]);
        overlay_frame(&ctx, &mut c, &mut st, vec![]);
        // Close: top right (its size: 1280 / 1.5 wide).
        let w = 1280.0 / 1.5;
        tap(&ctx, &mut c, &mut st, on_screen(egui::pos2(w - 8.0 - 35.0, 14.0)));
        assert!(!st.browser_big, "closed");
        // Esc closes it too.
        st.browser_big = true;
        overlay_frame(&ctx, &mut c, &mut st, vec![]);
        overlay_frame(&ctx, &mut c, &mut st, vec![egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() }]);
        assert!(!st.browser_big);
    }

    #[test]
    fn the_touch_keyboard_types_into_the_search() {
        let (_e, mut c, _config) = crate::core::tests::core();
        c.settings.touch_keyboard = true;
        c.settings.keyboard_layout = crate::settings::KeyboardLayout::Azerty;
        let ctx = egui::Context::default();
        super::skin::install_fonts(&ctx);
        let mut st = super::UiState { browser_big: true, keyboard_open: true, ..Default::default() };
        overlay_frame(&ctx, &mut c, &mut st, vec![]);
        overlay_frame(&ctx, &mut c, &mut st, vec![]);
        let size = egui::vec2(1280.0, 560.0) / 1.5;
        let kb_h = (size.y * 0.42).min(260.0);
        let kr = egui::Rect::from_min_max(egui::pos2(0.0, size.y - kb_h), size.to_pos2()).shrink(8.0);
        let keys = super::keyboard::keys(kr, crate::settings::KeyboardLayout::Azerty);
        let at = |k: &str| on_screen(keys.iter().find(|(l, _)| l == k).expect("a key").1.center());
        for k in ["d", "j", "space", "a", "z", "Back"] {
            tap(&ctx, &mut c, &mut st, at(k));
        }
        assert_eq!(c.browser_query, "dj a", "typed (AZERTY: a and z where they are)");
        tap(&ctx, &mut c, &mut st, at("Hide"));
        assert!(!st.keyboard_open && st.browser_big, "hidden, the browser stays");
    }

    #[test]
    fn the_load_alert_loads_on_return_and_keeps_the_deck_on_escape() {
        let (_e, mut c, _config) = crate::core::tests::core();
        let ctx = egui::Context::default();
        super::skin::install_fonts(&ctx);
        let path = std::path::PathBuf::from("/music/next.wav");
        c.pending_load = Some((0, path.clone()));
        frame(&ctx, &mut c, None);
        assert!(c.pending_load.is_some(), "shown, waiting");
        frame(&ctx, &mut c, Some(egui::Key::Escape));
        assert!(c.pending_load.is_none() && c.decks[0].loading.is_none(), "Escape: nothing loads");
        c.pending_load = Some((0, path.clone()));
        frame(&ctx, &mut c, Some(egui::Key::Enter));
        assert!(c.pending_load.is_none());
        assert_eq!(c.decks[0].loading.as_deref(), Some(path.as_path()), "Return: it loads");
    }
}
