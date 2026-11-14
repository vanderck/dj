use std::sync::atomic::Ordering;
use std::sync::Arc;

use nih_plug::prelude::*;
use nih_plug_egui::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use nih_plug_egui::{create_egui_editor, resizable_window::ResizableWindow, widgets};
use stems_core::{band_center_hz, SoundfieldFrame, Stem, SOUNDFIELD_MAX_HZ, SOUNDFIELD_MIN_HZ};

use crate::params::{FocusModeParam, StemsParams};
use crate::UiShared;

const STEM_COLORS: [Color32; 4] = [
    Color32::from_rgb(0xF2, 0x8C, 0x28), // Drums
    Color32::from_rgb(0x3D, 0x8B, 0xFD), // Bass
    Color32::from_rgb(0xE8, 0x4A, 0x8A), // Vocals
    Color32::from_rgb(0x4C, 0xC9, 0x8E), // Other
];

pub(crate) fn create(
    params: Arc<StemsParams>,
    ui_shared: Arc<UiShared>,
) -> Option<Box<dyn Editor>> {
    let egui_state = params.editor_state.clone();
    create_egui_editor(
        params.editor_state.clone(),
        (),
        |_, _| {},
        move |ctx, setter, _| {
            let frame: SoundfieldFrame = *ui_shared.soundfield.lock().unwrap().read();
            ResizableWindow::new("stems-window")
                .min_size(Vec2::new(720.0, 460.0))
                .show(ctx, egui_state.as_ref(), |ui| {
                    header(ui, &ui_shared);
                    ui.separator();
                    ui.horizontal_top(|ui| {
                        soundfield(ui, &frame, &params);
                        ui.add_space(12.0);
                        ui.vertical(|ui| {
                            for stem in Stem::ALL {
                                stem_strip(ui, setter, &params, stem);
                            }
                            ui.separator();
                            focus_section(ui, setter, &params);
                            ui.separator();
                            ui.horizontal(|ui| {
                                ui.label("Output");
                                ui.add(widgets::ParamSlider::for_param(
                                    &params.output_gain,
                                    setter,
                                ));
                            });
                        });
                    });
                });
            ctx.request_repaint();
        },
    )
}

fn header(ui: &mut egui::Ui, shared: &UiShared) {
    let latency = shared.latency_samples.load(Ordering::Relaxed);
    let sr = shared.sample_rate.load(Ordering::Relaxed).max(1);
    let info = shared.separator.lock().unwrap().clone();
    ui.horizontal(|ui| {
        ui.heading("Stems");
        ui.label(
            egui::RichText::new(format!(
                "latency {} samples ({:.0} ms)",
                latency,
                latency as f32 * 1000.0 / sr as f32
            ))
            .weak(),
        );
    });
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("Separator: {}", info.name)).small());
        if let Some(stats) = &info.stats {
            let ms = |us: u32| us as f32 / 1000.0;
            let text = format!(
                "· infer {:.1} ms (max {:.1}) · underruns {}",
                ms(stats.last_infer_us.load(Ordering::Relaxed)),
                ms(stats.max_infer_us.load(Ordering::Relaxed)),
                stats.underruns.load(Ordering::Relaxed),
            );
            ui.label(egui::RichText::new(text).small().weak());
            if let Some(err) = stats.error.lock().unwrap().as_ref() {
                ui.colored_label(Color32::LIGHT_RED, format!("model error: {err}"));
            }
        }
    });
    if let Some(note) = &info.note {
        ui.label(
            egui::RichText::new(note)
                .small()
                .color(Color32::from_rgb(0xE0, 0xB0, 0x40)),
        );
    }
}

fn soundfield(ui: &mut egui::Ui, frame: &SoundfieldFrame, params: &StemsParams) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(380.0, 400.0), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, Color32::from_gray(18));

    let log_span = (SOUNDFIELD_MAX_HZ / SOUNDFIELD_MIN_HZ).ln();
    let y_for_hz = |hz: f32| {
        let t =
            (hz.clamp(SOUNDFIELD_MIN_HZ, SOUNDFIELD_MAX_HZ) / SOUNDFIELD_MIN_HZ).ln() / log_span;
        rect.bottom() - t * rect.height()
    };
    let x_for_pan = |pan: f32| rect.center().x + pan.clamp(-1.0, 1.0) * rect.width() * 0.48;

    // Focus EQ selection.
    let focus = &params.focus;
    if focus.mode.value() != FocusModeParam::Off {
        let half = focus.pan_width.value() * 0.5;
        let c = focus.pan_center.value();
        let sel = Rect::from_min_max(
            Pos2::new(x_for_pan(c - half), y_for_hz(focus.high_hz.value())),
            Pos2::new(x_for_pan(c + half), y_for_hz(focus.low_hz.value())),
        );
        painter.rect_filled(sel, 2.0, Color32::from_white_alpha(22));
    }

    // Grid.
    let grid = Stroke::new(1.0_f32, Color32::from_gray(45));
    let label = Color32::from_gray(110);
    for hz in [100.0, 1_000.0, 10_000.0] {
        let y = y_for_hz(hz);
        painter.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            grid,
        );
        let text = if hz >= 1_000.0 {
            format!("{}k", hz / 1_000.0)
        } else {
            format!("{hz}")
        };
        painter.text(
            Pos2::new(rect.left() + 4.0, y - 2.0),
            Align2::LEFT_BOTTOM,
            text,
            FontId::proportional(10.0),
            label,
        );
    }
    for (pan, text) in [(-1.0, "L"), (0.0, "C"), (1.0, "R")] {
        let x = x_for_pan(pan);
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            grid,
        );
        painter.text(
            Pos2::new(x, rect.bottom() - 2.0),
            Align2::CENTER_BOTTOM,
            text,
            FontId::proportional(10.0),
            label,
        );
    }

    // One dot per stem and band: x = stereo position, y = frequency, size/alpha = level.
    for (stem, points) in frame.points.iter().enumerate() {
        let color = STEM_COLORS[stem];
        for (band, p) in points.iter().enumerate() {
            let strength = ((p.level_db + 70.0) / 60.0).clamp(0.0, 1.0);
            if strength <= 0.0 {
                continue;
            }
            let pos = Pos2::new(x_for_pan(p.pan), y_for_hz(band_center_hz(band)));
            let c = Color32::from_rgba_unmultiplied(
                color.r(),
                color.g(),
                color.b(),
                (60.0 + 195.0 * strength) as u8,
            );
            painter.circle_filled(pos, 1.5 + 6.0 * strength, c);
        }
    }
}

fn stem_strip(ui: &mut egui::Ui, setter: &ParamSetter, params: &StemsParams, stem: Stem) {
    let p = params.stem(stem);
    ui.horizontal(|ui| {
        ui.add_sized(
            Vec2::new(56.0, 18.0),
            egui::Label::new(
                egui::RichText::new(stem.name())
                    .color(STEM_COLORS[stem as usize])
                    .strong(),
            ),
        );
        toggle(ui, setter, &p.mute, "M");
        toggle(ui, setter, &p.solo, "S");
        ui.add(widgets::ParamSlider::for_param(&p.gain, setter).with_width(150.0));
        ui.label("Iso");
        ui.add(widgets::ParamSlider::for_param(&p.isolation, setter).with_width(90.0));
    });
}

fn focus_section(ui: &mut egui::Ui, setter: &ParamSetter, params: &StemsParams) {
    let f = &params.focus;
    ui.horizontal(|ui| {
        ui.strong("Focus EQ");
        let current = f.mode.value();
        for (mode, text) in [
            (FocusModeParam::Off, "Off"),
            (FocusModeParam::Isolate, "Isolate"),
            (FocusModeParam::Extract, "Extract"),
        ] {
            if ui.selectable_label(current == mode, text).clicked() && current != mode {
                setter.begin_set_parameter(&f.mode);
                setter.set_parameter(&f.mode, mode);
                setter.end_set_parameter(&f.mode);
            }
        }
    });
    egui::Grid::new("focus-grid").num_columns(2).show(ui, |ui| {
        for (name, param) in [
            ("Low", &f.low_hz),
            ("High", &f.high_hz),
            ("Pan", &f.pan_center),
            ("Width", &f.pan_width),
        ] {
            ui.label(name);
            ui.add(widgets::ParamSlider::for_param(param, setter).with_width(200.0));
            ui.end_row();
        }
    });
    ui.label(
        egui::RichText::new("The complementary signal goes to the \"Focus\" aux output.")
            .weak()
            .small(),
    );
}

fn toggle(ui: &mut egui::Ui, setter: &ParamSetter, param: &BoolParam, text: &str) {
    let mut value = param.value();
    if ui.toggle_value(&mut value, text).changed() {
        setter.begin_set_parameter(param);
        setter.set_parameter(param, value);
        setter.end_set_parameter(param);
    }
}
