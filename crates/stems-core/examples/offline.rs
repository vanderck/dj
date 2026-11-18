//! How fast whole tracks separate offline (e.g. on track load in DJ software).
//!
//!   cargo run --release -p stems-core --features onnx --example offline -- model.onnx [seconds]
//!   cargo run --release -p stems-core --example offline            # placeholder separator
use std::time::Instant;

use stems_core::offline;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let model = args.first().filter(|a| a.ends_with(".onnx")).cloned();
    let seconds: f32 = args.iter().find_map(|a| a.parse().ok()).unwrap_or(60.0);
    let rate = 44_100u32;
    let audio: Vec<[f32; 2]> = (0..(seconds * rate as f32) as usize)
        .map(|i| {
            let t = i as f32 / rate as f32;
            let v = 0.3 * (std::f32::consts::TAU * 110.0 * t).sin() + 0.1 * (t * 7919.0).sin();
            [v, -v]
        })
        .collect();

    let start = Instant::now();
    let mut last = -1.0;
    let mut progress = |p: f32| {
        if p - last >= 0.25 || p >= 1.0 {
            eprintln!("  {:3.0}%", p * 100.0);
            last = p;
        }
        true
    };
    let stems = match model {
        Some(path) => run_model(&audio, rate, &path, &mut progress),
        None => offline::separate_with_heuristic(&audio, rate, &mut progress).unwrap(),
    };
    let secs = start.elapsed().as_secs_f32();
    println!(
        "separated {seconds:.0}s of audio into {} stems in {secs:.2}s ({:.1}x real time)",
        stems.len(),
        seconds / secs
    );
}

#[cfg(any(feature = "onnx", feature = "onnx-dynamic"))]
fn run_model(
    audio: &[[f32; 2]],
    rate: u32,
    path: &str,
    progress: offline::Progress,
) -> offline::StemBuffers {
    let model = stems_core::OnnxModel::from_file(path, &Default::default()).expect("load model");
    offline::separate_with_model(audio, rate, model, progress).unwrap()
}

#[cfg(not(any(feature = "onnx", feature = "onnx-dynamic")))]
fn run_model(_: &[[f32; 2]], _: u32, _: &str, _: offline::Progress) -> offline::StemBuffers {
    panic!("rebuild with --features onnx (or onnx-dynamic) to use a model");
}
