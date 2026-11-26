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
    check_within(model, n, 1e-5);
}

fn check_within(model: &mut DemucsModel, n: usize, tol: f32) {
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
                    (y[c] - want).abs() < tol,
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
fn the_gpu_option_runs_where_it_can_and_says_where() {
    use stems_core::Device;
    let options = OnnxOptions {
        gpu: true,
        ..OnnxOptions::default()
    };
    let mut model =
        DemucsModel::from_files(&[format!("{DIR}/demucs_test.onnx")], &options).unwrap();
    // On an accelerator (the Neural Engine with CoreML, the GPU with DirectML), or on the CPU
    // with the reason it isn't (none on this system, or the model doesn't run there).
    match model.device() {
        Device::Accelerator(name) => {
            assert!(["Neural Engine (CoreML)", "GPU (DirectML)"].contains(name))
        }
        Device::Cpu(why) => assert!(
            why.as_deref().is_some_and(|w| !w.is_empty()),
            "{:?}",
            model.device()
        ),
    }
    let shown = model.device().to_string();
    assert!(
        shown == "Neural Engine (CoreML)"
            || shown == "GPU (DirectML)"
            || shown.starts_with("CPU (no acceleration: "),
        "{shown}"
    );
    // The same stems either way (the Neural Engine computes in half precision).
    for n in [3456, 999] {
        check_within(&mut model, n, 2e-3);
    }
    // Without it: the CPU, nothing to explain.
    let cpu = DemucsModel::from_files(
        &[format!("{DIR}/demucs_test.onnx")],
        &OnnxOptions::default(),
    )
    .unwrap();
    assert_eq!(cpu.device(), &Device::Cpu(None));
    assert_eq!(cpu.device().to_string(), "CPU");
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

/// The real HT-Demucs in 16 bits (`DEMUCS_MODEL`: htdemucs.onnx; `DEMUCS_MIX`: interleaved
/// 32-bit float stereo at 44.1 kHz) separates as the 32-bit model does: each stem within
/// 30 dB. `cargo test -p stems-core --features onnx -- --ignored half`
#[test]
#[ignore]
fn half_precision_htdemucs_separates_as_the_full_model() {
    let (Ok(model), Ok(mix)) = (std::env::var("DEMUCS_MODEL"), std::env::var("DEMUCS_MIX")) else {
        panic!("set DEMUCS_MODEL and DEMUCS_MIX");
    };
    let bytes = std::fs::read(mix).unwrap();
    let mix: Vec<[f32; 2]> = bytes
        .chunks_exact(8)
        .map(|c| {
            [
                f32::from_le_bytes(c[0..4].try_into().unwrap()),
                f32::from_le_bytes(c[4..8].try_into().unwrap()),
            ]
        })
        .collect();
    let dir = std::env::temp_dir().join("dj-half-test");
    let options = OnnxOptions {
        intra_threads: 4,
        gpu_cache: Some(dir),
        ..OnnxOptions::default()
    };
    let started = std::time::Instant::now();
    let half = stems_core::half_model(&options, std::path::Path::new(&model)).unwrap();
    eprintln!(
        "16-bit model in {:.1} s: {}",
        started.elapsed().as_secs_f64(),
        half.display()
    );
    let run = |path: &std::path::Path| {
        let mut m = DemucsModel::from_files(&[path], &options).unwrap();
        let t = std::time::Instant::now();
        let s = m.separate(&mix, 44_100, &mut |_| true).unwrap();
        eprintln!("{}: {:.1} s", path.display(), t.elapsed().as_secs_f64());
        s
    };
    let full = run(std::path::Path::new(&model));
    let halved = run(&half);
    for (i, (a, b)) in full.iter().zip(halved.iter()).enumerate() {
        let (mut sig, mut noise) = (0.0f64, 0.0f64);
        for (x, y) in a.iter().zip(b.iter()) {
            for c in 0..2 {
                sig += (x[c] as f64).powi(2);
                noise += (x[c] as f64 - y[c] as f64).powi(2);
            }
        }
        let snr = 10.0 * (sig / noise.max(1e-20)).log10();
        eprintln!("stem {i}: {snr:.1} dB");
        assert!(snr > 30.0, "stem {i}: {snr:.1} dB");
    }
}

#[test]
fn a_16_bit_model_separates_as_the_full_one() {
    // The stand-in's "network" (its /encoder.0 node) in 16 bits, the input reshape in 32.
    let dir = std::env::temp_dir().join(format!("dj-half-{}", std::process::id()));
    let options = OnnxOptions {
        gpu_cache: Some(dir.clone()),
        ..OnnxOptions::default()
    };
    let src = std::path::PathBuf::from(format!("{DIR}/demucs_test.onnx"));
    let half = stems_core::half_model(&options, &src).unwrap();
    assert!(half.starts_with(&dir) && half.to_string_lossy().ends_with(".fp16.onnx"));
    assert_eq!(
        stems_core::half_model(&options, &src).unwrap(),
        half,
        "made once"
    );
    let mut model = DemucsModel::from_files(&[&half], &OnnxOptions::default()).unwrap();
    for n in [3456, 999] {
        check_within(&mut model, n, 2e-3);
    }
    let _ = std::fs::remove_dir_all(dir);
}
