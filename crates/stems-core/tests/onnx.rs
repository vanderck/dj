//! End-to-end tests of the ONNX backend against the generated test model
//! (models/tools/make_test_model.py). Run with:
//!   ORT_DYLIB_PATH=/path/to/libonnxruntime.so cargo test -p stems-core --features onnx-dynamic
//! or `--features onnx` where ONNX Runtime can be downloaded at build time.
#![cfg(any(feature = "onnx", feature = "onnx-dynamic"))]

use stems_core::{
    MaskModel, ModelSpec, OnnxModel, OnnxOptions, Settings, Stem, StemEngine, ThreadedSeparator,
};

const MODEL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/test_model.onnx");
const SR: f32 = 44_100.0;

fn load() -> OnnxModel {
    OnnxModel::from_file(MODEL, &OnnxOptions::default()).expect("load test model")
}

#[test]
fn reads_spec_from_metadata() {
    let model = load();
    assert_eq!(
        model.spec(),
        ModelSpec {
            sample_rate: 44_100.0,
            fft_size: 4096,
            context_frames: 8,
            lookahead_frames: 2
        }
    );
    assert!(model.name().contains("test model"));
}

#[test]
fn from_memory_matches_from_file() {
    let bytes = std::fs::read(MODEL).unwrap();
    let mut a = load();
    let mut b = OnnxModel::from_memory(&bytes, &OnnxOptions::default()).unwrap();
    let spec = a.spec();
    let ctx: Vec<f32> = (0..spec.input_len())
        .map(|i| ((i % 13) as f32 - 6.0) * 0.1)
        .collect();
    let (mut ma, mut mb) = (vec![0.0; spec.output_len()], vec![0.0; spec.output_len()]);
    a.infer(&ctx, &mut ma).unwrap();
    b.infer(&ctx, &mut mb).unwrap();
    assert_eq!(ma, mb);
}

#[test]
fn rejects_files_that_are_not_models() {
    let path = std::env::temp_dir().join("stems-not-a-model.onnx");
    std::fs::write(&path, b"definitely not protobuf").unwrap();
    assert!(OnnxModel::from_file(&path, &OnnxOptions::default()).is_err());
}

fn engine() -> StemEngine {
    let model = load();
    let fft = model.spec().fft_size;
    let sep = ThreadedSeparator::new(model, ThreadedSeparator::DEFAULT_PIPELINE_FRAMES).unwrap();
    let mut e = StemEngine::with_separator(SR, fft, Box::new(sep));
    e.set_offline(true);
    e
}

fn sine(hz: f32, secs: f32) -> Vec<f32> {
    (0..(SR * secs) as usize)
        .map(|i| 0.5 * (std::f32::consts::TAU * hz * i as f32 / SR).sin())
        .collect()
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

#[test]
fn full_engine_is_transparent_at_unity() {
    let mut e = engine();
    let lat = e.latency_samples();
    assert_eq!(lat, 4096 + (2 + 2) * 1024);
    let x: Vec<f32> = sine(330.0, 1.0)
        .iter()
        .zip(sine(2_500.0, 1.0))
        .map(|(a, b)| a + b)
        .collect();
    let (mut l, mut r) = (x.clone(), x.clone());
    e.process(&mut [&mut l, &mut r], None, &Settings::default());
    let err = l[lat..]
        .iter()
        .zip(&x)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f32::max);
    assert!(err < 1e-3, "err {err}");
}

#[test]
fn soloing_bass_keeps_lows_only() {
    let mut settings = Settings::default();
    settings.stems[Stem::Bass as usize].solo = true;
    for (hz, keep) in [(55.0, true), (1_000.0, false)] {
        let mut e = engine();
        let lat = e.latency_samples();
        let x = sine(hz, 1.0);
        let (mut l, mut r) = (x.clone(), x.clone());
        e.process(&mut [&mut l, &mut r], None, &settings);
        let ratio = rms(&l[2 * lat..]) / rms(&x[2 * lat..]);
        assert_eq!(ratio > 0.7, keep, "{hz} Hz: ratio {ratio}");
    }
}

#[test]
fn realtime_paced_processing_has_no_underruns() {
    use std::sync::atomic::Ordering::Relaxed;
    use std::time::{Duration, Instant};

    let model = load();
    let sep = ThreadedSeparator::new(model, ThreadedSeparator::DEFAULT_PIPELINE_FRAMES).unwrap();
    let stats = sep.stats();
    let mut e = StemEngine::with_separator(SR, 4096, Box::new(sep)); // real-time mode

    let block = 512;
    let x = sine(220.0, 2.0);
    let start = Instant::now();
    for (i, chunk) in x.chunks(block).enumerate() {
        let (mut l, mut r) = (chunk.to_vec(), chunk.to_vec());
        e.process(&mut [&mut l, &mut r], None, &Settings::default());
        // Sleep until this block's real-time deadline, like an audio callback.
        let due = Duration::from_secs_f32((i + 1) as f32 * block as f32 / SR);
        if let Some(wait) = due.checked_sub(start.elapsed()) {
            std::thread::sleep(wait);
        }
    }
    assert!(stats.inferences.load(Relaxed) > 50);
    assert_eq!(stats.underruns.load(Relaxed), 0, "worker missed deadlines");
    assert!(!stats.failed.load(Relaxed));
}

#[test]
fn offline_separation_with_the_model() {
    use stems_core::offline::{separate_with_model, OfflineError};
    for rate in [44_100u32, 48_000] {
        let x: Vec<[f32; 2]> = (0..rate as usize)
            .map(|i| {
                let t = i as f32 / rate as f32;
                let v = 0.4 * (std::f32::consts::TAU * 55.0 * t).sin()
                    + 0.2 * (std::f32::consts::TAU * 1_200.0 * t).sin();
                [v, v]
            })
            .collect();
        let stems = separate_with_model(&x, rate, load(), &mut |_| true).unwrap();
        assert!(stems.iter().all(|s| s.len() == x.len()));
        let energy = |v: &[[f32; 2]]| v.iter().map(|f| f[0] * f[0]).sum::<f32>();
        let (bass, total) = (energy(&stems[Stem::Bass as usize]), energy(&x));
        assert!(
            bass > 0.5 * total * 0.8,
            "{rate} Hz: bass {bass} of {total}"
        );
    }
    let cancelled = separate_with_model(&vec![[0.0; 2]; 100_000], 44_100, load(), &mut |_| false);
    assert_eq!(cancelled.unwrap_err(), OfflineError::Cancelled);
}
