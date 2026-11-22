//! Song structure with All-In-One (Kim & Nam, "All-In-One Metrical and Functional Structure
//! Analysis with Neighborhood Attentions on Demixed Audio", 2023): a track's sections (intro,
//! verse, chorus, bridge, break, instrumental, solo, outro) and beats, from its four stems.
//!
//! The model is the published "harmonix-all" (eight trained copies, averaged) exported to ONNX by
//! dj, with NATTEN's neighbourhood attention rewritten in plain operations. Its input is madmom's
//! log-filtered spectrogram of each stem, computed here as All-In-One does: 2048-sample frames
//! centred every 441 samples (100 a second at 44.1 kHz), a Hann window, 12-per-octave triangular
//! filters from 30 Hz to 17 kHz (normalised), log10(1 + x).

use realfft::RealFftPlanner;

/// Section labels, in the model's order.
pub const LABELS: [&str; 10] = [
    "start", "end", "intro", "outro", "break", "bridge", "inst", "solo", "verse", "chorus",
];

/// The label as one of [`LABELS`] (for sections read back from a file).
pub fn label(name: &str) -> Option<&'static str> {
    LABELS.iter().copied().find(|l| *l == name)
}

/// Sample rate, frame size and hop of the model's input.
pub const SAMPLE_RATE: u32 = 44_100;
const FRAME: usize = 2048;
const HOP: usize = 441;
/// Frames per second.
pub const FPS: f64 = 100.0;
/// The model takes at least this many frames (its widest attention); shorter tracks are padded
/// with silence.
pub const MIN_FRAMES: usize = 10_240;

/// One section of a track, in seconds.
#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    pub start: f64,
    pub end: f64,
    pub label: &'static str,
}

/// A beat the model hears: when (seconds) and how surely it starts a bar (0..1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Beat {
    pub secs: f64,
    pub downbeat: f32,
}

/// What the model finds in a track.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Structure {
    pub sections: Vec<Section>,
    /// The beats, picked from the activations (see [`beats`]).
    pub beats: Vec<Beat>,
    /// Beat and downbeat activations (0..1), 100 per second.
    pub beat: Vec<f32>,
    pub downbeat: Vec<f32>,
}

/// The 81 triangular filters (rows: the 1024 FFT bins, columns: bands), as madmom's
/// LogarithmicFilterbank(num_bands=12, fmin=30, fmax=17000, norm_filters=True, unique_filters=True).
pub fn filterbank() -> Vec<Vec<(usize, f32)>> {
    let bins = FRAME / 2;
    let bin_freq = |k: usize| k as f64 * SAMPLE_RATE as f64 / FRAME as f64;
    let (left, right) = (
        ((30.0f64 / 440.0).log2() * 12.0).floor(),
        ((17_000.0f64 / 440.0).log2() * 12.0).ceil(),
    );
    let freqs: Vec<f64> = (left as i64..right as i64)
        .map(|i| 440.0 * 2f64.powf(i as f64 / 12.0))
        .filter(|f| *f >= 30.0 && *f <= 17_000.0)
        .collect();
    // The FFT bin nearest each frequency (ties to the lower one), unique.
    let mut idx: Vec<usize> = freqs
        .iter()
        .map(|&f| {
            let i = (0..bins)
                .position(|k| bin_freq(k) >= f)
                .unwrap_or(bins - 1)
                .clamp(1, bins - 1);
            let (l, r) = (bin_freq(i - 1), bin_freq(i));
            if f - l < r - f {
                i - 1
            } else {
                i
            }
        })
        .collect();
    idx.dedup();
    idx.windows(3)
        .map(|w| {
            let (start, mut center, mut stop) = (w[0], w[1], w[2]);
            if stop - start < 2 {
                center = start;
                stop = start + 1;
            }
            let (c, s) = (center - start, stop - start);
            let mut data: Vec<f64> = (0..c).map(|i| i as f64 / c as f64).collect();
            data.extend((0..s - c).map(|i| 1.0 - i as f64 / (s - c) as f64));
            let sum: f64 = data.iter().sum();
            data.iter()
                .enumerate()
                .map(|(i, v)| (start + i, (v / sum) as f32))
                .collect()
        })
        .collect()
}

/// The log-filtered spectrogram of a mono signal at 44.1 kHz: one row of 81 bands per frame.
pub fn spectrogram(mono: &[f32], fb: &[Vec<(usize, f32)>]) -> Vec<Vec<f32>> {
    let n = mono.len().div_ceil(HOP);
    // Hann window, symmetric (numpy's hanning).
    let window: Vec<f32> = (0..FRAME)
        .map(|i| (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / (FRAME - 1) as f64).cos()) as f32)
        .collect();
    let fft = RealFftPlanner::<f32>::new().plan_fft_forward(FRAME);
    let mut buf = fft.make_input_vec();
    let mut spec = fft.make_output_vec();
    let mut mag = vec![0.0f32; FRAME / 2];
    (0..n)
        .map(|t| {
            // Frame t centred on sample t·hop, silence outside the signal.
            let start = (t * HOP) as i64 - (FRAME / 2) as i64;
            for (i, b) in buf.iter_mut().enumerate() {
                let j = start + i as i64;
                *b = if j >= 0 && (j as usize) < mono.len() {
                    mono[j as usize] * window[i]
                } else {
                    0.0
                };
            }
            let _ = fft.process(&mut buf, &mut spec);
            for (m, c) in mag.iter_mut().zip(&spec) {
                *m = c.norm();
            }
            fb.iter()
                .map(|f| (f.iter().map(|(k, w)| mag[*k] * w).sum::<f32>() + 1.0).log10())
                .collect()
        })
        .collect()
}

/// Sections from the model's outputs (allin1's post-processing): boundaries where the section
/// activation peaks (the largest within ±0.48 s, then above its mean over the 12 s before and
/// after), each section labelled with its most likely function on average.
pub fn sections(section_logits: &[f32], function_logits: &[Vec<f32>]) -> Vec<Section> {
    let n = section_logits.len();
    if n == 0 {
        return Vec::new();
    }
    let prob: Vec<f32> = section_logits
        .iter()
        .map(|x| 1.0 / (1.0 + (-x).exp()))
        .collect();
    // Local maxima within ±48 frames (4 × 24 hops per beat + 1).
    let local = |v: &[f32], half: usize| -> Vec<f32> {
        (0..v.len())
            .map(|i| {
                if v[i]
                    >= v[i.saturating_sub(half)..(i + half + 1).min(v.len())]
                        .iter()
                        .copied()
                        .fold(f32::MIN, f32::max)
                {
                    v[i]
                } else {
                    0.0
                }
            })
            .collect()
    };
    let peaks = local(&prob, 48);
    // Peak picking against the mean of the 12 s before and after.
    let w = (12.0 * FPS) as usize;
    let at = |i: i64| {
        if i >= 0 && (i as usize) < n {
            peaks[i as usize]
        } else {
            0.0
        }
    };
    let mut bounds: Vec<usize> = (1..n)
        .filter(|&i| {
            let v = peaks[i];
            if v <= 0.0 {
                return false;
            }
            let max = (i as i64 - w as i64..=i as i64 + w as i64)
                .map(at)
                .fold(f32::MIN, f32::max);
            let past = (i as i64 - w as i64..i as i64).map(at).sum::<f32>() / w as f32;
            let future = (i as i64 + 1..=i as i64 + w as i64).map(at).sum::<f32>() / w as f32;
            v == max && v - (past + future) / 2.0 > 0.0
        })
        .collect();
    bounds.insert(0, 0);
    bounds.push(n);
    bounds
        .windows(2)
        .map(|b| {
            // The label whose softmax probability is highest on average over the section.
            let mut mean = [0.0f64; 10];
            for t in b[0]..b[1] {
                let m = function_logits
                    .iter()
                    .map(|f| f[t])
                    .fold(f32::MIN, f32::max);
                let e: Vec<f64> = function_logits
                    .iter()
                    .map(|f| ((f[t] - m) as f64).exp())
                    .collect();
                let sum: f64 = e.iter().sum();
                for (k, v) in e.iter().enumerate() {
                    mean[k] += v / sum;
                }
            }
            let best = (0..10)
                .max_by(|a, z| mean[*a].total_cmp(&mean[*z]))
                .unwrap_or(0);
            Section {
                start: b[0] as f64 / FPS,
                end: b[1] as f64 / FPS,
                label: LABELS[best],
            }
        })
        .collect()
}

/// The model's four inputs, in its order.
pub const PARTS: [&str; 4] = ["bass", "drums", "other", "vocals"];

/// A stem (stereo, at `rate`) as the model hears it: mono at 44.1 kHz.
pub fn mono_44k(frames: impl Iterator<Item = [f32; 2]>, rate: u32) -> Vec<f32> {
    if rate == SAMPLE_RATE {
        frames.map(|f| 0.5 * (f[0] + f[1])).collect()
    } else {
        let stereo: Vec<[f32; 2]> = frames.collect();
        crate::resample(&stereo, rate, SAMPLE_RATE)
            .iter()
            .map(|f| 0.5 * (f[0] + f[1]))
            .collect()
    }
}

/// Beats from the activations (100 a second): peaks above 0.3 that are the highest within
/// ±80 ms, timed between frames by the peak's shape; each with the strongest downbeat activation
/// within ±20 ms.
pub fn beats(beat: &[f32], downbeat: &[f32]) -> Vec<Beat> {
    let n = beat.len();
    let half = 8;
    (1..n.saturating_sub(1))
        .filter(|&i| {
            let v = beat[i];
            v >= 0.3
                && beat[i.saturating_sub(half)..(i + half + 1).min(n)]
                    .iter()
                    .all(|&w| w <= v)
                && beat[i - 1] < v
        })
        .map(|i| {
            let (a, b, c) = (beat[i - 1] as f64, beat[i] as f64, beat[i + 1] as f64);
            let d = a - 2.0 * b + c;
            let off = if d < 0.0 {
                (0.5 * (a - c) / d).clamp(-0.5, 0.5)
            } else {
                0.0
            };
            let db = downbeat[i.saturating_sub(2)..(i + 3).min(downbeat.len())]
                .iter()
                .copied()
                .fold(0.0, f32::max);
            Beat {
                secs: (i as f64 + off) / FPS,
                downbeat: db,
            }
        })
        .collect()
}

#[cfg(any(feature = "onnx", feature = "onnx-dynamic"))]
pub use model::StructureModel;

#[cfg(any(feature = "onnx", feature = "onnx-dynamic"))]
mod model {
    use super::*;
    use crate::offline::ModelError;
    use crate::onnx::{builder, err, OnnxOptions};
    use ort::session::Session;
    use ort::value::Tensor;
    use std::sync::Mutex;

    /// The model, built into dj (11 MB).
    static MODEL: &[u8] = include_bytes!("../models/allinone.onnx");

    /// The All-In-One model, loaded.
    pub struct StructureModel {
        session: Mutex<Session>,
    }

    impl StructureModel {
        pub fn new(options: &OnnxOptions) -> Result<Self, ModelError> {
            let session = builder(options)?.commit_from_memory(MODEL).map_err(err)?;
            Ok(Self {
                session: Mutex::new(session),
            })
        }

        /// Sections and beats of a track. `part(k)`: the track's stem [`PARTS`]`[k]`, mono at
        /// 44.1 kHz (see [`mono_44k`]); asked for one at a time, so only one is in memory.
        pub fn analyze(
            &self,
            mut part: impl FnMut(usize) -> Vec<f32>,
        ) -> Result<Structure, ModelError> {
            let fb = filterbank();
            let bands = fb.len();
            let specs: Vec<Vec<Vec<f32>>> = (0..4).map(|k| spectrogram(&part(k), &fb)).collect();
            let frames = specs.iter().map(Vec::len).max().unwrap_or(0);
            if frames == 0 {
                return Ok(Structure::default());
            }
            let t = frames.max(MIN_FRAMES);
            let mut input = vec![0.0f32; 4 * t * bands];
            for (k, spec) in specs.iter().enumerate() {
                for (f, row) in spec.iter().enumerate() {
                    input[(k * t + f) * bands..(k * t + f + 1) * bands].copy_from_slice(row);
                }
            }
            drop(specs);
            let tensor = Tensor::from_array(([1usize, 4, t, bands], input)).map_err(err)?;
            let mut session = self.session.lock().unwrap_or_else(|p| p.into_inner());
            let out = session.run(ort::inputs!["spec" => tensor]).map_err(err)?;
            let flat = |name: &str| -> Result<Vec<f32>, ModelError> {
                let (_, v) = out[name].try_extract_tensor::<f32>().map_err(err)?;
                Ok(v.to_vec())
            };
            let sig = |v: Vec<f32>| {
                v.into_iter()
                    .take(frames)
                    .map(|x| 1.0 / (1.0 + (-x).exp()))
                    .collect::<Vec<f32>>()
            };
            let section = flat("section")?;
            let function = flat("function")?; // (1, 10, t)
            let functions: Vec<Vec<f32>> = (0..LABELS.len())
                .map(|k| function[k * t..k * t + frames].to_vec())
                .collect();
            let (beat, downbeat) = (sig(flat("beat")?), sig(flat("downbeat")?));
            Ok(Structure {
                sections: sections(&section[..frames], &functions),
                beats: beats(&beat, &downbeat),
                beat,
                downbeat,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_filterbank_is_madmoms() {
        let fb = filterbank();
        assert_eq!(fb.len(), 81, "All-In-One's 81 bands");
        for f in &fb {
            let sum: f32 = f.iter().map(|(_, w)| w).sum();
            assert!((sum - 1.0).abs() < 1e-4, "normalised");
        }
    }

    #[test]
    fn beats_are_the_activation_peaks() {
        // Beats every 50 frames (0.5 s), peaking a third of a frame late; every 4th a downbeat.
        let n = 1000;
        let bump =
            |i: usize, k: usize| (-(((i as f32) - (50 * k) as f32 - 0.33) / 2.0).powi(2)).exp();
        let beat: Vec<f32> = (0..n)
            .map(|i| (1..20).map(|k| bump(i, k)).sum::<f32>() * 0.9)
            .collect();
        let down: Vec<f32> = (0..n)
            .map(|i| {
                (1..20)
                    .filter(|k| k % 4 == 0)
                    .map(|k| bump(i, k))
                    .sum::<f32>()
                    * 0.8
            })
            .collect();
        let b = beats(&beat, &down);
        assert_eq!(b.len(), 19);
        for (k, x) in b.iter().enumerate() {
            assert!(
                (x.secs - (k + 1) as f64 * 0.5 - 0.0033).abs() < 0.001,
                "{x:?}"
            );
            assert_eq!(x.downbeat > 0.5, (k + 1) % 4 == 0, "{x:?}");
        }
    }

    #[test]
    fn sections_split_where_the_activation_peaks() {
        // 60 s; boundaries at 20 s and 40 s; intro, chorus, outro.
        let n = 6000;
        // (A gently falling baseline, bumps at the boundaries: real activations are never flat.)
        let bump = |i: usize, at: f32| 10.0 * (-((i as f32 - at) / 30.0).powi(2)).exp();
        let sec: Vec<f32> = (0..n)
            .map(|i| -6.0 - i as f32 * 1e-4 + bump(i, 2000.0) + bump(i, 4000.0))
            .collect();
        let fun: Vec<Vec<f32>> = (0..10)
            .map(|k| {
                (0..n)
                    .map(|i| match (k, i / 2000) {
                        (2, 0) | (9, 1) | (3, 2) => 5.0,
                        _ => 0.0,
                    })
                    .collect()
            })
            .collect();
        let s = sections(&sec, &fun);
        let got: Vec<(f64, &str)> = s.iter().map(|x| (x.start, x.label)).collect();
        assert_eq!(got, vec![(0.0, "intro"), (20.0, "chorus"), (40.0, "outro")]);
        assert_eq!(s.last().unwrap().end, 60.0);
    }
}

#[cfg(test)]
mod check {
    /// Writes the spectrogram of a raw f32 mono file (`SPEC_IN`) to `SPEC_OUT`, to compare with
    /// madmom's.
    #[test]
    #[ignore]
    fn dump_spectrogram() {
        let (Ok(i), Ok(o)) = (std::env::var("SPEC_IN"), std::env::var("SPEC_OUT")) else {
            return;
        };
        let raw = std::fs::read(i).unwrap();
        let mono: Vec<f32> = raw
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        let spec = super::spectrogram(&mono, &super::filterbank());
        let out: Vec<u8> = spec
            .iter()
            .flatten()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        std::fs::write(o, out).unwrap();
    }
}
