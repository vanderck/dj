//! Measure dj's analysis on an annotated set of tracks (not part of the app).
//!
//! `cargo run --release -p dj-library --example analysis_eval -- <audio dir> > out.tsv`
//! prints, per file: its name, dj's detected BPM, the key dj detects (and how sure), the key in
//! its tags (`-` if none), and the bass and treble 36-bin chroma the key comes from.

use std::sync::{Arc, Mutex};

fn main() {
    let dir = std::env::args().nth(1).expect("audio dir");
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).filter(|p| dj_library::is_audio_file(p)).collect();
    files.sort();
    let queue = Arc::new(Mutex::new(files));
    let threads: Vec<_> = (0..std::thread::available_parallelism().map_or(4, |n| n.get()))
        .map(|_| {
            let queue = queue.clone();
            std::thread::spawn(move || loop {
                let Some(p) = queue.lock().unwrap().pop() else { break };
                let Ok(audio) = dj_library::decode(&p) else {
                    eprintln!("can't decode {}", p.display());
                    continue;
                };
                let env = dj_library::analysis::Envelope::compute(&audio);
                let bpm = dj_library::analysis::detect_grid(&env).map_or(0.0, |t| t.grid.bpm);
                let env_f = |k: &str, d: f64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
                let o = dj_library::keydetect::Opts {
                    harmonics: env_f("H", 4.0) as i32,
                    log_weight: env_f("LOGW", 0.0) > 0.0,
                    bass_below: env_f("BASS", 200.0),
                    threshold: env_f("THR", 2.0) as f32,
                };
                let Some((b, t)) = dj_library::keydetect::chroma_split(&audio, &o) else { continue };
                let c: Vec<String> = b.iter().chain(&t).map(|v| format!("{v:.4}")).collect();
                let (key, conf) = dj_library::keydetect::key_from_chroma(&b, &t);
                let tag = dj_library::tags::read(&p).ok().and_then(|t| t.key).map_or("-".to_string(), |k| k.musical());
                println!("{}\t{bpm:.2}\t{}\t{:.3}\t{tag}\t{}", p.file_stem().unwrap().to_string_lossy(), key.musical(), conf, c.join("\t"));
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
}
