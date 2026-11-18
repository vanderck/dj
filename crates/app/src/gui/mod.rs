//! egui front end: header, 2×2 deck grid with the mixer in the middle,
//! browser, footer.

mod browser;
mod deck;
mod fx;
mod mixer;
mod settings_ui;
pub mod skin;
mod waves;
pub mod widgets;

use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, CornerRadius, Pos2, Rect, Ui, Vec2};

use collective_engine::fx::InsertPos;
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
    /// What the channel faders control (the toggle row above the strips).
    pub fader_mode: mixer::FaderMode,
    /// Value each channel/mode had before its ON/OFF button switched it off.
    pub fader_restore: std::collections::HashMap<(usize, mixer::FaderMode), f32>,
    /// Controller layout this frame (see `Settings::layout`): slim cue pads, four full decks.
    pub dense: bool,
    pub fx_edit: [[bool; 4]; 4],
    pub insert_browser: Option<InsertBrowser>,
    pub preset_name: [String; 4],
    pub settings_open: bool,
    pub mapping_open: bool,
    pub bpm_edit: Option<String>,
    pub map_learn: Option<(usize, usize)>,
    pub new_folder: String,
    pub new_vst_path: String,
    pub browser_hist: bool,
    pub browser_harmonic: bool,
    pub browser_sort: browser::Col,
    pub browser_asc: bool,
    /// Which deck is shown large on the left (0 or 2) and right (1 or 3).
    pub focus: [usize; 2],
    pub loop_size: [f64; 4],
    pub lane_zoom: f32,
    pub taps: Vec<Instant>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            fx_open: [false; 4],
            fader_mode: mixer::FaderMode::Level,
            fader_restore: Default::default(),
            dense: false,
            fx_edit: [[false; 4]; 4],
            insert_browser: None,
            preset_name: Default::default(),
            settings_open: false,
            mapping_open: false,
            bpm_edit: None,
            map_learn: None,
            new_folder: String::new(),
            new_vst_path: String::new(),
            browser_hist: false,
            browser_harmonic: false,
            browser_sort: browser::Col::Artist,
            browser_asc: true,
            focus: [0, 1],
            loop_size: [4.0; 4],
            lane_zoom: 1.0,
            taps: Vec::new(),
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
    // COLLECTIVE_RENDERER=glow|wgpu.
    let order = match std::env::var("COLLECTIVE_RENDERER").as_deref() {
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
                .with_title("Collective")
                .with_icon(std::sync::Arc::new(
                    eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon-256.png")).unwrap_or_default(),
                )),
            renderer,
            ..Default::default()
        };
        let slot = &mut booted;
        let res = eframe::run_native(
            "Collective",
            opts,
            Box::new(move |cc| {
                let (host, core) = slot.take().unwrap_or_else(crate::boot);
                install_fonts(&cc.egui_ctx);
                setup_style(&cc.egui_ctx, core.settings.light_theme, core.settings.ui_scale);
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

pub fn setup_style(ctx: &egui::Context, _light: bool, scale: f32) {
    set_visuals(ctx, scale);
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

    /// Scale the whole interface with the window from a 1600×1000 baseline.
    fn autoscale(&self, ctx: &egui::Context) {
        let (size, native) = ctx.input(|i| (i.viewport_rect().size() * i.pixels_per_point(), i.viewport().native_pixels_per_point.unwrap_or(1.0)));
        if size.x <= 0.0 || size.y <= 0.0 {
            return;
        }
        let fit = (size.x / native / 1600.0).min(size.y / native / 1000.0).clamp(0.6, 2.0);
        let want = (native * fit * self.core.settings.ui_scale).clamp(0.5, 3.0);
        if (ctx.pixels_per_point() - want).abs() > 0.01 {
            ctx.set_pixels_per_point(want);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.core.tick();
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
            waves::show(ui, &mut self.core, &mut self.ui, lanes);

            let mid = Rect::from_min_max(Pos2::new(area.left(), lanes.bottom() + gap), Pos2::new(area.right(), area.bottom() - browser_h - gap));
            // With an external mixer the centre column only holds the clock and routing.
            let mixer_w = if self.core.external_routing() { 240.0 } else { (area.width() * 0.31).clamp(360.0, 640.0) };
            let deck_w = (area.width() - mixer_w - 2.0 * gap) / 2.0;
            let four = self.core.deck_count() >= 4;
            self.ui.dense = match self.core.settings.layout {
                crate::settings::LayoutMode::Controller => true,
                crate::settings::LayoutMode::Touch => false,
                crate::settings::LayoutMode::Auto => self.core.midi.has_controller(),
            };
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
            browser::show(ui, &mut self.core, &mut self.ui, br);

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

        fx::windows(&ctx, &mut self.core, &mut self.ui);
        settings_ui::windows(&ctx, &mut self.core, &mut self.ui);

        if self.last_save.elapsed() > Duration::from_secs(60) {
            self.last_save = Instant::now();
            self.core.save_all();
        }
        ctx.request_repaint_after(Duration::from_millis(16));
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.core.save_all();
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.core.save_all();
    }
}
