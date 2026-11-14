//! Offline separation of a whole track into stems (for DJ software, batch tools, ...).
//!
//! Uses exactly the same STFT framing, separator protocol and lookahead handling as
//! [`crate::StemEngine`] (a test checks that the two agree), but synthesises every stem separately
//! and removes the latency, so stem sample `n` lines up with input sample `n`.

use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::sync::Arc;

use crate::model::MaskModel;
use crate::resample::resample;
use crate::{Complex32, HeuristicSeparator, Separator, StemMasks, ThreadedSeparator, NUM_STEMS};

/// Stereo frame, `[left, right]`.
pub type Frame = [f32; 2];

/// Separated stems in [`crate::Stem`] order, each the same length and sample rate as the input.
pub type StemBuffers = [Vec<Frame>; NUM_STEMS];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfflineError {
    /// The progress callback returned `false`.
    Cancelled,
    /// The model failed (see [`crate::SeparatorStats::error`]).
    Model(String),
}

impl std::fmt::Display for OfflineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OfflineError::Cancelled => f.write_str("cancelled"),
            OfflineError::Model(e) => write!(f, "model error: {e}"),
        }
    }
}

impl std::error::Error for OfflineError {}

/// Progress callback: receives the fraction done (0..=1); return `false` to cancel.
pub type Progress<'a> = &'a mut dyn FnMut(f32) -> bool;

/// How many hops between progress callbacks.
const PROGRESS_EVERY: usize = 64;

/// Separate with an ONNX (or any [`MaskModel`]) model. Resamples to the model's rate and back.
pub fn separate_with_model<M: MaskModel>(
    input: &[Frame],
    input_rate: u32,
    model: M,
    progress: Progress,
) -> Result<StemBuffers, OfflineError> {
    let spec = model.spec();
    // One frame of pipeline is enough: offline mode waits for every result anyway.
    let sep = ThreadedSeparator::new(model, 1).map_err(|e| OfflineError::Model(e.to_string()))?;
    let stats = sep.stats();
    let out = separate(
        input,
        input_rate,
        Box::new(sep),
        spec.sample_rate.round() as u32,
        spec.fft_size,
        progress,
    )?;
    if stats.failed.load(std::sync::atomic::Ordering::Relaxed) {
        let err = stats.error.lock().unwrap().clone().unwrap_or_default();
        return Err(OfflineError::Model(err));
    }
    Ok(out)
}

/// Separate with the placeholder [`HeuristicSeparator`] (no model needed; poor quality).
pub fn separate_with_heuristic(
    input: &[Frame],
    input_rate: u32,
    progress: Progress,
) -> Result<StemBuffers, OfflineError> {
    let (rate, fft) = (44_100, 4096);
    let sep = HeuristicSeparator::new(rate as f32, fft);
    separate(input, input_rate, Box::new(sep), rate, fft, progress)
}

/// Separate `input` (at `input_rate`) with `separator`, which runs at `model_rate` with an STFT of
/// `fft_size` (hop `fft_size / 4`). The input is resampled to `model_rate` if needed and the stems
/// back to `input_rate`. Each output stem has exactly `input.len()` frames.
pub fn separate(
    input: &[Frame],
    input_rate: u32,
    mut separator: Box<dyn Separator>,
    model_rate: u32,
    fft_size: usize,
    progress: Progress,
) -> Result<StemBuffers, OfflineError> {
    separator.set_offline(true);
    let audio = resample(input, input_rate, model_rate);
    let mut stems = Stft::new(fft_size, separator).run(&audio, progress)?;
    for stem in &mut stems {
        if input_rate != model_rate {
            *stem = resample(stem, model_rate, input_rate);
        }
        stem.resize(input.len(), [0.0; 2]);
    }
    progress(1.0);
    Ok(stems)
}

struct Stft {
    n: usize,
    hop: usize,
    bins: usize,
    fft: Arc<dyn RealToComplex<f32>>,
    ifft: Arc<dyn ComplexToReal<f32>>,
    window: Vec<f32>,
    synth_norm: f32,
    separator: Box<dyn Separator>,
}

impl Stft {
    fn new(n: usize, separator: Box<dyn Separator>) -> Self {
        assert!(n.is_power_of_two() && n >= 64);
        let hop = n / 4;
        let mut planner = RealFftPlanner::<f32>::new();
        // Same periodic sqrt-Hann and normalisation as the real-time engine.
        let window: Vec<f32> = (0..n)
            .map(|i| (0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos()).sqrt())
            .collect();
        let ola: f32 = (0..n / hop).map(|k| window[k * hop].powi(2)).sum();
        Self {
            n,
            hop,
            bins: n / 2 + 1,
            fft: planner.plan_fft_forward(n),
            ifft: planner.plan_fft_inverse(n),
            synth_norm: 1.0 / (n as f32 * ola),
            window,
            separator,
        }
    }

    /// Streams `audio` through the same per-hop pipeline as the engine, then drops the latency.
    fn run(&mut self, audio: &[Frame], progress: Progress) -> Result<StemBuffers, OfflineError> {
        let (n, hop, bins) = (self.n, self.hop, self.bins);
        let lookahead = self.separator.lookahead_frames();
        // The engine emits a frame's output during the *next* hop, which accounts for one hop of
        // its `fft_size + lookahead * hop` latency; here output is appended right away.
        let delay = n + lookahead * hop - hop;
        let total = audio.len() + delay;
        let hops = total.div_ceil(hop);

        let mut out: StemBuffers = std::array::from_fn(|_| Vec::with_capacity(hops * hop));
        let mut in_buf = [vec![0.0f32; n], vec![0.0f32; n]];
        let mut acc: Vec<[Vec<f32>; 2]> = (0..NUM_STEMS)
            .map(|_| [vec![0.0; n], vec![0.0; n]])
            .collect();
        let zero = Complex32::new(0.0, 0.0);
        let mut hist: Vec<[Vec<Complex32>; 2]> = (0..=lookahead)
            .map(|_| [vec![zero; bins], vec![zero; bins]])
            .collect();
        let mut write = 0;
        let mut masks = StemMasks::new(bins);
        let mut time = vec![0.0f32; n];
        let mut spec = vec![zero; bins];
        let mut fft_scratch = self.fft.make_scratch_vec();
        let mut ifft_scratch = self.ifft.make_scratch_vec();

        for h in 0..hops {
            if h % PROGRESS_EVERY == 0 && !progress(0.98 * h as f32 / hops as f32) {
                return Err(OfflineError::Cancelled);
            }
            // New hop of input at the end of the analysis buffer (zeros past the end).
            for (ch, buf) in in_buf.iter_mut().enumerate() {
                buf.copy_within(hop.., 0);
                for i in 0..hop {
                    buf[n - hop + i] = audio.get(h * hop + i).map_or(0.0, |f| f[ch]);
                }
            }
            for (ch, buf) in in_buf.iter().enumerate() {
                for ((t, &x), &w) in time.iter_mut().zip(buf).zip(&self.window) {
                    *t = x * w;
                }
                let _ = self.fft.process_with_scratch(
                    &mut time,
                    &mut hist[write][ch],
                    &mut fft_scratch,
                );
            }
            self.separator
                .separate(&hist[write][0], &hist[write][1], &mut masks);
            let d = (write + 1) % hist.len(); // masks belong to the oldest spectrum
            write = d;

            for (s, stem_acc) in acc.iter_mut().enumerate() {
                for (ch, a) in stem_acc.iter_mut().enumerate() {
                    for ((y, &x), &m) in spec.iter_mut().zip(&hist[d][ch]).zip(&masks.masks[s]) {
                        *y = x * m;
                    }
                    spec[0].im = 0.0;
                    spec[bins - 1].im = 0.0;
                    let _ = self
                        .ifft
                        .process_with_scratch(&mut spec, &mut time, &mut ifft_scratch);
                    for ((a, &t), &w) in a.iter_mut().zip(&time).zip(&self.window) {
                        *a += t * w * self.synth_norm;
                    }
                }
            }
            for (s, stem_acc) in acc.iter_mut().enumerate() {
                out[s].extend((0..hop).map(|i| [stem_acc[0][i], stem_acc[1][i]]));
                for a in stem_acc.iter_mut() {
                    a.copy_within(hop.., 0);
                    a[n - hop..].fill(0.0);
                }
            }
        }

        for stem in &mut out {
            stem.drain(..delay.min(stem.len()));
            stem.truncate(audio.len());
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Settings, Stem, StemEngine};

    const SR: u32 = 44_100;

    fn signal(len: usize) -> Vec<Frame> {
        let mut seed = 3u32;
        (0..len)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                let t = i as f32 / SR as f32;
                let bass = 0.4 * (std::f32::consts::TAU * 70.0 * t).sin();
                let tone = 0.2 * (std::f32::consts::TAU * 880.0 * t).sin();
                [bass + tone + 0.1 * noise, bass - tone + 0.1 * noise]
            })
            .collect()
    }

    fn max_err(a: &[Frame], b: &[Frame]) -> f32 {
        a.iter()
            .zip(b)
            .map(|(x, y)| (x[0] - y[0]).abs().max((x[1] - y[1]).abs()))
            .fold(0.0, f32::max)
    }

    #[test]
    fn stems_sum_to_the_input_and_are_aligned() {
        let x = signal(SR as usize);
        let stems = separate_with_heuristic(&x, SR, &mut |_| true).unwrap();
        for s in &stems {
            assert_eq!(s.len(), x.len());
        }
        let sum: Vec<Frame> = (0..x.len())
            .map(|i| {
                let mut f = [0.0; 2];
                for s in &stems {
                    f[0] += s[i][0];
                    f[1] += s[i][1];
                }
                f
            })
            .collect();
        assert!(max_err(&sum, &x) < 1e-4, "{}", max_err(&sum, &x));
    }

    #[test]
    fn matches_the_realtime_engine_for_any_gains() {
        let x = signal(SR as usize);
        let stems = separate_with_heuristic(&x, SR, &mut |_| true).unwrap();

        let gains = [0.0, 1.0, 0.5, 2.0];
        let mut settings = Settings::default();
        for (s, g) in settings.stems.iter_mut().zip(gains) {
            s.gain = g;
        }
        let mut engine = StemEngine::new(SR as f32);
        let lat = engine.latency_samples();
        let (mut l, mut r): (Vec<f32>, Vec<f32>) = x.iter().map(|f| (f[0], f[1])).unzip();
        engine.process(&mut [&mut l, &mut r], None, &settings);

        let offline: Vec<Frame> = (0..x.len())
            .map(|i| {
                let mut f = [0.0; 2];
                for (s, g) in stems.iter().zip(gains) {
                    f[0] += g * s[i][0];
                    f[1] += g * s[i][1];
                }
                f
            })
            .collect();
        let realtime: Vec<Frame> = l[lat..]
            .iter()
            .zip(&r[lat..])
            .map(|(&a, &b)| [a, b])
            .collect();
        assert!(max_err(&offline[..realtime.len()], &realtime) < 1e-4);
    }

    #[test]
    fn bass_stem_gets_the_low_end() {
        let x = signal(SR as usize);
        let stems = separate_with_heuristic(&x, SR, &mut |_| true).unwrap();
        let energy = |v: &[Frame]| v.iter().map(|f| f[0] * f[0] + f[1] * f[1]).sum::<f32>();
        // The 70 Hz component dominates the input, so bass must be the strongest stem.
        let bass = energy(&stems[Stem::Bass as usize]);
        for s in Stem::ALL.into_iter().filter(|&s| s != Stem::Bass) {
            assert!(bass > 2.0 * energy(&stems[s as usize]), "{s:?}");
        }
    }

    #[test]
    fn resamples_48k_input_and_returns_48k_stems() {
        let x48 = resample(&signal(SR as usize), SR, 48_000);
        let stems = separate_with_heuristic(&x48, 48_000, &mut |_| true).unwrap();
        assert!(stems.iter().all(|s| s.len() == x48.len()));
        let sum: Vec<Frame> = (0..x48.len())
            .map(|i| {
                stems
                    .iter()
                    .fold([0.0; 2], |a, s| [a[0] + s[i][0], a[1] + s[i][1]])
            })
            .collect();
        // Not exact (two resampling passes), but transparent in the audio band.
        let e: f32 = sum
            .iter()
            .zip(&x48)
            .skip(500)
            .map(|(a, b)| (a[0] - b[0]).powi(2))
            .sum();
        let s: f32 = x48.iter().skip(500).map(|f| f[0] * f[0]).sum();
        assert!(10.0 * (s / e).log10() > 40.0);
    }

    #[test]
    fn cancel_and_progress() {
        let x = signal(SR as usize * 3);
        let mut seen = Vec::new();
        let r = separate_with_heuristic(&x, SR, &mut |p| {
            seen.push(p);
            seen.len() < 3
        });
        assert_eq!(r.unwrap_err(), OfflineError::Cancelled);
        assert!(seen.windows(2).all(|w| w[1] >= w[0]));

        let mut last = 0.0;
        separate_with_heuristic(&x, SR, &mut |p| {
            last = p;
            true
        })
        .unwrap();
        assert_eq!(last, 1.0);
    }
}
