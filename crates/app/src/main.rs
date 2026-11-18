//! Collective — live performance DJ software.

mod audio;
mod core;
mod fxcat;
mod gui;
mod midi;
mod model;
mod osc;
mod settings;
mod stems;
mod syndicate_map;

use std::path::PathBuf;
use std::time::Duration;

use crate::core::Core;
use crate::model::Session;
use crate::settings::Settings;

const USAGE: &str = "\
Collective — live performance DJ software

USAGE:
    collective                 start the GUI
    collective --headless      run the engine with OSC/MIDI control only
    collective --info FILE     print tags (BPM, key, energy, cues) for a file
    collective --list-devices  list audio and MIDI devices
    collective --import-mapping FILE.sdcmap
                               import a Syndicate MIDI mapping into Collective's mappings
    collective --prepare-stems PATH...
                               separate files / folders with the external separator
                               (Settings → Stems) into the prepared-stems folder
    collective --vst3-scan B   (internal) scan one VST3 bundle and print JSON
";

fn main() {
    let args: Vec<String> = std::env::args().collect();
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
            match collective_library::tags::read(&PathBuf::from(p)) {
                Ok(t) => println!("{}", serde_json::to_string_pretty(&t).unwrap_or_default()),
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

/// `--prepare-stems`: run the external separator over files and folders (recursively), skipping
/// tracks that are already prepared. Returns the exit code.
fn prepare_stems(paths: &[String]) -> i32 {
    use std::io::Write;
    let settings = Settings::load();
    let Some(command) = settings.stems_external() else {
        eprintln!("No external separator set. Add one in Settings → Stems → External separator.");
        return 2;
    };
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
        } else if collective_library::is_audio_file(p) {
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
        };
        match stems::prepare_file(f, &command, &dir, &mut progress) {
            Ok(stems::Prepared::Done(_)) => {
                done += 1;
                println!("\r{label}  done");
            }
            Ok(stems::Prepared::AlreadyPrepared) => {
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
