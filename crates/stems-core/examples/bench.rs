//! Real-time factor of the engine.
//!
//! Placeholder separator:
//!   cargo run --release -p stems-core --example bench
//! ONNX model (offline mode, so every frame really runs the model):
//!   cargo run --release -p stems-core --features onnx --example bench -- path/to/model.onnx
use std::time::Instant;

use stems_core::{FocusMode, Settings, StemEngine};

fn main() {
    let sr = 44_100.0;
    let seconds = 30;
    let block = 256;
    let model_path = std::env::args().nth(1);

    let (mut engine, stats) = match model_path {
        None => (StemEngine::new(sr), None),
        Some(path) => model_engine(sr, &path),
    };
    engine.set_analysis_enabled(true);

    let mut settings = Settings::default();
    settings.stems[0].isolation = 0.5; // exercise the powf path
    settings.focus.mode = FocusMode::Isolate; // exercise the aux path

    let mut seed = 1u32;
    let mut noise = || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
    };
    let (mut l, mut r) = (vec![0.0f32; block], vec![0.0f32; block]);
    let (mut al, mut ar) = (vec![0.0f32; block], vec![0.0f32; block]);

    let blocks = seconds * sr as usize / block;
    let start = Instant::now();
    for _ in 0..blocks {
        l.iter_mut().for_each(|x| *x = noise());
        r.iter_mut().for_each(|x| *x = noise());
        engine.process(
            &mut [&mut l, &mut r],
            Some(&mut [&mut al, &mut ar]),
            &settings,
        );
    }
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "processed {seconds}s of stereo audio in {elapsed:.3}s -> {:.0}x real time, latency {} samples ({:.0} ms)",
        seconds as f64 / elapsed,
        engine.latency_samples(),
        engine.latency_samples() as f32 * 1000.0 / sr
    );
    if let Some(stats) = stats {
        use std::sync::atomic::Ordering::Relaxed;
        let n = stats.inferences.load(Relaxed);
        println!(
            "model: {n} inferences, last {:.2} ms, max {:.2} ms, budget per hop {:.1} ms",
            stats.last_infer_us.load(Relaxed) as f32 / 1000.0,
            stats.max_infer_us.load(Relaxed) as f32 / 1000.0,
            engine.hop_size() as f32 * 1000.0 / sr
        );
    }
}

#[cfg(any(feature = "onnx", feature = "onnx-dynamic"))]
fn model_engine(
    sr: f32,
    path: &str,
) -> (
    StemEngine,
    Option<std::sync::Arc<stems_core::SeparatorStats>>,
) {
    use stems_core::{MaskModel, OnnxModel, OnnxOptions, ThreadedSeparator};
    let model = OnnxModel::from_file(path, &OnnxOptions::default()).expect("load model");
    println!("model: {} ({:?})", model.name(), model.spec());
    let fft = model.spec().fft_size;
    let sep = ThreadedSeparator::new(model, ThreadedSeparator::DEFAULT_PIPELINE_FRAMES).unwrap();
    let stats = sep.stats();
    let mut engine = StemEngine::with_separator(sr, fft, Box::new(sep));
    engine.set_offline(true);
    (engine, Some(stats))
}

#[cfg(not(any(feature = "onnx", feature = "onnx-dynamic")))]
fn model_engine(
    _sr: f32,
    _path: &str,
) -> (
    StemEngine,
    Option<std::sync::Arc<stems_core::SeparatorStats>>,
) {
    panic!("rebuild with --features onnx (or onnx-dynamic) to benchmark a model");
}
