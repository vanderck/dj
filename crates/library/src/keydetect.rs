//! Key detection for tracks without a key tag.
//!
//! The track's chroma (how much of each of the 12 pitch classes it holds), gathered as a
//! harmonic pitch class profile: the spectrum of each frame (mono, ~11 kHz, 8192-point frames),
//! whitened so that loud low notes don't drown the rest, its peaks between 50 Hz and 5 kHz each
//! adding to the notes they could be (the peak itself and, weaker, as the 2nd–4th harmonic of a
//! lower note), in thirds of a semitone, apart for bass (below 200 Hz) and the rest. The track's
//! tuning is the third collecting most; the bins fold into 12 around it.
//!
//! The key is the one of 24 a small linear model scores highest on the two chroma (bass and
//! treble, each log-scaled, turned to that key's tonic and standardised). dj trained the model
//! (50 numbers) on the GiantSteps key dataset (604 EDM tracks, Knees et al. 2015): 69 % of keys
//! exact, and 83 % exact, a fifth off or the relative key (all of which mix well), in
//! cross-validation.

use dj_engine::track::AudioData;
use realfft::RealFftPlanner;

use crate::key::Key;

const TARGET_RATE: f64 = 11_025.0;
const N: usize = 8192;
const HOP: usize = 4096;
const F_MIN: f64 = 50.0;
const F_MAX: f64 = 5000.0;
/// Bins per semitone, and in all.
const RES: usize = 3;
pub const BINS: usize = 12 * RES;

/// How the chroma is gathered.
#[derive(Clone, Debug)]
pub struct Opts {
    /// Harmonics a peak may be (1 = only itself).
    pub harmonics: i32,
    /// Peak weight: its whitened height (false) or the log of it (true).
    pub log_weight: bool,
    /// Notes below this (Hz) count as bass.
    pub bass_below: f64,
    /// A peak must stand this far above its surroundings (whitened).
    pub threshold: f32,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { harmonics: 4, log_weight: false, bass_below: 200.0, threshold: 2.0 }
    }
}

/// Bass and treble chroma (36 bins each, unnormalised sums of per-frame peak-normalised
/// profiles).
pub fn chroma_split(audio: &AudioData, o: &Opts) -> Option<([f64; BINS], [f64; BINS])> {
    let sr = audio.sample_rate as f64;
    if sr <= 0.0 || audio.frames.len() < N {
        return None;
    }
    let factor = (sr / TARGET_RATE).round().max(1.0) as usize;
    let rate = sr / factor as f64;
    let mono: Vec<f32> = audio.frames.chunks(factor).map(|c| c.iter().map(|f| 0.5 * (f[0] + f[1])).sum::<f32>() / c.len() as f32).collect();
    let window: Vec<f32> = (0..N).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / N as f32).cos()).collect();
    let fft = RealFftPlanner::<f32>::new().plan_fft_forward(N);
    let mut buf = fft.make_input_vec();
    let mut spec = fft.make_output_vec();
    let bin_hz = rate / N as f64;
    let lo = (F_MIN / bin_hz).floor().max(1.0) as usize;
    let hi = ((F_MAX / bin_hz).ceil() as usize).min(N / 2 - 1);
    let mut mag = vec![0f32; N / 2 + 1];
    let mut white = vec![0f32; N / 2 + 1];
    let mut total = [0f64; BINS];
    let mut total_bass = [0f64; BINS];
    let mut start = 0;
    while start + N <= mono.len() {
        for (b, (x, w)) in buf.iter_mut().zip(mono[start..start + N].iter().zip(&window)) {
            *b = x * w;
        }
        start += HOP;
        if fft.process(&mut buf, &mut spec).is_err() {
            continue;
        }
        for (m, c) in mag.iter_mut().zip(&spec) {
            *m = c.norm();
        }
        // Whitening: each bin over the mean around it (a third of an octave each way).
        let mut prefix = vec![0f64; mag.len() + 1];
        for (i, m) in mag.iter().enumerate() {
            prefix[i + 1] = prefix[i] + *m as f64;
        }
        let mut frame = [0f64; BINS];
        let mut bass = [0f64; BINS];
        let mut any = false;
        for k in lo..=hi {
            let a = ((k as f64) / 1.26).floor() as usize;
            let b = (((k as f64) * 1.26).ceil() as usize).min(mag.len() - 1);
            let mean = (prefix[b + 1] - prefix[a]) / (b + 1 - a) as f64;
            white[k] = if mean > 1e-9 { (mag[k] as f64 / mean) as f32 } else { 0.0 };
        }
        for k in lo + 1..hi {
            let (l, m, r) = (white[k - 1], white[k], white[k + 1]);
            // Peaks clearly above their surroundings.
            if m <= l || m < r || m < o.threshold {
                continue;
            }
            // Where the peak really is (parabola through the three bins).
            let d = 0.5 * (l - r) as f64 / (l as f64 - 2.0 * m as f64 + r as f64).min(-1e-9);
            let f = (k as f64 + d) * bin_hz;
            let weight = if o.log_weight { (m as f64).ln() } else { (m as f64 - 1.0).min(30.0) };
            for h in 1..=o.harmonics {
                let note = 12.0 * (f / h as f64 / 440.0).log2() + 9.0; // C = 0
                if f / (h as f64) < F_MIN * 0.8 {
                    break;
                }
                let w = weight * 0.6f64.powi(h - 1);
                add(if f / (h as f64) < o.bass_below { &mut bass } else { &mut frame }, note, w);
                any = true;
            }
        }
        if any {
            for (fr, tot) in [(&frame, &mut total), (&bass, &mut total_bass)] {
                let max = fr.iter().cloned().fold(0.0, f64::max);
                if max > 0.0 {
                    for (t, v) in tot.iter_mut().zip(fr) {
                        *t += v / max;
                    }
                }
            }
        }
    }
    let any = total.iter().chain(&total_bass).any(|v| *v > 0.0);
    any.then_some((total_bass, total))
}

/// Spread a note (in semitones from C, fractional) over the nearest bins with a cos² window a
/// semitone wide.
fn add(bins: &mut [f64; BINS], note: f64, w: f64) {
    let pos = note * RES as f64;
    let half = RES as f64 / 2.0;
    let first = (pos - half).ceil() as i64;
    let last = (pos + half).floor() as i64;
    for b in first..=last {
        let d = (b as f64 - pos) / half; // -1..1
        let g = (d * std::f64::consts::FRAC_PI_2).cos().powi(2);
        bins[b.rem_euclid(BINS as i64) as usize] += w * g;
    }
}

/// The track's tuning: the third of a semitone (-1, 0 or +1) collecting most.
fn tuning(c: &[f64; BINS]) -> i64 {
    let r = RES as i64;
    let at = |b: i64| c[b.rem_euclid(BINS as i64) as usize];
    (-(r / 2)..=r / 2)
        .max_by(|&a, &b| {
            let s = |o: i64| (0..12).map(|p| at(p * r + o)).sum::<f64>();
            s(a).total_cmp(&s(b))
        })
        .unwrap_or(0)
}

/// Fold 36 bins into 12, each pitch class the three thirds around it at that tuning.
fn fold_at(c: &[f64; BINS], o: i64) -> [f64; 12] {
    let r = RES as i64;
    let at = |b: i64| c[b.rem_euclid(BINS as i64) as usize];
    std::array::from_fn(|p| (-(r / 2)..=r / 2).map(|j| at(p as i64 * r + o + j)).sum())
}

/// The model: per mode (major, minor), weights for the bass chroma then the treble chroma (each
/// turned to the key's tonic, log-scaled and standardised), and a bias.
const WEIGHTS: [[f64; 24]; 2] = [
    [1.5646, -0.6489, -0.6808, -0.1610, 0.6290, 0.3661, -0.8239, 0.9877, -0.3596, 0.0961, -0.5790, -0.3727, 0.3334, -0.9782, 0.2529, -0.6943, 0.4553, -0.0535, -0.0790, 0.4670, 0.2569, -0.3326, 0.3900, -0.0355],
    [1.6594, -0.6044, -0.3647, 0.3297, -0.0378, -0.3406, -0.8221, 0.8778, 0.0906, -0.4634, -0.1206, -0.2213, 0.8991, -0.1278, -0.1625, 0.2567, -0.1846, -0.2351, -0.1096, 0.3301, -0.5681, -0.6289, 0.9321, -0.3837],
];
const BIAS: [f64; 2] = [-0.7721, 0.7721];

/// The track's key, and how sure the model is (the chosen key's probability among the 24);
/// `None` for silence.
pub fn detect(audio: &AudioData) -> Option<(Key, f64)> {
    let (bass, treble) = chroma_split(audio, &Opts::default())?;
    Some(key_from_chroma(&bass, &treble))
}

/// The key for a bass and a treble chroma (36 bins each, see [`chroma_split`]).
pub fn key_from_chroma(bass: &[f64; BINS], treble: &[f64; BINS]) -> (Key, f64) {
    let both: [f64; BINS] = std::array::from_fn(|i| bass[i] + treble[i]);
    let o = tuning(&both);
    let feature = |c: &[f64; BINS]| {
        let f = fold_at(c, o);
        let max = f.iter().cloned().fold(0.0, f64::max).max(1e-9);
        f.map(|v| (1.0 + 4.0 * v / max).ln())
    };
    let (b, t) = (feature(bass), feature(treble));
    let mut scores = [0f64; 24];
    for tonic in 0..12 {
        // Both chroma turned so the tonic comes first, then standardised together.
        let x: [f64; 24] = std::array::from_fn(|i| if i < 12 { b[(i + tonic) % 12] } else { t[(i - 12 + tonic) % 12] });
        let mean = x.iter().sum::<f64>() / 24.0;
        let sd = (x.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / 24.0).sqrt() + 1e-9;
        for m in 0..2 {
            scores[m * 12 + tonic] = WEIGHTS[m].iter().zip(&x).map(|(w, v)| w * (v - mean) / sd).sum::<f64>() + BIAS[m];
        }
    }
    let best = (0..24).max_by(|&a, &b| scores[a].total_cmp(&scores[b])).unwrap_or(0);
    let top = scores[best];
    let sum: f64 = scores.iter().map(|s| (s - top).exp()).sum();
    (Key { pitch: (best % 12) as u8, minor: best >= 12 }, 1.0 / sum)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::key::Key;

    /// A chord progression (MIDI notes per chord, one second each, eight times round) with a
    /// bass note, as plain tones with a few harmonics.
    pub(crate) fn song(chords: &[[u8; 4]]) -> AudioData {
        let sr = 44_100u32;
        let mut frames = Vec::new();
        for _ in 0..8 {
            for chord in chords {
                for i in 0..sr as usize {
                    let t = i as f64 / sr as f64;
                    let mut v = 0.0;
                    for (j, &n) in chord.iter().enumerate() {
                        let f = 440.0 * 2f64.powf((n as f64 - 69.0) / 12.0);
                        let level = if j == 0 { 0.5 } else { 0.25 };
                        for h in 1..=3 {
                            v += level / h as f64 * (std::f64::consts::TAU * f * h as f64 * t).sin();
                        }
                    }
                    let v = (v * 0.2) as f32;
                    frames.push([v, v]);
                }
            }
        }
        AudioData { sample_rate: sr, frames }
    }

    #[test]
    fn finds_the_key_of_a_chord_progression() {
        // A minor: Am Dm E Am (bass A1 D2 E2 A1; E's G♯ makes it minor, not C major).
        let am = song(&[[33, 57, 60, 64], [38, 57, 62, 65], [40, 56, 59, 64], [33, 57, 60, 64]]);
        let (key, sure) = detect(&am).expect("a key");
        assert_eq!(key, Key { pitch: 9, minor: true }, "{} ({sure:.2})", key.musical());
        // D major: D G A D (bass D2 G1 A1 D2).
        let d = song(&[[38, 62, 66, 69], [31, 59, 62, 67], [33, 61, 64, 69], [38, 62, 66, 69]]);
        let (key, sure) = detect(&d).expect("a key");
        assert_eq!(key, Key { pitch: 2, minor: false }, "{} ({sure:.2})", key.musical());
    }

    #[test]
    fn silence_has_no_key() {
        let a = AudioData { sample_rate: 44_100, frames: vec![[0.0, 0.0]; 44_100 * 10] };
        assert!(detect(&a).is_none());
    }

    #[test]
    fn a_detuned_track_keeps_its_key() {
        // The A minor progression a third of a semitone sharp.
        let base = song(&[[33, 57, 60, 64], [38, 57, 62, 65], [40, 56, 59, 64], [33, 57, 60, 64]]);
        let (k0, _) = detect(&base).unwrap();
        // (Resample by playing it 1/3 semitone faster: same frames, higher rate.)
        let sharp = AudioData { sample_rate: (44_100.0 * 2f64.powf(1.0 / 36.0)) as u32, frames: base.frames };
        let (k1, _) = detect(&sharp).unwrap();
        assert_eq!((k0, k1), (Key { pitch: 9, minor: true }, Key { pitch: 9, minor: true }));
    }
}
