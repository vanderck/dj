//! dj — live performance DJ software.

// Windows: a GUI app, so no console window opens behind it (command-line use attaches to the
// terminal it was started from, see `attach_console`).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod audio;
mod core;
mod fxcat;
mod gui;
mod midi;
mod model;
mod osc;
mod profile;
mod settings;
mod stems;
mod syndicate_map;
mod transition;

use std::path::PathBuf;
use std::time::Duration;

use crate::core::Core;
use crate::model::Session;
use crate::settings::Settings;

const USAGE: &str = "\
dj — live performance DJ software

USAGE:
    dj                 start the GUI
    dj --headless      run the engine with OSC/MIDI control only
    dj --info FILE     print tags (BPM, key, energy, cues) for a file
    dj --list-devices  list audio and MIDI devices
    dj --import-mapping FILE.sdcmap
                               import a Syndicate MIDI mapping into dj's mappings
    dj --download-demucs
                               download HT-Demucs (300 MB) and use it for stems
    dj --prepare-stems PATH...
                               separate files / folders with Demucs into the
                               prepared-stems folder (Settings → Stems)
    dj --vst3-scan B   (internal) scan one VST3 bundle and print JSON
";

/// Windows: show command-line output (`--help`, `--info`, …) in the terminal dj was started from.
#[cfg(windows)]
fn attach_console() {
    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    // SAFETY: plain Win32 call; failing (no parent console) is harmless.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    #[cfg(windows)]
    if args.len() > 1 && args[1] != "--vst3-scan" {
        attach_console(); // (the plugin scanner talks to dj over a pipe, not a terminal)
    }
    match args.get(1).map(|s| s.as_str()) {
        Some("--vst3-scan") => {
            let code = args.get(2).map(|b| fxcat::scan_child_main(&PathBuf::from(b))).unwrap_or(2);
            std::process::exit(code);
        }
        Some("--import-mapping") => {
            let Some(p) = args.get(2) else {
                eprintln!("{USAGE}");
                std::process::exit(2);
            };
            let mut profile = settings::MappingProfile::load();
            let mut settings = Settings::load();
            match import_mapping_file(&PathBuf::from(p), &mut profile, &mut settings) {
                Ok(summary) => {
                    println!("{summary}");
                    println!("Saved to {}", settings::MappingProfile::path().display());
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        Some("--prepare-stems") => std::process::exit(prepare_stems(&args[2..])),
        Some("--download-demucs") => std::process::exit(download_demucs()),
        Some("--list-devices") => {
            println!("Audio outputs:");
            for d in audio::device_names() {
                println!("  {d}");
            }
            let (ins, outs) = midi::list_ports();
            println!("MIDI inputs:");
            for p in ins {
                println!("  {p}");
            }
            println!("MIDI outputs:");
            for p in outs {
                println!("  {p}");
            }
        }
        Some("--info") => {
            let Some(p) = args.get(2) else {
                eprintln!("{USAGE}");
                std::process::exit(2);
            };
            match dj_library::tags::read(&PathBuf::from(p)) {
                Ok(t) => {
                    // Tags, plus what dj makes of the track when it loads it: the beat grid it plays
                    // by (checked against the audio) and the loudness it measures.
                    let mut v = serde_json::to_value(&t).unwrap_or_default();
                    let loaded = dj_library::load(&PathBuf::from(p), None, false, dj_library::DetectOptions::default()).ok();
                    if let (Some(o), Some(l)) = (v.as_object_mut(), loaded) {
                        o.insert("loudness_lufs".into(), l.loudness.map(|x| (x * 10.0).round() / 10.0).into());
                        if let Some(g) = l.track.grid {
                            o.insert("dj_grid".into(), serde_json::json!({ "bpm": g.bpm, "anchor_secs": g.anchor_secs, "source": format!("{:?}", l.grid_source) }));
                        }
                        if let Some(m) = &l.track.tempo_map {
                            let tempos: Vec<f64> = m.points.windows(2).map(|w| (60.0 * (w[1].0 - w[0].0) / (w[1].1 - w[0].1) * 100.0).round() / 100.0).collect();
                            let (lo, hi) = tempos.iter().fold((f64::MAX, f64::MIN), |a, t| (a.0.min(*t), a.1.max(*t)));
                            o.insert("dj_tempo_map".into(), serde_json::json!({ "points": m.points.len(), "bpm_from": lo, "bpm_to": hi, "measured": l.detected.tempo_map }));
                        }
                        if let Some((ms, bpm)) = l.detected.grid_fix {
                            o.insert("dj_grid_fix".into(), serde_json::json!({ "moved_ms": (ms * 10.0).round() / 10.0, "bpm_change": bpm }));
                        }
                    }
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default())
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        Some("--headless") => headless(),
        Some("-h") | Some("--help") => print!("{USAGE}"),
        _ => gui::run(),
    }
}

/// `--download-demucs`: fetch HT-Demucs (Mixxx's ONNX conversion) and select it in the settings.
fn download_demucs() -> i32 {
    use std::io::Write;
    let dest = stems::default_demucs_path();
    println!("Downloading HT-Demucs to {}", dest.display());
    let mut last = 0;
    let result = stems::download(stems::DEMUCS_URL, &dest, &mut |done, total| {
        let mb = done / 1_000_000;
        if mb != last {
            last = mb;
            match total {
                Some(t) => print!("\r{mb} of {} MB", t / 1_000_000),
                None => print!("\r{mb} MB"),
            }
            let _ = std::io::stdout().flush();
        }
    });
    println!();
    match result {
        Ok(()) => {
            let mut settings = Settings::load();
            settings.stems_demucs = Some(dest);
            if let Err(e) = settings.save() {
                eprintln!("Downloaded, but saving the settings failed: {e}");
                return 1;
            }
            println!("Done. dj now uses HT-Demucs for stems.");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// `--prepare-stems`: prepare files and folders (recursively) for mixing, as the library's
/// Prepare does: stems (HT-Demucs), sections and mix profiles, skipping tracks that are already
/// prepared. Returns the exit code.
fn prepare_stems(paths: &[String]) -> i32 {
    use std::io::Write;
    let settings = Settings::load();
    let mut caches = (stems::DemucsCache::default(), stems::StructureCache::default());
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let Some(model) = settings.stems_demucs.clone() else {
        eprintln!("No Demucs model. Download it in Settings → Stems, or run: dj --download-demucs");
        return 2;
    };
    let trackdb = settings::TrackDb::load();
    if paths.is_empty() {
        eprintln!("{USAGE}");
        return 2;
    }
    fn collect(p: &std::path::Path, out: &mut Vec<PathBuf>) {
        let hidden = p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.'));
        if hidden {
            return;
        }
        if p.is_dir() {
            if let Ok(rd) = std::fs::read_dir(p) {
                let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
                entries.sort();
                for e in entries {
                    collect(&e, out);
                }
            }
        } else if dj_library::is_audio_file(p) {
            out.push(p.to_path_buf());
        }
    }
    let mut files = Vec::new();
    for p in paths {
        collect(std::path::Path::new(p), &mut files);
    }
    let dir = settings.stems_prepared_dir();
    println!("{} files → {}", files.len(), dir.display());
    let (mut done, mut skipped, mut failed) = (0, 0, 0);
    for (i, f) in files.iter().enumerate() {
        let label = format!("[{}/{}] {}", i + 1, files.len(), f.file_name().unwrap_or_default().to_string_lossy());
        let mut last = -1;
        let mut progress = |p: f32| {
            let pct = (p * 100.0) as i32;
            if pct != last {
                last = pct;
                print!("\r{label}  {pct:3}%");
                let _ = std::io::stdout().flush();
            }
            true
        };
        let overrides = trackdb.tracks.get(&settings::TrackDb::key(f));
        match profile::prepare_track(f, overrides, Some(&model), &dir, threads, &mut caches, &mut progress) {
            Ok(profile::Prepared::Done { sections, .. }) => {
                done += 1;
                println!("\r{label}  done{}", if sections { "" } else { " (no sections)" });
            }
            Ok(profile::Prepared::Already) => {
                skipped += 1;
                println!("\r{label}  already prepared");
            }
            Err(e) => {
                failed += 1;
                println!("\r{label}  FAILED: {e}");
            }
        }
    }
    println!("prepared {done}, already prepared {skipped}, failed {failed}");
    i32::from(failed > 0)
}

/// Import a Syndicate `.sdcmap` into `profile` and save it. If MIDI inputs are restricted to a
/// list, the mapping's devices are added to it so they are actually opened.
pub fn import_mapping_file(path: &std::path::Path, profile: &mut settings::MappingProfile, settings: &mut Settings) -> Result<String, String> {
    let json = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let report = syndicate_map::import(&json, profile)?;
    profile.save().map_err(|e| format!("saving mappings: {e}"))?;
    if !settings.midi_inputs.is_empty() {
        for d in &report.devices {
            if !settings.midi_inputs.contains(d) {
                settings.midi_inputs.push(d.clone());
            }
        }
        settings.save().map_err(|e| format!("saving settings: {e}"))?;
    }
    Ok(report.summary())
}

/// Build the core (audio + engine + session restore).
pub fn boot() -> (audio::AudioHost, Core) {
    let settings = Settings::load();
    let session = Session::load();
    let (host, ctl, sr) = audio::start(&settings.audio).expect("audio");
    let mut core = Core::new(ctl, sr, audio::MAX_BLOCK, settings, session);
    core.audio_channels = host.channels;
    core.log(format!("Audio: {} @ {} Hz, {} channels", host.device_name, host.sample_rate, host.channels));
    core.sync_engine();
    if core.library.is_empty() {
        core.rescan_library();
    }
    core.rescan_vst3();
    (host, core)
}

fn headless() {
    let (_host, mut core) = boot();
    let root = core.settings.osc_root.clone();
    core.log(format!(
        "Headless: OSC on UDP {} (root {root}). Send {root}Deck/1/PlayPause etc. Ctrl+C to quit.",
        core.settings.osc_port
    ));
    loop {
        core.tick();
        std::thread::sleep(Duration::from_millis(10));
    }
}
