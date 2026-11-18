//! Separation of a whole track into stems with an STFT mask model or the heuristic placeholder.
//!
//! The STFT framing is the one the model contract promises (`docs/stems/MODEL_CONTRACT.md`):
//! frame `t` is `rfft(x[(t+1)*hop - N .. (t+1)*hop] * sqrt_hann)` with zeros before the start,
//! hop `N / 4`. Every stem is synthesised separately and the latency removed, so stem sample
//! `n` lines up with input sample `n`.

use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::model::{sanitize_masks, MaskModel};
use crate::resample::resample;
use crate::{Complex32, HeuristicSeparator, Separator, StemMasks, NUM_STEMS};

/// Stereo frame, `[left, right]`.
pub type Frame = [f32; 2];

/// Separated stems in [`crate::Stem`] order, each the same length and sample rate as the input.
pub type StemBuffers = [Vec<Frame>; NUM_STEMS];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfflineError {
    /// The progress callback returned `false`.
    Cancelled,
    /// The model failed.
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
pub fn separate_with_model<M: MaskModel + 'static>(
    input: &[Frame],
    input_rate: u32,
    model: M,
    progress: Progress,
) -> Result<StemBuffers, OfflineError> {
    let spec = model.spec();
    spec.validate()
        .map_err(|e| OfflineError::Model(e.to_string()))?;
    let sep = ModelSeparator::new(model);
    let error = sep.error.clone();
    let out = separate(
        input,
        input_rate,
        Box::new(sep),
        spec.sample_rate.round() as u32,
        spec.fft_size,
        progress,
    )?;
    let failed = error.borrow_mut().take();
    match failed {
        Some(e) => Err(OfflineError::Model(e)),
        None => Ok(out),
    }
}

/// Runs a [`MaskModel`] over a sliding window of frames.
struct ModelSeparator<M: MaskModel> {
    model: M,
    /// `[context_frames][INPUT_PLANES][num_bins]`, oldest first.
    context: Vec<f32>,
    out: Vec<f32>,
    /// The first inference error (later frames get even masks).
    error: Rc<RefCell<Option<String>>>,
}

impl<M: MaskModel> ModelSeparator<M> {
    fn new(model: M) -> Self {
        let spec = model.spec();
        Self {
            context: vec![0.0; spec.input_len()],
            out: vec![0.0; spec.output_len()],
            model,
            error: Default::default(),
        }
    }
}

impl<M: MaskModel> Separator for ModelSeparator<M> {
    fn lookahead_frames(&self) -> usize {
        self.model.spec().lookahead_frames
    }

    fn separate(&mut self, left: &[Complex32], right: &[Complex32], masks: &mut StemMasks) {
        let spec = self.model.spec();
        let (bins, frame) = (spec.num_bins(), spec.frame_len());
        // Slide the window and append the newest frame as planes L.re, L.im, R.re, R.im.
        self.context.copy_within(frame.., 0);
        let newest = &mut self.context[spec.input_len() - frame..];
        let planes = left
            .iter()
            .map(|c| c.re)
            .chain(left.iter().map(|c| c.im))
            .chain(right.iter().map(|c| c.re))
            .chain(right.iter().map(|c| c.im));
        for (dst, v) in newest.iter_mut().zip(planes) {
            *dst = v;
        }
        if self.error.borrow().is_none() {
            if let Err(e) = self.model.infer(&self.context, &mut self.out) {
                *self.error.borrow_mut() = Some(e.to_string());
            }
        }
        if self.error.borrow().is_some() {
            self.out.fill(1.0 / NUM_STEMS as f32);
        }
        sanitize_masks(&mut self.out, bins);
        for (s, m) in masks.masks.iter_mut().enumerate() {
            m.copy_from_slice(&self.out[s * bins..(s + 1) * bins]);
        }
    }
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
    separator: Box<dyn Separator>,
    model_rate: u32,
    fft_size: usize,
    progress: Progress,
) -> Result<StemBuffers, OfflineError> {
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
        // Periodic sqrt-Hann for analysis and synthesis; with 75 % overlap the windows sum to a
        // constant (`ola`), which `synth_norm` divides out along with the inverse FFT's scale.
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

    /// Streams `audio` through the STFT hop by hop, then drops the latency.
    fn run(&mut self, audio: &[Frame], progress: Progress) -> Result<StemBuffers, OfflineError> {
        let (n, hop, bins) = (self.n, self.hop, self.bins);
        let lookahead = self.separator.lookahead_frames();
        // A hop's synthesis is complete `fft_size` samples after its first input sample, plus
        // `lookahead` hops while the masks wait for future frames; the hop appended right away
        // saves one hop.
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
    use crate::Stem;

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

    /// A model that sends a frame to Drums when its target frame (T - 1 - lookahead) has a
    /// positive DC bin, else to Other. Wrong lookahead handling or misplaced frames would leak
    /// some of a DC block into Other.
    struct DcModel(crate::ModelSpec);

    impl MaskModel for DcModel {
        fn spec(&self) -> crate::ModelSpec {
            self.0
        }

        fn infer(&mut self, context: &[f32], masks: &mut [f32]) -> Result<(), crate::ModelError> {
            let s = self.0;
            let target = s.context_frames - 1 - s.lookahead_frames;
            let dc = context[target * s.frame_len()]; // plane L.re, bin 0
            let bins = s.num_bins();
            masks.fill(0.0);
            let stem = if dc > 1e-6 { 0 } else { 3 };
            masks[stem * bins..(stem + 1) * bins].fill(1.0);
            Ok(())
        }
    }

    #[test]
    fn model_masks_line_up_with_their_frames() {
        for lookahead in [0, 1, 3] {
            let spec = crate::ModelSpec {
                sample_rate: 44_100.0,
                fft_size: 64,
                context_frames: 5,
                lookahead_frames: lookahead,
            };
            let x: Vec<Frame> = (0..2000)
                .map(|i| {
                    if (700..1300).contains(&i) {
                        [0.5, 0.25]
                    } else {
                        [0.0; 2]
                    }
                })
                .collect();
            let stems = separate_with_model(&x, SR, DcModel(spec), &mut |_| true).unwrap();
            assert!(
                max_err(&stems[0], &x) < 1e-5,
                "lookahead {lookahead}: drums"
            );
            assert!(
                max_err(&stems[3], &vec![[0.0; 2]; x.len()]) < 1e-5,
                "lookahead {lookahead}: other"
            );
        }
    }

    #[test]
    fn model_errors_are_reported() {
        struct Broken;
        impl MaskModel for Broken {
            fn spec(&self) -> crate::ModelSpec {
                crate::ModelSpec {
                    sample_rate: 44_100.0,
                    fft_size: 64,
                    context_frames: 2,
                    lookahead_frames: 0,
                }
            }
            fn infer(&mut self, _: &[f32], _: &mut [f32]) -> Result<(), crate::ModelError> {
                Err(crate::ModelError::new("boom"))
            }
        }
        let r = separate_with_model(&signal(1000), SR, Broken, &mut |_| true);
        assert_eq!(r.unwrap_err(), OfflineError::Model("boom".into()));
    }

    /// The frames a separator sees follow the documented STFT exactly.
    #[test]
    fn frames_match_the_documented_stft() {
        type Frames = Rc<RefCell<Vec<(Vec<Complex32>, Vec<Complex32>)>>>;
        struct Capture(Frames);
        impl Separator for Capture {
            fn separate(&mut self, l: &[Complex32], r: &[Complex32], masks: &mut StemMasks) {
                self.0.borrow_mut().push((l.to_vec(), r.to_vec()));
                for m in &mut masks.masks {
                    m.fill(0.25);
                }
            }
        }
        let (n, hop) = (64, 16);
        let len = 10 * hop + 5;
        let x: Vec<Frame> = (0..len)
            .map(|i| [((i * 37 % 23) as f32 - 11.0) / 11.0, (i as f32 * 0.3).sin()])
            .collect();
        let frames: Frames = Default::default();
        separate(
            &x,
            SR,
            Box::new(Capture(frames.clone())),
            SR,
            n,
            &mut |_| true,
        )
        .unwrap();
        let window: Vec<f64> = (0..n)
            .map(|i| (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos()).sqrt())
            .collect();
        for (t, (fl, fr)) in frames.borrow().iter().enumerate().take(len / hop) {
            for (ch, got) in [fl, fr].into_iter().enumerate() {
                let seg: Vec<f64> = (0..n)
                    .map(|i| {
                        let idx = (t + 1) as isize * hop as isize - n as isize + i as isize;
                        let v = if idx >= 0 {
                            x[idx as usize][ch] as f64
                        } else {
                            0.0
                        };
                        v * window[i]
                    })
                    .collect();
                for (k, g) in got.iter().enumerate() {
                    let (re, im) = seg
                        .iter()
                        .enumerate()
                        .fold((0.0, 0.0), |(re, im), (i, &v)| {
                            let a = -std::f64::consts::TAU * (k * i) as f64 / n as f64;
                            (re + v * a.cos(), im + v * a.sin())
                        });
                    assert!(
                        (g.re as f64 - re).abs() < 1e-3 && (g.im as f64 - im).abs() < 1e-3,
                        "frame {t} bin {k}"
                    );
                }
            }
        }
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
