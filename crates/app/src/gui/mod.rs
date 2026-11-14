//! egui front end: header, 2×2 deck grid with the mixer in the middle,
//! browser, footer.

mod browser;
mod deck;
mod fx;
mod mixer;
mod settings_ui;
pub mod skin;
pub mod widgets;

use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, CornerRadius, Pos2, Rect, Sense, Ui, UiBuilder, Vec2};

use collective_engine::engine::ClockOwner;
use collective_engine::fx::InsertPos;
use collective_engine::mixer::MixMode;
use rosc::OscType;

use crate::core::Core;
use crate::model::QUANTIZE_VALUES;
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
    pub browser_zoom: f32,
    pub browser_sort: browser::Col,
    pub browser_asc: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            fx_open: [false; 4],
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
            browser_zoom: 1.0,
            browser_sort: browser::Col::Artist,
            browser_asc: true,
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
                .with_inner_size([1400.0, 880.0])
                .with_min_inner_size([1150.0, 720.0])
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

pub fn fmt_beats(b: f64) -> String {
    if b >= 4.0 && (b / 4.0).fract() == 0.0 {
        format!("{} bar", b / 4.0)
    } else if b >= 1.0 {
        format!("{b} beat")
    } else {
        format!("1/{} beat", (1.0 / b).round())
    }
}

fn key_name(k: egui::Key) -> String {
    format!("{k:?}")
}

impl App {
    fn header(&mut self, ui: &mut Ui, rect: Rect) {
        let core = &mut self.core;
        let s = core.snap;
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::ZERO, BG);
        let y = rect.top() + 3.0;
        let h = rect.height() - 6.0;

        // Logo + version.
        let logo = Rect::from_min_size(Pos2::new(rect.left() + 4.0, y), Vec2::new(112.0, h));
        panel(ui, logo, PANEL);
        let p = ui.painter();
        for (i, bh) in [5.0, 8.0, 11.0].iter().enumerate() {
            p.rect_filled(
                Rect::from_min_size(Pos2::new(logo.left() + 8.0 + i as f32 * 4.0, logo.center().y - 2.0 - bh), Vec2::new(2.5, *bh)),
                CornerRadius::ZERO,
                if i == 2 { RED } else { TEXT },
            );
        }
        p.text(Pos2::new(logo.left() + 22.0, logo.center().y - 8.0), Align2::LEFT_CENTER, "COLLECTIVE", font(13.0), TEXT);
        p.text(Pos2::new(logo.left() + 8.0, logo.bottom() - 4.0), Align2::LEFT_BOTTOM, env!("CARGO_PKG_VERSION"), font(8.0), TEXT_DIM);

        // Mode.
        let mode = Rect::from_min_size(Pos2::new(logo.right() + 4.0, y), Vec2::new(130.0, h));
        let resp = btn_sized(ui, mode, "PERFORM  ▾", Look::On(PANEL), 11.5);
        egui::Popup::menu(&resp).show(|ui| {
            let _ = ui.selectable_label(true, "Perform");
            if ui.button("Settings…").clicked() {
                self.ui.settings_open = true;
            }
            if ui.button("Mapping…").clicked() {
                self.ui.mapping_open = true;
            }
        });

        // Master / cue section.
        let msec = Rect::from_min_size(Pos2::new(mode.right() + 8.0, y), Vec2::new(330.0, h));
        panel(ui, msec, PANEL);
        let lim = core.session.master.limiter;
        let lr = Rect::from_min_size(Pos2::new(msec.left() + 6.0, msec.top() + 5.0), Vec2::new(h - 10.0, h - 10.0));
        let resp = btn_sized(ui, lr, "⋀", if lim { Look::On(RED) } else { Look::Normal }, 13.0)
            .on_hover_text(format!("Master limiter (−0.3 dBFS, before master volume). Reduction {:.1} dB", s.limiter_db));
        learnable(core, &resp, "Header/Limiter");
        if resp.clicked() {
            act_i(core, "Header/Limiter", !lim as i32);
        }
        let mut mv = core.session.master.volume;
        let resp = small_knob(ui, Pos2::new(lr.right() + 18.0, msec.center().y), 11.0, &mut mv, 0.85, false, RED).on_hover_text("Master volume");
        learnable(core, &resp, "Header/MasterVolume");
        if resp.changed() {
            act_f(core, "Header/MasterVolume", mv);
        }
        let mm = Rect::from_min_size(Pos2::new(lr.right() + 38.0, msec.top() + 4.0), Vec2::new(92.0, h - 8.0));
        ui.painter().text(mm.left_top(), Align2::LEFT_TOP, "MASTER", font(8.0), TEXT_DIM);
        dot_meter(ui, Rect::from_min_max(Pos2::new(mm.left(), mm.top() + 11.0), mm.right_bottom()), s.master.peak);
        let cm = Rect::from_min_size(Pos2::new(mm.right() + 10.0, mm.top()), Vec2::new(92.0, h - 8.0));
        ui.painter().text(cm.left_top(), Align2::LEFT_TOP, "CUE", font(8.0), TEXT_DIM);
        dot_meter(ui, Rect::from_min_max(Pos2::new(cm.left(), cm.top() + 11.0), cm.right_bottom()), if core.settings.audio.cue_enabled { s.cue.peak } else { [0.0; 2] });
        let mut mix = core.session.master.cue_mix;
        let kx = cm.right() + 20.0;
        let resp = small_knob(ui, Pos2::new(kx, msec.center().y), 10.0, &mut mix, 0.0, false, RED).on_hover_text("Cue / master mix");
        ui.painter().text(Pos2::new(kx, msec.bottom() - 1.0), Align2::CENTER_BOTTOM, "MIX", font(7.5), TEXT_DIM);
        ui.painter().text(Pos2::new(kx + 30.0, msec.bottom() - 1.0), Align2::CENTER_BOTTOM, "CUE", font(7.5), TEXT_DIM);
        learnable(core, &resp, "Header/CueMix");
        if resp.changed() {
            act_f(core, "Header/CueMix", mix);
        }
        let mut cv = core.session.master.cue_volume;
        let resp = small_knob(ui, Pos2::new(kx + 30.0, msec.center().y), 10.0, &mut cv, 0.75, false, RED).on_hover_text("Cue (headphone) volume");
        learnable(core, &resp, "Header/CueVolume");
        if resp.changed() {
            act_f(core, "Header/CueVolume", cv);
        }

        // Clock section.
        let csec = Rect::from_min_size(Pos2::new(msec.right() + 6.0, y), Vec2::new(440.0, h));
        panel(ui, csec, PANEL);
        let met = core.session.master.metronome;
        let mr = Rect::from_min_size(Pos2::new(csec.left() + 6.0, csec.top() + 5.0), Vec2::new(h - 10.0, h - 10.0));
        let resp = btn_sized(ui, mr, "◷", if met { Look::On(RED) } else { Look::Normal }, 14.0).on_hover_text("Metronome");
        learnable(core, &resp, "Header/Metronome");
        if resp.clicked() {
            act_i(core, "Header/Metronome", !met as i32);
        }
        let mut metv = core.session.master.metronome_volume;
        if small_knob(ui, Pos2::new(mr.right() + 16.0, csec.center().y), 10.0, &mut metv, 0.5, false, RED).changed() {
            core.session.master.metronome_volume = metv;
            core.apply_master();
        }
        let owner = match s.clock.owner {
            ClockOwner::Header => "HEADER MASTER".to_string(),
            ClockOwner::Deck(d) => format!("DECK {} MASTER", d + 1),
        };
        let st_r = Rect::from_min_size(Pos2::new(mr.right() + 34.0, csec.top() + 4.0), Vec2::new(96.0, h - 8.0));
        let p = ui.painter();
        p.text(st_r.left_top(), Align2::LEFT_TOP, format!("LOCAL | {owner}"), font(8.0), TEXT);
        p.text(Pos2::new(st_r.left(), st_r.center().y + 2.0), Align2::LEFT_CENTER, "BAR", font(8.0), TEXT_DIM);
        let bar = Rect::from_min_size(Pos2::new(st_r.left(), st_r.bottom() - 4.0), Vec2::new(st_r.width(), 3.0));
        p.rect_filled(bar, CornerRadius::ZERO, Color32::from_gray(40));
        let prog = ((s.clock.beat / 4.0).rem_euclid(1.0)) as f32;
        p.rect_filled(Rect::from_min_size(bar.min, Vec2::new(bar.width() * prog, 3.0)), CornerRadius::ZERO, RED);

        let mut x = st_r.right() + 8.0;
        let header_owns = s.clock.owner == ClockOwner::Header;
        let r = Rect::from_min_size(Pos2::new(x, csec.top() + 6.0), Vec2::new(24.0, h - 12.0));
        let resp = btn(ui, r, "M", if header_owns { Look::On(SILVER) } else { Look::Normal })
            .on_hover_text("Header owns the clock. Click to hand it to a playing synced deck / take it back.");
        learnable(core, &resp, "Header/Master");
        if resp.clicked() {
            act_i(core, "Header/Master", 1);
        }
        x += 27.0;
        let wc = core.session.wallclock;
        let r = Rect::from_min_size(Pos2::new(x, csec.top() + 6.0), Vec2::new(24.0, h - 12.0));
        let resp = btn(ui, r, "🔒", if wc { Look::On(SILVER) } else { Look::Normal }).on_hover_text("Wallclock: only the header can own the clock");
        learnable(core, &resp, "Header/Wallclock");
        if resp.clicked() {
            act_i(core, "Header/Wallclock", !wc as i32);
        }
        x += 32.0;

        // BPM: drag to change, double-click to type.
        let bpm_r = Rect::from_min_size(Pos2::new(x, csec.top()), Vec2::new(112.0, h));
        if let Some(text) = &mut self.ui.bpm_edit {
            let mut child = ui.new_child(UiBuilder::new().max_rect(bpm_r.shrink(6.0)));
            let r = child.add(egui::TextEdit::singleline(text).font(mono(16.0)).desired_width(90.0));
            if r.lost_focus() {
                if child.input(|i| i.key_pressed(egui::Key::Enter)) {
                    if let Ok(v) = text.trim().parse::<f32>() {
                        act_f(core, "Header/BPM", v);
                    }
                }
                self.ui.bpm_edit = None;
            } else {
                r.request_focus();
            }
        } else {
            let resp = ui.allocate_rect(bpm_r, Sense::click_and_drag()).on_hover_text("Master tempo: drag (Shift: fine), double-click to type");
            let p = ui.painter();
            p.text(Pos2::new(bpm_r.right() - 30.0, bpm_r.center().y), Align2::RIGHT_CENTER, format!("{:.2}", s.clock.bpm), mono(22.0), TEXT);
            p.text(Pos2::new(bpm_r.right() - 4.0, bpm_r.center().y + 6.0), Align2::RIGHT_CENTER, "BPM", font(8.0), TEXT_DIM);
            learnable(core, &resp, "Header/BPM");
            if resp.dragged() {
                let fine = ui.input(|i| i.modifiers.shift);
                let dv = resp.drag_delta().x as f64 * if fine { 0.01 } else { 0.1 };
                if dv != 0.0 {
                    act_f(core, "Header/BPM", (s.clock.bpm + dv) as f32);
                }
            }
            if resp.double_clicked() {
                self.ui.bpm_edit = Some(format!("{:.2}", s.clock.bpm));
            }
        }
        x = bpm_r.right() + 6.0;
        let q_on = core.session.quantize_on;
        let r = Rect::from_min_size(Pos2::new(x, csec.top() + 7.0), Vec2::new(22.0, h - 14.0));
        let resp = btn(ui, r, "Q", if q_on { Look::On(RED) } else { Look::Normal }).on_hover_text("Global quantize");
        learnable(core, &resp, "Header/QuantizeOn");
        if resp.clicked() {
            act_i(core, "Header/QuantizeOn", !q_on as i32);
        }
        let qr = Rect::from_min_size(Pos2::new(x + 26.0, csec.top()), Vec2::new(36.0, h));
        let resp = ui.allocate_rect(qr, Sense::click()).on_hover_text("Quantize value (bars)");
        ui.painter().text(qr.left_center(), Align2::LEFT_CENTER, QUANTIZE_VALUES[core.session.quantize].1, font(14.0), TEXT);
        egui::Popup::menu(&resp).show(|ui| {
            for (i, (_, l)) in QUANTIZE_VALUES.iter().enumerate() {
                if ui.selectable_label(core.session.quantize == i, format!("{l} bar")).clicked() {
                    act_i(core, "Header/Quantize", i as i32 + 1);
                }
            }
        });
        x = qr.right() + 4.0;
        let midi_on = core.midi.activity.elapsed() < Duration::from_millis(150);
        let p = ui.painter();
        p.text(Pos2::new(x, csec.top() + 8.0), Align2::LEFT_CENTER, "MIDI", font(8.0), if midi_on { TEXT } else { TEXT_DIM });
        p.text(Pos2::new(x, csec.bottom() - 8.0), Align2::LEFT_CENTER, "OSC", font(8.0), if core.osc_in.is_some() { TEXT } else { TEXT_DIM });

        // Beat display.
        let bsec = Rect::from_min_size(Pos2::new(csec.right() + 6.0, y), Vec2::new(70.0, h));
        panel(ui, bsec, PANEL);
        let beat = (s.clock.beat.floor() as i64).rem_euclid(4) as usize;
        let p = ui.painter();
        p.text(Pos2::new(bsec.left() + 8.0, bsec.center().y), Align2::LEFT_CENTER, format!("{}", beat + 1), mono(20.0), TEXT);
        p.text(Pos2::new(bsec.left() + 26.0, bsec.top() + 9.0), Align2::LEFT_CENTER, "BEAT", font(7.5), TEXT_DIM);
        for i in 0..4 {
            let r = Rect::from_min_size(Pos2::new(bsec.left() + 26.0 + i as f32 * 9.0, bsec.center().y + 3.0), Vec2::new(7.0, 3.0));
            p.rect_filled(r, CornerRadius::ZERO, if i == beat { RED } else { Color32::from_gray(150) });
        }

        // Right: learn, rec, settings.
        let mut rx = rect.right() - 4.0;
        let gr = Rect::from_min_size(Pos2::new(rx - h, y), Vec2::new(h, h));
        if btn_sized(ui, gr, "⚙", Look::On(PANEL), 15.0).on_hover_text("Settings").clicked() {
            self.ui.settings_open = !self.ui.settings_open;
        }
        rx -= h + 4.0;
        let rec = core.recorder.is_some();
        let rec_txt = match &core.recorder {
            Some(r) => format!("● {}", widgets::fmt_time(r.started.elapsed().as_secs_f64())),
            None => "● REC".into(),
        };
        let rr = Rect::from_min_size(Pos2::new(rx - 70.0, y + 5.0), Vec2::new(70.0, h - 10.0));
        let resp = btn(ui, rr, &rec_txt, if rec { Look::On(RED) } else { Look::Normal });
        learnable(core, &resp, "Header/Record");
        if resp.clicked() {
            core.toggle_recording();
        }
        rx -= 74.0;
        let lr = Rect::from_min_size(Pos2::new(rx - 58.0, y + 5.0), Vec2::new(58.0, h - 10.0));
        let resp = btn(ui, lr, "LEARN", if core.learn { Look::On(Color32::from_rgb(0x3b, 0x7b, 0xe6)) } else { Look::Normal })
            .on_hover_text("MIDI learn: click a control, then move a controller");
        if resp.clicked() {
            core.learn = !core.learn;
            core.last_touched = None;
        }
        if core.learn {
            ui.painter().text(
                Pos2::new(lr.left() - 6.0, lr.center().y),
                Align2::RIGHT_CENTER,
                core.last_touched.clone().unwrap_or_else(|| "click a control…".into()),
                font(9.0),
                Color32::from_rgb(0x6b, 0x9b, 0xf6),
            );
        }
        let mr2 = Rect::from_min_size(Pos2::new(lr.left() - 46.0 - if core.learn { 170.0 } else { 0.0 }, y + 5.0), Vec2::new(42.0, h - 10.0));
        if btn(ui, mr2, "MIDI", Look::Normal).on_hover_text("MIDI & keyboard mappings").clicked() {
            self.ui.mapping_open = !self.ui.mapping_open;
        }
    }

    fn footer(&mut self, ui: &mut Ui, rect: Rect) {
        let core = &self.core;
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::ZERO, Color32::from_gray(12));
        p.text(rect.left_center() + Vec2::new(10.0, 0.0), Align2::LEFT_CENTER, "COLLECTIVE  |  NATIVE ENGINE", font(9.5), TEXT_DIM);
        if let Some((t, m)) = core.status.back() {
            let c = if t.elapsed().as_secs() < 8 { TEXT } else { TEXT_FAINT };
            p.text(rect.center(), Align2::CENTER_CENTER, m, font(9.0), c);
        }
        let owner = match core.snap.clock.owner {
            ClockOwner::Header => "HEADER".to_string(),
            ClockOwner::Deck(d) => format!("DECK {}", d + 1),
        };
        let right = format!("{} TRACKS IN LIBRARY", core.library.len());
        p.text(rect.right_center() - Vec2::new(10.0, 0.0), Align2::RIGHT_CENTER, &right, font(9.5), TEXT);
        let w = right.len() as f32 * 6.2 + 30.0;
        let dsp = core.snap.cpu_load;
        p.text(
            rect.right_center() - Vec2::new(w, 0.0),
            Align2::RIGHT_CENTER,
            format!(
                "CLOCK: {owner} MASTER | {:.2}   ·   DSP {:.0}%{}",
                core.snap.clock.bpm,
                dsp * 100.0,
                if core.snap.compensation_latency > 0 {
                    format!(" · PDC {:.1} ms", core.snap.compensation_latency as f32 / core.sample_rate * 1000.0)
                } else {
                    String::new()
                }
            ),
            font(9.5),
            RED,
        );
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
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.core.tick();
        let ctx = ui.ctx().clone();
        self.handle_keys(&ctx);

        let full = ui.max_rect();
        let header_h = 44.0;
        let footer_h = 22.0;
        egui::Panel::top("header").exact_size(header_h).frame(egui::Frame::NONE).show(ui, |ui| {
            let r = ui.max_rect();
            self.header(ui, r);
        });
        egui::Panel::bottom("footer").exact_size(footer_h).frame(egui::Frame::NONE).show(ui, |ui| {
            let r = ui.max_rect();
            self.footer(ui, r);
        });
        egui::Panel::bottom("browser")
            .resizable(true)
            .default_size(full.height() * 0.40)
            .min_size(140.0)
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                let r = ui.max_rect();
                browser::show(ui, &mut self.core, &mut self.ui, r);
            });
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(BG)).show(ui, |ui| {
            let area = ui.max_rect().shrink(4.0);
            let n = self.core.deck_count();
            let gap = 4.0;
            let mixer_w = (area.width() * 0.19).clamp(230.0, 330.0);
            let deck_w = (area.width() - mixer_w - 2.0 * gap) / 2.0;
            let rows = if n >= 4 { 2 } else { 1 };
            let row_h = (area.height() - gap * (rows as f32 - 1.0)) / rows as f32;
            for row in 0..rows {
                let y = area.top() + row as f32 * (row_h + gap);
                let left = Rect::from_min_size(Pos2::new(area.left(), y), Vec2::new(deck_w, row_h));
                let mid = Rect::from_min_size(Pos2::new(left.right() + gap, y), Vec2::new(mixer_w, row_h));
                let right = Rect::from_min_size(Pos2::new(mid.right() + gap, y), Vec2::new(deck_w, row_h));
                deck::show(ui, &mut self.core, &mut self.ui, row * 2, left);
                deck::show(ui, &mut self.core, &mut self.ui, row * 2 + 1, right);
                let ch_h = if rows == 1 { row_h * 0.78 } else { row_h };
                let cw = (mixer_w - gap) / 2.0;
                for c in 0..2 {
                    let r = Rect::from_min_size(Pos2::new(mid.left() + c as f32 * (cw + gap), y), Vec2::new(cw, ch_h));
                    mixer::channel(ui, &mut self.core, &mut self.ui, row * 2 + c, r);
                }
                if rows == 1 {
                    let tools = Rect::from_min_max(Pos2::new(mid.left(), y + ch_h + gap), mid.right_bottom());
                    panel(ui, tools, PANEL);
                    mixer::mix_tools(ui, &mut self.core, tools);
                }
            }
        });

        // In the 4-deck layout the crossfader / vector pad floats when enabled.
        if self.core.deck_count() >= 4 && self.core.session.mix_mode != MixMode::Off {
            egui::Window::new("Mix").default_size([240.0, 180.0]).resizable(true).show(&ctx, |ui| {
                let r = ui.available_rect_before_wrap();
                let r = Rect::from_min_size(r.min, Vec2::new(r.width().max(200.0), 150.0));
                ui.allocate_rect(r, Sense::hover());
                mixer::mix_tools(ui, &mut self.core, r);
            });
        }

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
