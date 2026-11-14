//! Pins down the STFT framing the model contract promises (docs/MODEL_CONTRACT.md,
//! models/tools/stft.py): engine frame t is
//! `rfft(x[(t+1)*hop - N .. (t+1)*hop] * sqrt_hann)` with zeros before the stream start.

use std::sync::{Arc, Mutex};

use stems_core::{Complex32, Separator, Settings, StemEngine, StemMasks};

type Frames = Arc<Mutex<Vec<(Vec<Complex32>, Vec<Complex32>)>>>;

struct Capture(Frames);

impl Separator for Capture {
    fn separate(&mut self, l: &[Complex32], r: &[Complex32], masks: &mut StemMasks) {
        self.0.lock().unwrap().push((l.to_vec(), r.to_vec()));
        for m in &mut masks.masks {
            m.fill(0.25);
        }
    }
}

fn naive_rfft(x: &[f64]) -> Vec<(f64, f64)> {
    let n = x.len();
    (0..=n / 2)
        .map(|k| {
            x.iter().enumerate().fold((0.0, 0.0), |(re, im), (i, &v)| {
                let a = -std::f64::consts::TAU * (k * i) as f64 / n as f64;
                (re + v * a.cos(), im + v * a.sin())
            })
        })
        .collect()
}

#[test]
fn engine_frames_match_the_documented_stft() {
    let n = 64;
    let hop = n / 4;
    let frames: Frames = Default::default();
    let mut engine = StemEngine::with_separator(44_100.0, n, Box::new(Capture(frames.clone())));

    let len = 10 * hop + 5;
    let left: Vec<f32> = (0..len)
        .map(|i| ((i * 37 % 23) as f32 - 11.0) / 11.0)
        .collect();
    let right: Vec<f32> = (0..len).map(|i| (i as f32 * 0.3).sin()).collect();
    let (mut l, mut r) = (left.clone(), right.clone());
    engine.process(&mut [&mut l, &mut r], None, &Settings::default());

    let window: Vec<f64> = (0..n)
        .map(|i| (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos()).sqrt())
        .collect();
    let frames = frames.lock().unwrap();
    assert_eq!(frames.len(), len / hop);

    for (t, (fl, fr)) in frames.iter().enumerate() {
        for (signal, got) in [(&left, fl), (&right, fr)] {
            let seg: Vec<f64> = (0..n)
                .map(|i| {
                    let idx = (t + 1) as isize * hop as isize - n as isize + i as isize;
                    let x = if idx >= 0 {
                        signal[idx as usize] as f64
                    } else {
                        0.0
                    };
                    x * window[i]
                })
                .collect();
            for (k, (re, im)) in naive_rfft(&seg).into_iter().enumerate() {
                assert!((got[k].re as f64 - re).abs() < 1e-3, "frame {t} bin {k} re");
                assert!((got[k].im as f64 - im).abs() < 1e-3, "frame {t} bin {k} im");
            }
        }
    }
}
