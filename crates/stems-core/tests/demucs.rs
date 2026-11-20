//! Demucs runner against a tiny stand-in model (tests/data/demucs_test*.onnx): each
//! source is a fixed fraction of the input at the same sample, so chunking, padding, trimming,
//! blending and normalisation errors all show up. Run with
//! `cargo test -p stems-core --features onnx` (or `onnx-dynamic` and `ORT_DYLIB_PATH`).
#![cfg(any(feature = "onnx", feature = "onnx-dynamic"))]

use stems_core::demucs::DemucsModel;
use stems_core::{OnnxOptions, Stem};

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data");
// drums, bass, other, vocals in the model; our order is drums, bass, vocals, other.
const GAIN: [f32; 4] = [0.1, 0.2, 0.4, 0.3];

fn signal(n: usize) -> Vec<[f32; 2]> {
    (0..n)
        .map(|i| {
            let t = i as f32;
            [
                0.3 * (t * 0.031).sin() + 0.05,
                0.2 * (t * 0.017).cos() - 0.02 + 0.1 * (t * 0.5).sin(),
            ]
        })
        .collect()
}

fn check(model: &mut DemucsModel, n: usize) {
    let x = signal(n);
    let mut calls = Vec::new();
    let stems = model
        .separate(&x, 44_100, &mut |p| {
            calls.push(p);
            true
        })
        .unwrap();
    // Demucs normalises by the mono mean m and un-normalises every source with it, so each
    // source is gain * (x - m) + m.
    let m = x
        .iter()
        .map(|f| 0.5 * (f[0] as f64 + f[1] as f64))
        .sum::<f64>() as f32
        / n as f32;
    for s in Stem::ALL {
        assert_eq!(stems[s as usize].len(), n);
        for (i, (y, x)) in stems[s as usize].iter().zip(&x).enumerate() {
            for c in 0..2 {
                let want = GAIN[s as usize] * (x[c] - m) + m;
                assert!(
                    (y[c] - want).abs() < 1e-5,
                    "{} sample {i} ch {c}: {} vs {want}",
                    s.name(),
                    y[c]
                );
            }
        }
    }
    assert_eq!(calls.last(), Some(&1.0));
}

#[test]
fn single_model_matches_demucs_chunking_exactly() {
    let mut model = DemucsModel::from_files(
        &[format!("{DIR}/demucs_test.onnx")],
        &OnnxOptions::default(),
    )
    .unwrap();
    assert_eq!((model.segment(), model.name()), (1000, "HT-Demucs"));
    for n in [3456, 1000, 1750, 999, 10] {
        check(&mut model, n);
    }
    assert!(model
        .separate(&[], 44_100, &mut |_| true)
        .unwrap()
        .iter()
        .all(|s| s.is_empty()));
}

#[test]
fn bag_of_models_takes_one_stem_from_each() {
    let files: Vec<String> = ["drums", "bass", "other", "vocals"]
        .iter()
        .map(|s| format!("{DIR}/demucs_test.{s}.onnx"))
        .collect();
    let mut model = DemucsModel::from_files(&files, &OnnxOptions::default()).unwrap();
    assert_eq!(model.name(), "HT-Demucs FT");
    check(&mut model, 2500);
    // A stem covered twice (or not at all) is refused.
    let twice = [
        format!("{DIR}/demucs_test.onnx"),
        format!("{DIR}/demucs_test.vocals.onnx"),
    ];
    let e = DemucsModel::from_files(&twice, &OnnxOptions::default())
        .err()
        .unwrap();
    assert!(e.to_string().contains("Vocals stem 2 times"), "{e}");
    let e = DemucsModel::from_files(&files[..3], &OnnxOptions::default())
        .err()
        .unwrap();
    assert!(e.to_string().contains("Vocals stem 0 times"), "{e}");
}

#[test]
fn other_sample_rates_and_cancelling() {
    let mut model = DemucsModel::from_files(
        &[format!("{DIR}/demucs_test.onnx")],
        &OnnxOptions::default(),
    )
    .unwrap();
    let x: Vec<[f32; 2]> = (0..4800)
        .map(|i| {
            let v = 0.3 * (i as f32 * 0.05).sin();
            [v, -v]
        })
        .collect();
    let stems = model.separate(&x, 48_000, &mut |_| true).unwrap();
    assert_eq!(stems[Stem::Vocals as usize].len(), 4800);
    let err: f32 = stems[Stem::Vocals as usize][300..4500]
        .iter()
        .zip(&x[300..4500])
        .map(|(y, x)| (y[0] - 0.4 * x[0]).abs())
        .fold(0.0, f32::max);
    assert!(err < 2e-3, "{err}");
    let mut calls = 0;
    let r = model.separate(&x, 44_100, &mut |_| {
        calls += 1;
        calls < 2
    });
    assert!(matches!(
        r,
        Err(stems_core::offline::OfflineError::Cancelled)
    ));
}
