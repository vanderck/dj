//! HT-Demucs's signal processing around its network, as Demucs 4.1 does it (htdemucs.py:
//! `_spec`, `_magnitude`, `_mask`, `_ispec`, and the normalisation in `forward`), for a network
//! that runs on its own (Core ML: see `tools/htdemucs_coreml`).
//!
//! The network's inputs: the segment's spectrogram (STFT, 4096 points, hop 1024, Hann window,
//! normalised; complex as channels: left re, left im, right re, right im), and its wave; each
//! normalised by its own mean and standard deviation. Its outputs: per source the same, still
//! normalised; the result per source is the inverse STFT of the one plus the other.

use realfft::num_complex::Complex32;
use realfft::RealFftPlanner;

pub const NFFT: usize = 4096;
pub const HOP: usize = NFFT / 4;
/// Frequency bins the network sees (the last, Nyquist, is left out).
pub const BINS: usize = NFFT / 2;

/// Frames for a segment of `len` samples.
pub fn frames(len: usize) -> usize {
    len.div_ceil(HOP)
}

/// What the network gets for one segment (`wave`: left then right, `len` samples each), and what
/// undoes its normalisation.
pub struct Inputs {
    /// [4, BINS, frames]
    pub spec: Vec<f32>,
    /// [2, len]
    pub wave: Vec<f32>,
    spec_norm: (f32, f32),
    wave_norm: (f32, f32),
}

/// Mean and (unbiased) standard deviation plus 1e-5, as `x.mean()`, `1e-5 + x.std()`.
fn norm(x: &[f32]) -> (f32, f32) {
    let n = x.len() as f64;
    let mean = x.iter().map(|&v| v as f64).sum::<f64>() / n;
    let var = x.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / (n - 1.0).max(1.0);
    (mean as f32, (var.sqrt() + 1e-5) as f32)
}

/// `x` padded by reflection (the edge sample not repeated), `left` and `right` samples.
fn reflect(x: &[f32], left: usize, right: usize) -> Vec<f32> {
    let n = x.len() as isize;
    let at = |i: isize| -> f32 {
        let period = 2 * (n - 1).max(1);
        let mut j = i.rem_euclid(period);
        if j >= n {
            j = period - j;
        }
        x[j as usize]
    };
    (-(left as isize)..n + right as isize).map(at).collect()
}

fn hann() -> Vec<f32> {
    (0..NFFT)
        .map(|i| (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / NFFT as f64).cos()) as f32)
        .collect()
}

/// The network's inputs for a segment `wave` ([2, len]).
pub fn inputs(wave: &[f32], len: usize) -> Inputs {
    let le = frames(len);
    let pad = HOP / 2 * 3;
    let window = hann();
    let mut planner = RealFftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(NFFT);
    let mut frame = fft.make_input_vec();
    let mut bins = fft.make_output_vec();
    let scale = 1.0 / (NFFT as f32).sqrt();
    let mut spec = vec![0.0f32; 4 * BINS * le];
    for c in 0..2 {
        let x = &wave[c * len..(c + 1) * len];
        // `_spec`: reflect `pad` each side (and up to whole frames), then torch.stft's centring.
        let x = reflect(x, pad, pad + le * HOP - len);
        let x = reflect(&x, NFFT / 2, NFFT / 2);
        // Frames 2..2+le of the STFT (`z[..., 2: 2 + le]`).
        for t in 0..le {
            let start = (t + 2) * HOP;
            for (i, f) in frame.iter_mut().enumerate() {
                *f = x[start + i] * window[i];
            }
            fft.process(&mut frame, &mut bins).expect("fft sizes");
            for (k, z) in bins[..BINS].iter().enumerate() {
                spec[((c * 2) * BINS + k) * le + t] = z.re * scale;
                spec[((c * 2 + 1) * BINS + k) * le + t] = z.im * scale;
            }
        }
    }
    let spec_norm = norm(&spec);
    spec.iter_mut()
        .for_each(|v| *v = (*v - spec_norm.0) / spec_norm.1);
    let wave_norm = norm(wave);
    let wave = wave
        .iter()
        .map(|v| (v - wave_norm.0) / wave_norm.1)
        .collect();
    Inputs {
        spec,
        wave,
        spec_norm,
        wave_norm,
    }
}

/// The sources' waves ([sources, 2, len]) from the network's outputs (`spec_out`: [sources, 4,
/// BINS, frames]; `wave_out`: [sources, 2, len]).
pub fn sources(input: &Inputs, spec_out: &[f32], wave_out: &[f32], len: usize) -> Vec<f32> {
    let le = frames(len);
    let n_sources = wave_out.len() / (2 * len);
    let pad = HOP / 2 * 3;
    // `_ispec`: a zero Nyquist bin, two empty frames each side, then istft (centred, length
    // `le * HOP + 2 * pad`) and the `pad` samples cut off.
    let frames_all = le + 4;
    let full = NFFT + HOP * (frames_all - 1);
    let window = hann();
    let mut planner = RealFftPlanner::<f32>::new();
    let ifft = planner.plan_fft_inverse(NFFT);
    let mut bins = ifft.make_input_vec();
    let mut frame = ifft.make_output_vec();
    // torch.istft(normalized=True): × sqrt(N), and irfft's 1/N.
    let scale = (NFFT as f32).sqrt() / NFFT as f32;
    let mut envelope = vec![0.0f32; full];
    for t in 0..frames_all {
        for (i, w) in window.iter().enumerate() {
            envelope[t * HOP + i] += w * w;
        }
    }
    let (sm, ss) = input.spec_norm;
    let (wm, ws) = input.wave_norm;
    let mut out = vec![0.0f32; n_sources * 2 * len];
    let mut acc = vec![0.0f32; full];
    for s in 0..n_sources {
        for c in 0..2 {
            acc.iter_mut().for_each(|v| *v = 0.0);
            for t in 0..le {
                for (k, b) in bins.iter_mut().enumerate() {
                    *b = if k < BINS {
                        let re = spec_out[((s * 4 + c * 2) * BINS + k) * le + t] * ss + sm;
                        let im = spec_out[((s * 4 + c * 2 + 1) * BINS + k) * le + t] * ss + sm;
                        Complex32::new(re, if k == 0 { 0.0 } else { im })
                    } else {
                        Complex32::new(0.0, 0.0)
                    };
                }
                ifft.process(&mut bins, &mut frame).expect("ifft sizes");
                let start = (t + 2) * HOP;
                for (i, (&v, &w)) in frame.iter().zip(&window).enumerate() {
                    acc[start + i] += v * scale * w;
                }
            }
            // Centring (NFFT / 2) and the `pad` cut: sample i is at NFFT / 2 + pad + i.
            let dst = &mut out[(s * 2 + c) * len..(s * 2 + c + 1) * len];
            let time = &wave_out[(s * 2 + c) * len..(s * 2 + c + 1) * len];
            for (i, (d, &xt)) in dst.iter_mut().zip(time).enumerate() {
                let j = NFFT / 2 + pad + i;
                *d = acc[j] / envelope[j] + xt * ws + wm;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reflection_skips_the_edge() {
        assert_eq!(
            reflect(&[1.0, 2.0, 3.0, 4.0], 2, 3),
            vec![3.0, 2.0, 1.0, 2.0, 3.0, 4.0, 3.0, 2.0, 1.0]
        );
    }

    #[test]
    fn the_spectrogram_inverts() {
        // A network that hands back its inputs, for one "source": the wave twice over (once
        // from the spectrogram, once from the wave).
        let len = 20_000;
        let wave: Vec<f32> = (0..2 * len)
            .map(|i| ((i % len) as f32 * if i < len { 0.013 } else { 0.021 }).sin() * 0.3 + 0.01)
            .collect();
        let inp = inputs(&wave, len);
        assert_eq!(inp.spec.len(), 4 * BINS * frames(len));
        let out = sources(&inp, &inp.spec, &inp.wave, len);
        // (Demucs keeps only the frames inside the segment, so its first and last 2.5k samples
        // come back attenuated: there the overlapping segments take over.)
        for c in 0..2 {
            for i in 3000..len - 3000 {
                let (y, x) = (out[c * len + i], wave[c * len + i]);
                assert!((y - 2.0 * x).abs() < 1e-4, "{c} {i}: {y} vs {}", 2.0 * x);
            }
        }
    }
}
