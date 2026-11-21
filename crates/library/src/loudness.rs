//! Integrated loudness (ITU-R BS.1770 / EBU R128): K-weighted, 400 ms blocks with 75 % overlap,
//! absolute gate at −70 LUFS and relative gate 10 LU below.

use dj_engine::track::AudioData;

/// A biquad in direct form I, per channel.
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    x: [[f64; 2]; 2],
    y: [[f64; 2]; 2],
}

impl Biquad {
    fn new(b: [f64; 3], a: [f64; 3]) -> Self {
        Biquad { b: [b[0] / a[0], b[1] / a[0], b[2] / a[0]], a: [a[1] / a[0], a[2] / a[0]], x: [[0.0; 2]; 2], y: [[0.0; 2]; 2] }
    }

    fn run(&mut self, ch: usize, v: f64) -> f64 {
        let (x, y) = (&mut self.x[ch], &mut self.y[ch]);
        let out = self.b[0] * v + self.b[1] * x[0] + self.b[2] * x[1] - self.a[0] * y[0] - self.a[1] * y[1];
        x[1] = x[0];
        x[0] = v;
        y[1] = y[0];
        y[0] = out;
        out
    }
}

/// The two K-weighting stages for a sample rate (BS.1770's filters, designed as libebur128 and
/// ffmpeg do, so they match the standard's coefficients at 48 kHz exactly).
fn k_weighting(sr: f64) -> (Biquad, Biquad) {
    use std::f64::consts::PI;
    // Stage 1: high shelf, +4 dB above ~1.7 kHz (the head).
    let (f0, g, q) = (1_681.974_450_955_533, 3.999_843_853_973_347, 0.707_175_236_955_419_6);
    let k = (PI * f0 / sr).tan();
    let vh = 10f64.powf(g / 20.0);
    let vb = vh.powf(0.499_666_774_154_541_6);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad::new([(vh + vb * k / q + k * k) / a0, 2.0 * (k * k - vh) / a0, (vh - vb * k / q + k * k) / a0], [1.0, 2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0]);
    // Stage 2: high pass at ~38 Hz.
    let (f0, q) = (38.135_470_876_024_44, 0.500_327_037_323_877_3);
    let k = (PI * f0 / sr).tan();
    let a0 = 1.0 + k / q + k * k;
    let hp = Biquad::new([1.0, -2.0, 1.0], [1.0, 2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0]);
    (shelf, hp)
}

/// Integrated loudness in LUFS, or `None` for silence (or audio shorter than one block).
pub fn integrated(audio: &AudioData) -> Option<f64> {
    let sr = audio.sample_rate as f64;
    if sr <= 0.0 {
        return None;
    }
    let (mut shelf, mut hp) = k_weighting(sr);
    // Mean square per 100 ms step (both channels summed); blocks are 4 steps.
    let step = (sr * 0.1).round() as usize;
    let mut steps = Vec::with_capacity(audio.frames.len() / step.max(1) + 1);
    let mut acc = 0.0f64;
    let mut n = 0usize;
    for f in &audio.frames {
        for (ch, v) in f.iter().enumerate() {
            let k = hp.run(ch, shelf.run(ch, *v as f64));
            acc += k * k;
        }
        n += 1;
        if n == step {
            steps.push(acc / step as f64);
            acc = 0.0;
            n = 0;
        }
    }
    let blocks: Vec<f64> = steps.windows(4).map(|w| w.iter().sum::<f64>() / 4.0).collect();
    let lufs = |z: f64| -0.691 + 10.0 * z.log10();
    let gated = |min: f64| -> Option<f64> {
        let kept: Vec<f64> = blocks.iter().copied().filter(|z| *z > 0.0 && lufs(*z) > min).collect();
        (!kept.is_empty()).then(|| kept.iter().sum::<f64>() / kept.len() as f64)
    };
    let abs = gated(-70.0)?;
    let rel = gated(lufs(abs) - 10.0)?;
    Some(lufs(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(amp: f32, secs: f32, sr: u32) -> AudioData {
        let n = (secs * sr as f32) as usize;
        let frames = (0..n).map(|i| {
            let v = amp * (i as f32 * 997.0 * std::f32::consts::TAU / sr as f32).sin();
            [v, v]
        });
        AudioData { sample_rate: sr, frames: frames.collect() }
    }

    #[test]
    fn a_full_scale_sine_reads_as_bs1770_says() {
        // A 997 Hz sine at 0 dBFS in both channels: −3.01 dB per channel, summed: about 0 LUFS.
        let l = integrated(&sine(1.0, 5.0, 48000)).unwrap();
        assert!((l - 0.0).abs() < 0.2, "{l}");
        // Half the amplitude is 6 dB quieter, at any sample rate.
        let h = integrated(&sine(0.5, 5.0, 44100)).unwrap();
        assert!((l - h - 6.02).abs() < 0.2, "{l} {h}");
    }

    #[test]
    fn silence_is_gated_out() {
        let mut a = sine(0.5, 5.0, 48000);
        let loud = integrated(&a).unwrap();
        a.frames.extend(std::iter::repeat([0.0, 0.0]).take(48000 * 20));
        assert!((integrated(&a).unwrap() - loud).abs() < 0.3, "silence doesn't pull it down");
        assert_eq!(integrated(&sine(0.0, 5.0, 48000)), None);
    }
}
