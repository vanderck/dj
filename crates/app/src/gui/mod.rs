//! egui front end.

mod browser;
mod deck;
mod fx;
mod mixer;
mod settings_ui;
pub mod widgets;

use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, Ui, Vec2};

use collective_engine::engine::ClockOwner;
use collective_engine::fx::InsertPos;
use rosc::OscType;

use crate::core::Core;
use crate::model::QUANTIZE_VALUES;
use widgets::*;

#[derive(Default)]
pub struct InsertBrowser {
    pub ch: usize,
    pub replace: Option<usize>,
    pub pos: InsertPos,
    pub query: String,
    pub category: Option<String>,
}

#[derive(Default)]
pub struct UiState {
    pub fx_open: [bool; 4],
    pub fx_edit: [[bool; 4]; 4],
    pub insert_browser: Option<InsertBrowser>,
    pub preset_name: [String; 4],
    pub settings_open: bool,
    pub mapping_open: bool,
    pub bpm_edit: Option<String>,
    pub map_learn: Option<(usize, usize)>,
    pub new_folder: String,
    pub new_vst_path: String,
    pub browser_height: f32,
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
                .with_inner_size([1440.0, 900.0])
                .with_min_inner_size([1100.0, 700.0])
                .with_title("Collective"),
            renderer,
            ..Default::default()
        };
        let slot = &mut booted;
        let res = eframe::run_native(
            "Collective",
            opts,
            Box::new(move |cc| {
                let (host, core) = slot.take().unwrap_or_else(crate::boot);
                setup_style(&cc.egui_ctx, core.settings.light_theme, core.settings.ui_scale);
                Ok(Box::new(App { core, _host: host, ui: UiState { browser_height: 230.0, ..Default::default() }, last_save: Instant::now() }))
            }),
        );
        match res {
            Ok(()) => return,
            Err(e) => eprintln!("GUI ({renderer:?}) failed: {e}"),
        }
    }
    eprintln!("No usable graphics backend. Use --headless for OSC/MIDI-only operation.");
}

pub fn setup_style(ctx: &egui::Context, light: bool, scale: f32) {
    let mut v = if light { egui::Visuals::light() } else { egui::Visuals::dark() };
    if !light {
        v.panel_fill = Color32::from_gray(24);
        v.window_fill = Color32::from_gray(28);
        v.extreme_bg_color = Color32::from_gray(14);
        v.widgets.inactive.weak_bg_fill = Color32::from_gray(44);
        v.widgets.inactive.bg_fill = Color32::from_gray(52);
    }
    v.selection.bg_fill = dim(ACCENT, 0.6);
    ctx.set_visuals(v);
    ctx.set_pixels_per_point(scale.clamp(0.5, 2.0));
}

/// Register a control for MIDI learn: clicking it while learning selects it.
pub fn learnable(core: &mut Core, resp: &egui::Response, addr: &str) {
    if core.learn {
        let selected = core.last_touched.as_deref() == Some(addr);
        let c = if selected { ACCENT } else { Color32::from_rgb(0x3b, 0x7b, 0xe6) };
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
    fn header(&mut self, ui: &mut Ui) {
        let core = &mut self.core;
        let s = core.snap;
        ui.horizontal(|ui| {
            ui.label(RichText::new("COLLECTIVE").strong().color(ACCENT).size(16.0));
            ui.separator();

            // Master / limiter / cue.
            let mut mv = core.session.master.volume;
            let r = knob(ui, &mut mv, 0.0, 1.0, 0.85, "MASTER", ACCENT);
            learnable(core, &r, "Header/MasterVolume");
            if r.changed() {
                act_f(core, "Header/MasterVolume", mv);
            }
            let (rect, _) = ui.allocate_exact_size(Vec2::new(14.0, 40.0), egui::Sense::hover());
            draw_meter(ui, rect, s.master.peak);
            let lim = core.session.master.limiter;
            let r = lit(ui, lim, "LIMIT", WARN).on_hover_text(format!("Master limiter (−0.3 dBFS). Reduction: {:.1} dB", s.limiter_db));
            learnable(core, &r, "Header/Limiter");
            if r.clicked() {
                act_i(core, "Header/Limiter", !lim as i32);
            }
            if core.settings.audio.cue_enabled {
                let mut cv = core.session.master.cue_volume;
                let r = knob(ui, &mut cv, 0.0, 1.0, 0.75, "CUE", ACCENT);
                learnable(core, &r, "Header/CueVolume");
                if r.changed() {
                    act_f(core, "Header/CueVolume", cv);
                }
                let mut cm = core.session.master.cue_mix;
                let r = knob(ui, &mut cm, 0.0, 1.0, 0.0, "MIX", ACCENT);
                learnable(core, &r, "Header/CueMix");
                if r.changed() {
                    act_f(core, "Header/CueMix", cm);
                }
            }
            ui.separator();

            // Metronome.
            let met = core.session.master.metronome;
            let r = lit(ui, met, "♩", ACCENT).on_hover_text("Metronome (cue bus)");
            learnable(core, &r, "Header/Metronome");
            if r.clicked() {
                act_i(core, "Header/Metronome", !met as i32);
            }

            // Clock ownership.
            let header_owns = s.clock.owner == ClockOwner::Header;
            let r = lit(ui, header_owns, "M", ACCENT).on_hover_text("Header owns the clock. Click to hand it to a playing synced deck / take it back.");
            learnable(core, &r, "Header/Master");
            if r.clicked() {
                act_i(core, "Header/Master", 1);
            }
            let wc = core.session.wallclock;
            let r = lit(ui, wc, "🔒", ACCENT).on_hover_text("Wallclock: only the header can own the clock");
            learnable(core, &r, "Header/Wallclock");
            if r.clicked() {
                act_i(core, "Header/Wallclock", !wc as i32);
            }

            // Tempo readout: drag to change, double-click to type.
            let owner = match s.clock.owner {
                ClockOwner::Header => "HEADER".to_string(),
                ClockOwner::Deck(d) => format!("DECK {}", d + 1),
            };
            if let Some(text) = &mut self.ui.bpm_edit {
                let r = ui.add(egui::TextEdit::singleline(text).desired_width(70.0));
                if r.lost_focus() {
                    if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        if let Ok(v) = text.trim().parse::<f32>() {
                            act_f(core, "Header/BPM", v);
                        }
                    }
                    self.ui.bpm_edit = None;
                } else {
                    r.request_focus();
                }
            } else {
                let r = ui.add(
                    egui::Label::new(RichText::new(format!("{:7.2}", s.clock.bpm)).monospace().size(22.0).strong())
                        .sense(egui::Sense::click_and_drag()),
                );
                let r = r.on_hover_text(format!("Master tempo ({owner}). Drag to change (Shift: fine), double-click to type."));
                learnable(core, &r, "Header/BPM");
                if r.dragged() {
                    let fine = ui.input(|i| i.modifiers.shift);
                    let d = r.drag_delta().x as f64 * if fine { 0.01 } else { 0.1 };
                    if d != 0.0 {
                        act_f(core, "Header/BPM", (s.clock.bpm + d) as f32);
                    }
                }
                if r.double_clicked() {
                    self.ui.bpm_edit = Some(format!("{:.2}", s.clock.bpm));
                }
            }
            ui.label(RichText::new(owner).small().weak());

            // Quantize.
            let q_on = core.session.quantize_on;
            let r = lit(ui, q_on, "Q", ACCENT);
            learnable(core, &r, "Header/QuantizeOn");
            if r.clicked() {
                act_i(core, "Header/QuantizeOn", !q_on as i32);
            }
            let cur = QUANTIZE_VALUES[core.session.quantize].1;
            egui::ComboBox::from_id_salt("quant").width(56.0).selected_text(format!("{cur} bar")).show_ui(ui, |ui| {
                for (i, (_, l)) in QUANTIZE_VALUES.iter().enumerate() {
                    if ui.selectable_label(core.session.quantize == i, format!("{l} bar")).clicked() {
                        act_i(core, "Header/Quantize", i as i32 + 1);
                    }
                }
            });

            // Beat indicator.
            let beat = s.clock.beat.floor() as i64;
            let in_bar = beat.rem_euclid(4);
            for i in 0..4 {
                let on = i == in_bar;
                let (r, _) = ui.allocate_exact_size(Vec2::new(10.0, 10.0), egui::Sense::hover());
                ui.painter().circle_filled(r.center(), 4.5, if on { if i == 0 { WARN } else { ACCENT } } else { Color32::from_gray(60) });
            }
            ui.separator();

            // Learn / record / settings.
            let r = lit(ui, core.learn, "LEARN", Color32::from_rgb(0x3b, 0x7b, 0xe6))
                .on_hover_text("MIDI learn: click a control, then move a controller");
            if r.clicked() {
                core.learn = !core.learn;
                core.last_touched = None;
            }
            if core.learn {
                ui.label(RichText::new(core.last_touched.clone().unwrap_or_else(|| "click a control…".into())).color(ACCENT).small());
            }
            let rec = core.recorder.is_some();
            let label = match &core.recorder {
                Some(r) => format!("● REC {}", fmt_time(r.started.elapsed().as_secs_f64())),
                None => "● REC".into(),
            };
            let r = lit(ui, rec, &label, DANGER);
            learnable(core, &r, "Header/Record");
            if r.clicked() {
                core.toggle_recording();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⚙").on_hover_text("Settings").clicked() {
                    self.ui.settings_open = !self.ui.settings_open;
                }
                if ui.button("MIDI").on_hover_text("Mappings").clicked() {
                    self.ui.mapping_open = !self.ui.mapping_open;
                }
                let load = s.cpu_load;
                let col = if load > 0.8 { DANGER } else if load > 0.5 { WARN } else { ui.visuals().weak_text_color() };
                ui.label(RichText::new(format!("DSP {:>3.0}%", load * 100.0)).small().color(col));
                if s.compensation_latency > 0 {
                    ui.label(
                        RichText::new(format!("PDC {:.1} ms", s.compensation_latency as f32 / core.sample_rate * 1000.0)).small().weak(),
                    )
                    .on_hover_text("Plugin delay compensation");
                }
                let midi_on = core.midi.activity.elapsed() < Duration::from_millis(150);
                ui.label(RichText::new("●").color(if midi_on { ACCENT } else { Color32::from_gray(60) })).on_hover_text("MIDI activity");
            });
        });
    }

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

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if let Some((t, m)) = self.core.status.back() {
                let age = t.elapsed().as_secs_f32();
                let a = if age < 6.0 { 1.0 } else { 0.5 };
                ui.label(RichText::new(m).small().color(dim(ui.visuals().text_color(), a)));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("{} tracks", self.core.library.len())).small().weak());
                if self.core.osc_in.is_some() {
                    ui.label(RichText::new(format!("OSC :{} {}", self.core.settings.osc_port, self.core.settings.osc_root)).small().weak());
                }
                ui.label(RichText::new(format!("{} Hz", self.core.sample_rate)).small().weak());
            });
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.core.tick();
        let ctx = ui.ctx().clone();
        self.handle_keys(&ctx);

        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(2.0);
            self.header(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::bottom("browser").resizable(true).default_size(self.ui.browser_height).show(ui, |ui| {
            browser::show(ui, &mut self.core, &mut self.ui);
        });
        egui::Panel::right("mixer").resizable(false).show(ui, |ui| {
            mixer::show(ui, &mut self.core, &mut self.ui);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let n = self.core.deck_count();
                for d in 0..n {
                    deck::show(ui, &mut self.core, &mut self.ui, d);
                    ui.add_space(4.0);
                }
            });
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
