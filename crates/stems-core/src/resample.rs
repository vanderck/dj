//! Offline, fixed-ratio, windowed-sinc resampling of whole buffers (used to bring tracks to the
//! model's sample rate and the stems back). Not real-time safe: it allocates.

use std::f64::consts::PI;

/// Filter half-length in input samples at unity ratio (quality vs speed).
const HALF_TAPS: usize = 32;
/// Kaiser window beta (~80 dB stopband).
const KAISER_BETA: f64 = 8.0;
/// Cutoff as a fraction of the lower Nyquist frequency.
const CUTOFF: f64 = 0.94;

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    while term > 1e-12 * sum {
        term *= (x / (2.0 * k)).powi(2);
        sum += term;
        k += 1.0;
    }
    sum
}

/// Resample stereo frames from `from_rate` to `to_rate`. The output has
/// `round(len * to_rate / from_rate)` frames and the same time alignment (sample 0 ↔ sample 0).
pub fn resample(input: &[[f32; 2]], from_rate: u32, to_rate: u32) -> Vec<[f32; 2]> {
    if from_rate == to_rate || input.is_empty() {
        return input.to_vec();
    }
    let g = gcd(from_rate as u64, to_rate as u64);
    let (up, down) = (
        (to_rate as u64 / g) as usize,
        (from_rate as u64 / g) as usize,
    );
    let out_len = ((input.len() as u64 * up as u64 + down as u64 / 2) / down as u64) as usize;

    // Low-pass at the lower of the two Nyquist frequencies, in input-sample units.
    let ratio = to_rate as f64 / from_rate as f64;
    let fc = CUTOFF * ratio.min(1.0); // cycles per input sample * 2
    let half = (HALF_TAPS as f64 / ratio.min(1.0)).ceil() as isize;
    let i0_beta = bessel_i0(KAISER_BETA);

    // Output sample j sits at input position j * down / up = base + phase / up.
    // Precompute one filter per phase (there are `up` phases).
    let taps = (2 * half + 1) as usize;
    let mut table = Vec::new();
    let polyphase = up <= 4096;
    if polyphase {
        table.reserve(up * taps);
        for phase in 0..up {
            let frac = phase as f64 / up as f64;
            let mut row: Vec<f32> = (0..taps)
                .map(|t| kernel(t as isize - half, frac, fc, half, i0_beta))
                .collect();
            // Normalise DC gain to exactly 1 per phase.
            let sum: f32 = row.iter().sum();
            row.iter_mut().for_each(|v| *v /= sum);
            table.extend(row);
        }
    }

    let mut out = Vec::with_capacity(out_len);
    let n = input.len() as isize;
    for j in 0..out_len {
        let num = j as u64 * down as u64;
        let base = (num / up as u64) as isize;
        let phase = (num % up as u64) as usize;
        let row_owned;
        let row: &[f32] = if polyphase {
            &table[phase * taps..(phase + 1) * taps]
        } else {
            let frac = phase as f64 / up as f64;
            let mut r: Vec<f32> = (0..taps)
                .map(|t| kernel(t as isize - half, frac, fc, half, i0_beta))
                .collect();
            let sum: f32 = r.iter().sum();
            r.iter_mut().for_each(|v| *v /= sum);
            row_owned = r;
            &row_owned
        };
        let mut acc = [0.0f32; 2];
        for (t, &w) in row.iter().enumerate() {
            let i = base + t as isize - half;
            if (0..n).contains(&i) {
                let x = input[i as usize];
                acc[0] += w * x[0];
                acc[1] += w * x[1];
            }
        }
        out.push(acc);
    }
    out
}

/// Windowed-sinc weight of input sample `base + k` for an output at `base + frac`.
fn kernel(k: isize, frac: f64, fc: f64, half: isize, i0_beta: f64) -> f32 {
    let x = k as f64 - frac; // distance in input samples
    let sinc = if x.abs() < 1e-12 {
        1.0
    } else {
        (PI * fc * x).sin() / (PI * fc * x)
    };
    let r = x / (half as f64 + 1.0);
    let w = if r.abs() >= 1.0 {
        0.0
    } else {
        bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / i0_beta
    };
    (fc * sinc * w) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f64, rate: u32, len: usize) -> Vec<[f32; 2]> {
        (0..len)
            .map(|i| {
                let v = (2.0 * PI * hz * i as f64 / rate as f64).sin() as f32 * 0.5;
                [v, -v]
            })
            .collect()
    }

    fn snr_db(got: &[[f32; 2]], want: &[[f32; 2]], skip: usize) -> f64 {
        let (mut s, mut e) = (0.0f64, 0.0f64);
        for (g, w) in got
            .iter()
            .zip(want)
            .skip(skip)
            .take(got.len().saturating_sub(2 * skip))
        {
            for c in 0..2 {
                s += (w[c] as f64).powi(2);
                e += (g[c] as f64 - w[c] as f64).powi(2);
            }
        }
        10.0 * (s / e.max(1e-30)).log10()
    }

    #[test]
    fn converts_a_sine_accurately_both_ways() {
        for (a, b) in [
            (48_000, 44_100),
            (44_100, 48_000),
            (96_000, 44_100),
            (22_050, 44_100),
        ] {
            let x = sine(1_000.0, a, a as usize / 2);
            let y = resample(&x, a, b);
            assert_eq!(y.len(), (x.len() as u64 * b as u64 / a as u64) as usize);
            let want = sine(1_000.0, b, y.len());
            let snr = snr_db(&y, &want, 200);
            assert!(snr > 60.0, "{a}->{b}: {snr:.1} dB");
        }
    }

    #[test]
    fn roundtrip_preserves_audio_band() {
        let x: Vec<[f32; 2]> = sine(440.0, 48_000, 24_000)
            .iter()
            .zip(sine(5_000.0, 48_000, 24_000))
            .map(|(a, b)| [a[0] + b[0], a[1] + b[1]])
            .collect();
        let back = resample(&resample(&x, 48_000, 44_100), 44_100, 48_000);
        assert_eq!(back.len(), x.len());
        assert!(snr_db(&back, &x, 300) > 55.0);
    }

    #[test]
    fn identity_and_empty() {
        let x = sine(100.0, 44_100, 100);
        assert_eq!(resample(&x, 44_100, 44_100), x);
        assert!(resample(&[], 48_000, 44_100).is_empty());
    }
}
