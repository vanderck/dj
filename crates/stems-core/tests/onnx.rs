//! End-to-end tests of the ONNX backend against a small hand-made test model
//! (tests/data/test_model.onnx). Run with:
//!   ORT_DYLIB_PATH=/path/to/libonnxruntime.so cargo test -p stems-core --features onnx-dynamic
//! or `--features onnx` where ONNX Runtime can be downloaded at build time.
#![cfg(any(feature = "onnx", feature = "onnx-dynamic"))]

use stems_core::{offline, MaskModel, ModelSpec, OnnxModel, OnnxOptions, Stem};

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

fn sine(hz: f32, secs: f32) -> Vec<[f32; 2]> {
    (0..(SR * secs) as usize)
        .map(|i| {
            let v = 0.5 * (std::f32::consts::TAU * hz * i as f32 / SR).sin();
            [v, -0.5 * v]
        })
        .collect()
}

#[test]
fn whole_track_separation_sums_back_to_the_input() {
    let x = sine(330.0, 1.0);
    let stems = offline::separate_with_model(&x, SR as u32, load(), &mut |_| true).unwrap();
    for (i, f) in x.iter().enumerate().skip(100) {
        for c in 0..2 {
            let sum: f32 = Stem::ALL.iter().map(|s| stems[*s as usize][i][c]).sum();
            assert!((sum - f[c]).abs() < 1e-4, "sample {i}");
        }
    }
    // The test model is not neutral: the stems really differ.
    let energy = |s: Stem| stems[s as usize].iter().map(|f| f[0] * f[0]).sum::<f32>();
    assert!(Stem::ALL
        .iter()
        .any(|&s| (energy(s) - energy(Stem::Drums)).abs() > 1e-3));
}
