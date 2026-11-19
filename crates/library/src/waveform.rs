//! 3-band waveform (low / mid / high RMS + peak per bin) and beat alignment.

use dj_engine::dsp::{Biquad, Lr4Crossover};
use dj_engine::track::AudioData;

pub const BINS_PER_SEC: f64 = 150.0;

#[derive(Clone, Debug, Default)]
pub struct Waveform {
    pub bins_per_sec: f64,
    /// Normalized 0..1 per bin.
    pub low: Vec<f32>,
    pub mid: Vec<f32>,
    pub high: Vec<f32>,
    pub peak: Vec<f32>,
}

impl Waveform {
    pub fn len(&self) -> usize {
        self.peak.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peak.is_empty()
    }

    pub fn build(audio: &AudioData) -> Self {
        let sr = audio.sample_rate as f32;
        let per = (audio.sample_rate as f64 / BINS_PER_SEC).max(1.0);
        let n = (audio.frames.len() as f64 / per).ceil() as usize;
        let mut w = Waveform {
            bins_per_sec: BINS_PER_SEC,
            low: Vec::with_capacity(n),
            mid: Vec::with_capacity(n),
            high: Vec::with_capacity(n),
            peak: Vec::with_capacity(n),
        };
        let mut xl = Lr4Crossover::new(sr, 200.0);
        let mut xh = Lr4Crossover::new(sr, 2500.0);
        let mut acc = [0.0f32; 3];
        let mut pk = 0.0f32;
        let mut cnt = 0usize;
        let mut next = per;
        for (i, f) in audio.frames.iter().enumerate() {
            let m = [(f[0] + f[1]) * 0.5; 2];
            let (lo, rest) = xl.split(m);
            let (mi, hi) = xh.split(rest);
            acc[0] += lo[0] * lo[0];
            acc[1] += mi[0] * mi[0];
            acc[2] += hi[0] * hi[0];
            pk = pk.max(f[0].abs()).max(f[1].abs());
            cnt += 1;
            if (i + 1) as f64 >= next || i + 1 == audio.frames.len() {
                let c = cnt.max(1) as f32;
                w.low.push((acc[0] / c).sqrt());
                w.mid.push((acc[1] / c).sqrt());
                w.high.push((acc[2] / c).sqrt());
                w.peak.push(pk);
                acc = [0.0; 3];
                pk = 0.0;
                cnt = 0;
                next += per;
            }
        }
        // Normalize bands jointly so relative colour stays meaningful.
        let max = w.low.iter().chain(&w.mid).chain(&w.high).fold(1e-6f32, |a, &b| a.max(b));
        for v in w.low.iter_mut().chain(w.mid.iter_mut()).chain(w.high.iter_mut()) {
            *v = (*v / max).min(1.0);
        }
        w
    }

    /// Downsample to `n` columns (max over each span) for overview drawing.
    pub fn overview(&self, n: usize) -> Vec<[f32; 4]> {
        let len = self.len();
        if len == 0 || n == 0 {
            return Vec::new();
        }
        (0..n)
            .map(|i| {
                let a = i * len / n;
                let b = ((i + 1) * len / n).max(a + 1).min(len);
                let mut o = [0.0f32; 4];
                for j in a..b {
                    o[0] = o[0].max(self.low[j]);
                    o[1] = o[1].max(self.mid[j]);
                    o[2] = o[2].max(self.high[j]);
                    o[3] = o[3].max(self.peak[j]);
                }
                o
            })
            .collect()
    }
}

/// Given a known BPM (from tags), find the time of a beat by aligning a
/// beat comb with the low-frequency onset envelope. Returns seconds in
/// `[0, 60/bpm)`. This does not estimate tempo.
pub fn align_beats(audio: &AudioData, bpm: f64) -> f64 {
    let sr = audio.sample_rate as f64;
    let hop = 128usize;
    let spb = 60.0 / bpm;
    let limit = audio.frames.len().min((sr * 120.0) as usize);
    let mut lp = Biquad::lowpass(sr as f32, 150.0, 0.7);
    let mut env = Vec::with_capacity(limit / hop + 1);
    let mut acc = 0.0f32;
    for (i, f) in audio.frames[..limit].iter().enumerate() {
        let x = lp.tick(0, (f[0] + f[1]) * 0.5);
        acc += x * x;
        if (i + 1) % hop == 0 {
            env.push(acc.sqrt());
            acc = 0.0;
        }
    }
    // Onset strength: positive first difference.
    let onset: Vec<f32> = env.windows(2).map(|w| (w[1] - w[0]).max(0.0)).collect();
    if onset.is_empty() {
        return 0.0;
    }
    let hop_s = hop as f64 / sr;
    let steps = 96;
    let mut best = (0.0, f32::MIN);
    for s in 0..steps {
        let phase = s as f64 / steps as f64 * spb;
        let mut score = 0.0f32;
        let mut t = phase;
        while t < onset.len() as f64 * hop_s {
            let idx = (t / hop_s) as usize;
            if idx < onset.len() {
                score += onset[idx];
            }
            t += spb;
        }
        if score > best.1 {
            best = (phase, score);
        }
    }
    // The onset index i covers env[i]→env[i+1]; shift by one hop.
    (best.0 + hop_s).rem_euclid(spb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligns_to_kicks_with_known_bpm() {
        let sr = 44100u32;
        let bpm = 125.0;
        let spb = 60.0 / bpm;
        let offset = 0.137;
        let mut frames = vec![[0.0f32; 2]; sr as usize * 30];
        let mut t = offset;
        while t < 29.0 {
            let s = (t * sr as f64) as usize;
            for k in 0..4000 {
                let env = (-(k as f32) / 900.0).exp();
                let v = (k as f32 / sr as f32 * 55.0 * std::f32::consts::TAU).sin() * env;
                frames[s + k] = [v, v];
            }
            t += spb;
        }
        let a = AudioData { sample_rate: sr, frames };
        let found = align_beats(&a, bpm);
        let err = (found - offset).abs().min((found - offset - spb).abs());
        assert!(err < 0.012, "found {found}, expected {offset}");
        let w = Waveform::build(&a);
        assert_eq!(w.len(), (30.0 * BINS_PER_SEC) as usize);
        assert!(w.low.iter().cloned().fold(0.0, f32::max) > 0.9);
        assert_eq!(w.overview(100).len(), 100);
    }
}
