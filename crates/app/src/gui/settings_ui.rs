//! Settings and mapping windows.

use eframe::egui::{self, RichText, Ui};

use dj_engine::deck::TempoRange;
use dj_engine::mixer::FaderCurve;

use super::widgets::*;
use super::UiState;
use crate::core::Core;
use crate::settings::{KeyNotation, MapMode, MidiKind};

pub fn windows(ctx: &egui::Context, core: &mut Core, st: &mut UiState) {
    if st.settings_open {
        let mut open = true;
        egui::Window::new("Settings").open(&mut open).default_width(520.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().max_height(620.0).show(ui, |ui| settings(ui, core, st));
        });
        st.settings_open = open;
    }
    if st.mapping_open {
        let mut open = true;
        egui::Window::new("MIDI & keyboard mapping").open(&mut open).default_width(560.0).show(ctx, |ui| mapping(ui, core));
        st.mapping_open = open;
    }
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(title).strong().color(ACCENT));
}

fn settings(ui: &mut Ui, core: &mut Core, st: &mut UiState) {
    let mut changed_audio = false;
    let mut changed_engine = false;
    let mut changed_osc = false;

    section(ui, "AUDIO");
    let devices = crate::audio::device_names();
    let cur = core.settings.audio.device.clone().unwrap_or_else(|| "System default".into());
    egui::ComboBox::from_label("Output device").selected_text(cur).show_ui(ui, |ui| {
        if ui.selectable_label(core.settings.audio.device.is_none(), "System default").clicked() {
            core.settings.audio.device = None;
            changed_audio = true;
        }
        for d in devices {
            if ui.selectable_label(core.settings.audio.device.as_deref() == Some(d.as_str()), &d).clicked() {
                core.settings.audio.device = Some(d.clone());
                changed_audio = true;
            }
        }
    });
    ui.horizontal(|ui| {
        let mut sr = core.settings.audio.sample_rate.unwrap_or(0);
        egui::ComboBox::from_label("Sample rate").selected_text(if sr == 0 { "Device default".into() } else { format!("{sr} Hz") }).show_ui(ui, |ui| {
            for r in [0u32, 44100, 48000, 88200, 96000] {
                if ui.selectable_value(&mut sr, r, if r == 0 { "Device default".into() } else { format!("{r} Hz") }).clicked() {
                    core.settings.audio.sample_rate = (r != 0).then_some(r);
                    changed_audio = true;
                }
            }
        });
        let mut b = core.settings.audio.buffer_frames.unwrap_or(0);
        egui::ComboBox::from_label("Buffer").selected_text(if b == 0 { "Default".into() } else { format!("{b}") }).show_ui(ui, |ui| {
            for x in [0u32, 64, 128, 256, 512, 1024] {
                if ui.selectable_value(&mut b, x, if x == 0 { "Default".into() } else { format!("{x}") }).clicked() {
                    core.settings.audio.buffer_frames = (x != 0).then_some(x);
                    changed_audio = true;
                }
            }
        });
    });
    ui.horizontal(|ui| {
        ui.label("Master → pair");
        changed_audio |= ui.add(egui::DragValue::new(&mut core.settings.audio.master_pair).range(0..=7).custom_formatter(|v, _| pair(v as u8))).changed();
        changed_audio |= ui.checkbox(&mut core.settings.audio.cue_enabled, "Enable cue output").changed();
        if core.settings.audio.cue_enabled {
            ui.label("Cue → pair");
            changed_audio |= ui.add(egui::DragValue::new(&mut core.settings.audio.cue_pair).range(0..=7).custom_formatter(|v, _| pair(v as u8))).changed();
        }
    });
    if ui.checkbox(&mut core.settings.audio.split_cue, "Split cue (cue left / master right) for single-pair interfaces").changed() {
        changed_engine = true;
    }
    if changed_audio {
        ui.label(RichText::new("Audio device changes apply after restarting dj.").color(WARN));
        core.log("Audio settings saved; restart to apply");
    }

    let mut ext = core.settings.external_mixer;
    if ui
        .checkbox(&mut ext, "External mixer: send each deck to its own output and hide the mixer section")
        .changed()
    {
        core.set_external_mixer(ext);
    }
    let pairs = core.output_pairs();
    let routing: Vec<String> = (0..core.deck_count())
        .map(|d| {
            let p = core.external_pair(d);
            format!("deck {} → {}/{}", d + 1, p * 2 + 1, p * 2 + 2)
        })
        .collect();
    let info = if pairs >= 2 {
        format!("The device has {} outputs. With an external mixer: {}.", core.audio_channels, routing.join(", "))
    } else {
        format!("The device has {} outputs; an external mixer needs at least 4 (two stereo pairs).", core.audio_channels)
    };
    ui.label(RichText::new(info).small().weak());

    section(ui, "MIXER");
    egui::ComboBox::from_label("Fader curve").selected_text(core.settings.fader_curve.label()).show_ui(ui, |ui| {
        for c in FaderCurve::ALL {
            if ui.selectable_value(&mut core.settings.fader_curve, c, c.label()).clicked() {
                changed_engine = true;
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label("EQ crossovers");
        changed_engine |= ui.add(egui::DragValue::new(&mut core.settings.crossover_low_hz).range(60.0..=800.0).suffix(" Hz")).changed();
        changed_engine |= ui.add(egui::DragValue::new(&mut core.settings.crossover_high_hz).range(1000.0..=8000.0).suffix(" Hz")).changed();
        if ui.button("Defaults").clicked() {
            core.settings.crossover_low_hz = 250.0;
            core.settings.crossover_high_hz = 2500.0;
            changed_engine = true;
        }
    });
    // With an external mixer, volume, EQ and filter are on the hardware: nothing to choose here.
    if !core.external_routing() {
        ui.horizontal(|ui| {
            ui.label("Direct outputs apply:");
            let e = &mut core.settings.ext_stages;
            changed_engine |= ui.checkbox(&mut e.trim, "Trim").changed();
            changed_engine |= ui.checkbox(&mut e.eq, "EQ").changed();
            changed_engine |= ui.checkbox(&mut e.filter, "Filter").changed();
            changed_engine |= ui.checkbox(&mut e.volume, "Volume").changed();
        });
    }

    section(ui, "DECKS");
    ui.horizontal(|ui| {
        ui.label("Decks");
        ui.selectable_value(&mut core.settings.deck_count, 2, "2");
        ui.selectable_value(&mut core.settings.deck_count, 4, "4");
    });
    // Applies to every deck now (each deck's menu can still change its own).
    egui::ComboBox::from_label("Tempo range (all decks)").selected_text(core.settings.tempo_range.label()).show_ui(ui, |ui| {
        for (i, r) in TempoRange::ALL.into_iter().enumerate() {
            if ui.selectable_value(&mut core.settings.tempo_range, r, r.label()).clicked() {
                for d in 0..4 {
                    core.route(&format!("Deck/{}/TempoRange", d + 1), &[rosc::OscType::Int(i as i32)]);
                }
            }
        }
    });
    let steps: [(f32, &str); 6] = [(0.0, "Continuous"), (0.01, "0.01 %"), (0.05, "0.05 %"), (0.1, "0.1 %"), (0.5, "0.5 %"), (1.0, "1 %")];
    let current = steps.iter().find(|(v, _)| (*v - core.settings.tempo_step_pct).abs() < 1e-6).map_or("Custom", |(_, l)| *l);
    egui::ComboBox::from_label("Tempo fader steps").selected_text(current).show_ui(ui, |ui| {
        for (v, l) in steps {
            ui.selectable_value(&mut core.settings.tempo_step_pct, v, l);
        }
    });
    ui.checkbox(&mut core.settings.keylock_on_load, "Key lock (pitch preserve) on load");
    ui.checkbox(&mut core.settings.tempo_home, "After a mix, ease back to the new track's own tempo")
        .on_hover_text("A mix plays the new track at the old one's tempo. Afterwards it glides back to its own (0 %), 0.25 % a bar, too slowly to hear, so a set doesn't drift ever further from the songs' tempos (real DJs keep most tracks within 5 %). Moving its tempo fader stops it.");
    ui.checkbox(&mut core.settings.load_at_first_cue, "Load tracks at their first cue");
    if cfg!(target_os = "macos") {
        ui.checkbox(&mut core.settings.keep_awake, "Keep the Mac awake while dj runs")
            .on_hover_text("No screen dimming, screen off or sleep in the middle of a set (as long as dj is open; closing the lid still sleeps).");
    }
    ui.horizontal(|ui| {
        use crate::settings::CuePadMode;
        let mut on = core.settings.cue_pads == CuePadMode::Loop;
        if ui
            .checkbox(&mut on, "Hot cue pads loop")
            .on_hover_text("On: a set pad starts a loop at its cue, of the loop length. Off: it jumps to its cue and plays on. (On screen and on your controller.)")
            .changed()
        {
            core.settings.cue_pads = if on { CuePadMode::Loop } else { CuePadMode::Jump };
        }
        // The loop length: for pads, the loop button, and the decks' loop size until changed.
        let mut beats = core.settings.default_loop_beats;
        egui::ComboBox::from_id_salt("default-loop")
            .selected_text(format!("{} beats", beats))
            .show_ui(ui, |ui| {
                for b in [1.0, 2.0, 4.0, 8.0, 16.0, 32.0] {
                    ui.selectable_value(&mut beats, b, format!("{b} beats"));
                }
            })
            .response
            .on_hover_text("Default loop length (a loop on a hot cue, the loop button)");
        if beats != core.settings.default_loop_beats {
            core.settings.default_loop_beats = beats;
            core.set_default_loop(beats);
        }
    });
    ui.horizontal(|ui| {
        use crate::settings::CuePoints;
        ui.label("Cue points");
        ui.selectable_value(&mut core.settings.cue_points, CuePoints::Tags, "Mixed In Key")
            .on_hover_text("The tags' cue points; dj detects them only for tracks without any");
        ui.selectable_value(&mut core.settings.cue_points, CuePoints::Detected, "dj's detection")
            .on_hover_text("dj finds cue points in every track from its stems (Intro, Drop, Break, Vocal, Outro). Cues you set yourself are kept.");
    });
    ui.label(
        RichText::new("Tracks without a BPM tag get a detected tempo and beat grid (shown with ≈), and an estimated energy. Keys are never guessed.")
            .small()
            .weak(),
    );
    ui.checkbox(&mut core.settings.match_loudness, "Match loudness")
        .on_hover_text("Measure each track's loudness when it loads and bring it to the same level, so a mix doesn't jump. Applies to tracks loaded after the change.");
    ui.horizontal(|ui| {
        ui.label("Tape stop");
        changed_engine |= ui.add(egui::DragValue::new(&mut core.settings.tape_stop_ms).range(0.0..=3000.0).suffix(" ms")).changed();
        ui.label("Loop size");
        let resp = ui.add(egui::DragValue::new(&mut core.settings.default_loop_beats).range(1.0..=64.0).suffix(" beats"))
            .on_hover_text("Every deck's loop size (the ÷2 / ×2 buttons change one deck's)");
        if resp.changed() {
            for d in 0..4 {
                core.route(&format!("Deck/{}/LoopSize", d + 1), &[rosc::OscType::Float(core.settings.default_loop_beats as f32)]);
            }
        }
        ui.label("Skip");
        ui.add(egui::DragValue::new(&mut core.settings.skip_beats).range(1.0..=32.0).suffix(" beats"));
    });
    egui::ComboBox::from_label("Key notation")
        .selected_text(match core.settings.key_notation {
            KeyNotation::Camelot => "Camelot (8A)",
            KeyNotation::Musical => "Musical (Am)",
            KeyNotation::OpenKey => "Open Key (1m)",
        })
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut core.settings.key_notation, KeyNotation::Camelot, "Camelot (8A)");
            ui.selectable_value(&mut core.settings.key_notation, KeyNotation::Musical, "Musical (Am)");
            ui.selectable_value(&mut core.settings.key_notation, KeyNotation::OpenKey, "Open Key (1m)");
        });

    section(ui, "LIBRARY");
    let mut remove = None;
    for (i, f) in core.settings.library_folders.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(f.display().to_string());
            if ui.small_button("✕").clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        core.settings.library_folders.remove(i);
        core.rescan_library(); // its tracks leave the browser
    }
    ui.horizontal(|ui| {
        let edit = ui.add(egui::TextEdit::singleline(&mut st.new_folder).hint_text("/path/to/music").desired_width(300.0));
        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (ui.button("Add folder").clicked() || enter) && !st.new_folder.trim().is_empty() {
            core.add_library_folder(st.new_folder.trim().into());
            st.new_folder.clear();
        }
        if ui.button("Choose…").clicked() {
            if let Some(dir) = rfd::FileDialog::new().set_title("Import music folder").pick_folder() {
                core.add_library_folder(dir);
            }
        }
        if ui.button("Rescan").clicked() {
            core.rescan_library();
        }
    });
    ui.label(RichText::new("Every subfolder is included, at any depth.").small().weak());

    section(ui, "OSC");
    changed_osc |= ui.checkbox(&mut core.settings.osc_enabled, "OSC input").changed();
    ui.horizontal(|ui| {
        ui.label("Listen port");
        changed_osc |= ui.add(egui::DragValue::new(&mut core.settings.osc_port).range(1024..=65535)).changed();
        ui.label("Address root");
        changed_osc |= ui.add(egui::TextEdit::singleline(&mut core.settings.osc_root).desired_width(120.0)).lost_focus();
        if ui.small_button("Syndicate-compatible").on_hover_text("Use /Syndicate/ so existing controller layouts work").clicked() {
            core.settings.osc_root = "/Syndicate/".into();
            changed_osc = true;
        }
    });
    ui.horizontal(|ui| {
        changed_osc |= ui.checkbox(&mut core.settings.osc_feedback, "Feedback to").changed();
        changed_osc |= ui.add(egui::TextEdit::singleline(&mut core.settings.osc_feedback_host).desired_width(110.0)).lost_focus();
        changed_osc |= ui.add(egui::DragValue::new(&mut core.settings.osc_feedback_port).range(1..=65535)).changed();
        if ui.button("Resend state").clicked() {
            core.route("Feedback/Resend", &[rosc::OscType::Int(1)]);
        }
    });

    section(ui, "MIDI");
    let (ins, outs) = crate::midi::ports_cached();
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(500));
    ui.label(RichText::new("Inputs (none ticked = all)").small());
    for p in ins {
        let mut on = core.settings.midi_inputs.contains(&p);
        if ui.checkbox(&mut on, &p).changed() {
            if on {
                core.settings.midi_inputs.push(p.clone());
            } else {
                core.settings.midi_inputs.retain(|x| x != &p);
            }
            core.midi = crate::midi::Midi::new(&core.settings);
        }
    }
    egui::ComboBox::from_label("Output (LEDs, clock)").selected_text(core.settings.midi_output.clone().unwrap_or_else(|| "None".into())).show_ui(ui, |ui| {
        if ui.selectable_label(core.settings.midi_output.is_none(), "None").clicked() {
            core.settings.midi_output = None;
            core.midi = crate::midi::Midi::new(&core.settings);
        }
        for p in outs {
            if ui.selectable_label(core.settings.midi_output.as_deref() == Some(p.as_str()), &p).clicked() {
                core.settings.midi_output = Some(p.clone());
                core.midi = crate::midi::Midi::new(&core.settings);
            }
        }
    });
    if ui.checkbox(&mut core.settings.midi_clock_out, "Send MIDI clock").changed() {
        core.midi.set_clock_running(core.settings.midi_clock_out);
    }

    section(ui, "STEMS");
    ui.checkbox(&mut core.settings.stems_enabled, "Separate loaded tracks into stems (drums, bass, vocals, other)");
    if core.settings.stems_enabled {
        ui.checkbox(&mut core.settings.song_sections, "Find song sections (intro, verse, chorus, break, outro)")
            .on_hover_text("With All-In-One, once a track's stems are ready (about a minute the first time, then kept with the stems). The auto mix uses them to leave and land on the song's real parts.");
        // HT-Demucs: best quality, offline, downloaded on request.
        let mut demucs = core.settings.stems_demucs.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label("Demucs model");
            let edit = egui::TextEdit::singleline(&mut demucs).hint_text("off: download it, or a path to a Demucs .onnx").desired_width(300.0);
            if ui.add(edit).changed() {
                let d = demucs.trim();
                core.settings.stems_demucs = (!d.is_empty()).then(|| d.into());
            }
        });
        let download = core.stems.demucs_download.as_ref().map(|d| d.state());
        match download {
            Some(crate::stems::DownloadState::Running { done, total }) => {
                let mb = |b: u64| b as f64 / 1e6;
                let text = match total {
                    Some(t) => format!("Downloading HT-Demucs… {:.0} of {:.0} MB", mb(done), mb(t)),
                    None => format!("Downloading HT-Demucs… {:.0} MB", mb(done)),
                };
                ui.label(RichText::new(text).small());
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
            }
            Some(crate::stems::DownloadState::Done(path)) => {
                core.settings.stems_demucs = Some(path);
                core.stems.demucs_download = None;
            }
            other => {
                if let Some(crate::stems::DownloadState::Failed(e)) = other {
                    ui.label(RichText::new(format!("Download failed: {e}")).small().color(egui::Color32::from_rgb(230, 90, 80)));
                }
                let missing = core.settings.stems_demucs.as_ref().is_none_or(|p| !p.exists());
                if missing && ui.button("Download HT-Demucs (300 MB)").clicked() {
                    core.stems.demucs_download =
                        Some(crate::stems::Download::start(crate::stems::DEMUCS_URL, crate::stems::default_demucs_path()));
                }
            }
        }
        let gpu = if cfg!(target_os = "macos") {
            "Separate on the Neural Engine (CoreML)"
        } else if cfg!(windows) {
            "Separate on the GPU (DirectML)"
        } else {
            "Separate with acceleration (not on this system: runs on the CPU)"
        };
        ui.checkbox(&mut core.settings.stems_gpu, gpu).on_hover_text(
            "Switch it off to compare. Takes effect from the next track separated. On a Mac it uses the Neural Engine, not the GPU (the GPU draws dj's window: busy with stems, the window hangs); the first track takes a few minutes longer while CoreML compiles the model (kept for next time). Where it can't be used, it runs on the CPU, and the stems status says why.",
        );
        ui.label(
            RichText::new(
                "Best-quality stems, made when a track loads (about 1–3 minutes; the deck plays the full mix meanwhile) and kept in the prepared-stems folder. HT-Demucs as converted by the Mixxx project; its weights are for non-commercial use only.",
            )
            .small()
            .weak(),
        );

        let mut prepared = core.settings.stems_prepared_dir().display().to_string();
        ui.horizontal(|ui| {
            ui.label("Prepared stems");
            if ui.add(egui::TextEdit::singleline(&mut prepared).desired_width(300.0)).changed() {
                let p = prepared.trim();
                core.settings.stems_prepared_dir = (!p.is_empty()).then(|| p.into());
            }
        });
        ui.label(
            RichText::new("Separation runs in the background after a track loads; the deck plays the full mix until the stems are ready. Prepared stems take about 32 MB per minute of audio. Without HT-Demucs (or prepared stems) tracks have no stems.")
                .small()
                .weak(),
        );
    }

    section(ui, "VST3");
    if ui.checkbox(&mut core.settings.vst3_enabled, "Enable VST3 effects").changed() && core.settings.vst3_enabled {
        core.rescan_vst3();
    }
    for p in dj_vst3::default_search_paths() {
        ui.label(RichText::new(p.display().to_string()).small().weak());
    }
    let mut remove = None;
    for (i, p) in core.settings.vst3_paths.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(RichText::new(p.display().to_string()).small());
            if ui.small_button("✕").clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        core.settings.vst3_paths.remove(i);
        if core.settings.vst3_enabled {
            core.rescan_vst3();
        }
    }
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut st.new_vst_path).hint_text("extra VST3 folder").desired_width(260.0));
        if ui.button("Add").clicked() && !st.new_vst_path.trim().is_empty() {
            core.settings.vst3_paths.push(st.new_vst_path.trim().into());
            st.new_vst_path.clear();
            if core.settings.vst3_enabled {
                core.rescan_vst3();
            }
        }
        if ui.button("Rescan").clicked() {
            core.rescan_vst3();
        }
    });
    ui.label(RichText::new("Each plugin is scanned in a separate process, so a broken plugin is listed as failed instead of crashing the app.").small().weak());

    if changed_engine {
        let s = core.settings.clone();
        core.send(dj_engine::Command::FaderCurve(s.fader_curve));
        core.send(dj_engine::Command::Crossovers { low_hz: s.crossover_low_hz, high_hz: s.crossover_high_hz });
        core.send(dj_engine::Command::ExtStages(s.ext_stages));
        core.send(dj_engine::Command::SplitCue(s.audio.split_cue));
        for d in 0..4u8 {
            core.send(dj_engine::Command::Deck { deck: d, cmd: dj_engine::engine::DeckCmd::TapeStopMs(s.tape_stop_ms) });
        }
    }
    if changed_osc {
        if !core.settings.osc_root.starts_with('/') {
            core.settings.osc_root.insert(0, '/');
        }
        if !core.settings.osc_root.ends_with('/') {
            core.settings.osc_root.push('/');
        }
        core.start_osc();
    }
    ui.add_space(8.0);
    if ui.button("Save settings").clicked() {
        core.save_all();
        core.log("Settings saved");
    }
}

fn pair(i: u8) -> String {
    format!("{}/{}", i as u32 * 2 + 1, i as u32 * 2 + 2)
}

fn mapping(ui: &mut Ui, core: &mut Core) {
    ui.horizontal(|ui| {
        let r = lit(ui, core.learn, "LEARN", ACCENT);
        if r.clicked() {
            core.learn = !core.learn;
            core.last_touched = None;
        }
        ui.label(
            RichText::new("Turn on LEARN, click any control in the app, then move a MIDI control. Inputs: ")
                .small(),
        );
        ui.label(RichText::new(core.midi.input_names.join(", ")).small().weak());
    });
    if let Some(t) = &core.last_touched {
        ui.label(RichText::new(format!("Waiting for MIDI for: {t}")).color(ACCENT));
    }
    // Ready-made mappings.
    ui.horizontal(|ui| {
        ui.label("Add mapping for");
        if ui.button("Pioneer OPUS-QUAD").on_hover_text("Its PC control mode (BROWSE > MIDI > LOAD). Replaces mappings on the same MIDI controls; select it as the MIDI output for its lights.").clicked() {
            let n = crate::presets::apply(&mut core.profile, crate::presets::opus_quad());
            let _ = core.profile.save();
            core.log(format!("OPUS-QUAD: {n} controls mapped"));
        }
    });
    // Import a mapping exported from Syndicate.
    ui.horizontal(|ui| {
        let id = egui::Id::unique("sdcmap-path");
        let mut path = ui.data_mut(|d| d.get_temp::<String>(id)).unwrap_or_default();
        ui.label("Import Syndicate mapping");
        ui.add(egui::TextEdit::singleline(&mut path).hint_text("/path/to/midi_mapping.sdcmap").desired_width(260.0));
        if ui.button("Import").clicked() && !path.trim().is_empty() {
            let msg = match crate::import_mapping_file(std::path::Path::new(path.trim()), &mut core.profile, &mut core.settings) {
                Ok(summary) => summary,
                Err(e) => format!("Import failed: {e}"),
            };
            core.log(msg);
            path.clear();
        }
        ui.data_mut(|d| d.insert_temp(id, path));
    });
    ui.separator();
    let mut remove = None;
    let mut changed = false;
    egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
        egui::Grid::new("maps").striped(true).num_columns(5).show(ui, |ui| {
            ui.label(RichText::new("MIDI").strong());
            ui.label(RichText::new("Address").strong());
            ui.label(RichText::new("Mode").strong());
            ui.label(RichText::new("LED").strong());
            ui.label("");
            ui.end_row();
            for (i, m) in core.profile.mappings.iter_mut().enumerate() {
                let kind = match m.source.kind {
                    MidiKind::Note => "Note",
                    MidiKind::Cc => "CC",
                    MidiKind::PitchBend => "Bend",
                };
                ui.label(format!("{kind} {} · ch{}", m.source.number, m.source.channel + 1));
                changed |= ui.add(egui::TextEdit::singleline(&mut m.address).desired_width(220.0)).changed();
                egui::ComboBox::from_id_salt(("mode", i))
                    .selected_text(match m.mode {
                        MapMode::Absolute => "Absolute",
                        MapMode::Toggle => "Toggle",
                        MapMode::Relative => "Relative",
                        MapMode::RelativeCentred => "Relative (64)",
                    })
                    .show_ui(ui, |ui| {
                        changed |= ui.selectable_value(&mut m.mode, MapMode::Absolute, "Absolute").changed();
                        changed |= ui.selectable_value(&mut m.mode, MapMode::Toggle, "Toggle").changed();
                        changed |= ui.selectable_value(&mut m.mode, MapMode::Relative, "Relative").changed();
                        changed |= ui.selectable_value(&mut m.mode, MapMode::RelativeCentred, "Relative (64)").changed();
                    });
                changed |= ui.checkbox(&mut m.feedback, "").changed();
                if ui.small_button("✕").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
    });
    if let Some(i) = remove {
        core.profile.mappings.remove(i);
        changed = true;
    }
    ui.separator();
    ui.label(RichText::new("Keyboard").strong());
    let mut kremove = None;
    egui::Grid::new("keys").striped(true).show(ui, |ui| {
        for (i, (k, a)) in core.profile.keys.iter_mut().enumerate() {
            changed |= ui.add(egui::TextEdit::singleline(k).desired_width(70.0)).changed();
            changed |= ui.add(egui::TextEdit::singleline(a).desired_width(240.0)).changed();
            if ui.small_button("✕").clicked() {
                kremove = Some(i);
            }
            ui.end_row();
        }
    });
    if let Some(i) = kremove {
        core.profile.keys.remove(i);
        changed = true;
    }
    ui.horizontal(|ui| {
        if ui.button("+ key").clicked() {
            core.profile.keys.push(("F1".into(), "Deck/1/PlayPause".into()));
            changed = true;
        }
        ui.label(
            RichText::new("Addresses are the OSC API without the root, e.g. Deck/1/Cue3, Mixer/CH2/FX/Macro/1. Add :value to fix a value, e.g. Deck/1/BeatSkip:-8.")
                .small()
                .weak(),
        );
    });
    if changed {
        let _ = core.profile.save();
    }
}
