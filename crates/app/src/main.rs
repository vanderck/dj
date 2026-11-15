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
    collective --vst3-scan B   (internal) scan one VST3 bundle and print JSON
";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("--vst3-scan") => {
            let code = args.get(2).map(|b| fxcat::scan_child_main(&PathBuf::from(b))).unwrap_or(2);
            std::process::exit(code);
        }
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

/// Build the core (audio + engine + session restore).
pub fn boot() -> (audio::AudioHost, Core) {
    let settings = Settings::load();
    let session = Session::load();
    let (host, ctl, sr) = audio::start(&settings.audio).expect("audio");
    let mut core = Core::new(ctl, sr, audio::MAX_BLOCK, settings, session);
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
