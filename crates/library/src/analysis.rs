//! Audio analysis for tracks without Mixed In Key tags: tempo, beat grid (with bar starts), a
//! rough energy level, and cue points found from the track's structure (sharper with stems).
//!
//! Everything works on an onset/spectrum envelope computed once per track ([`Envelope`]):
//! mono, decimated to ~22 kHz, 1024-point frames every 10 ms.

use dj_engine::track::{AudioData, BeatGrid, Cue, CueKind, StemData};
use realfft::RealFftPlanner;

const N: usize = 1024;
const TARGET_RATE: f64 = 22_050.0;
const BANDS: usize = 24;
const BPM_MIN: f64 = 60.0;
const BPM_MAX: f64 = 200.0;
/// Detected tempos are folded into this range (halved / doubled).
pub const FOLD_MIN: f64 = 78.0;
pub const FOLD_MAX: f64 = 178.0;

/// Per-frame features of a track.
pub struct Envelope {
    /// Seconds between frames.
    pub hop_s: f64,
    /// Time of frame 0's centre.
    pub t0: f64,
    /// Onset strength (spectral flux, kick-weighted), detrended and normalised to mean 1.
    pub onset: Vec<f32>,
    /// The same below 150 Hz only (kicks): where the beat lands.
    pub low_onset: Vec<f32>,
    /// Energy below 150 Hz, 150 Hz–2 kHz and above 2 kHz.
    pub bands3: Vec<[f32; 3]>,
    /// Log energy in `BANDS` log-spaced bands (for bar-start detection).
    pub spectrum: Vec<[f32; BANDS]>,
    pub duration: f64,
}

impl Envelope {
    pub fn compute(audio: &AudioData) -> Self {
        let sr = audio.sample_rate as f64;
        let factor = (sr / TARGET_RATE).round().max(1.0) as usize;
        let rate = sr / factor as f64;
        let mono: Vec<f32> = audio
            .frames
            .chunks(factor)
            .map(|c| c.iter().map(|f| 0.5 * (f[0] + f[1])).sum::<f32>() / c.len() as f32)
            .collect();
        let hop = (rate * 0.01).round() as usize;
        let frames = mono.len() / hop + 1;
        let window: Vec<f32> = (0..N).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / N as f32).cos()).collect();
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(N);
        let mut buf = fft.make_input_vec();
        let mut spec = fft.make_output_vec();
        let bins = N / 2 + 1;
        let bin_hz = rate / N as f64;
        let low_bin = (150.0 / bin_hz) as usize;
        let mid_bin = (2000.0 / bin_hz) as usize;
        // Log-spaced band edges from 40 Hz to 11 kHz.
        let edges: Vec<usize> = (0..=BANDS)
            .map(|b| ((40.0 * (11_000.0f64 / 40.0).powf(b as f64 / BANDS as f64)) / bin_hz).round() as usize)
            .map(|k| k.clamp(1, bins - 1))
            .collect();

        let mut prev = vec![0.0f32; bins];
        let mut flux = Vec::with_capacity(frames);
        let mut flux_low = Vec::with_capacity(frames);
        let mut bands3 = Vec::with_capacity(frames);
        let mut spectrum = Vec::with_capacity(frames);
        for t in 0..frames {
            let start = t * hop;
            for (i, b) in buf.iter_mut().enumerate() {
                *b = mono.get(start + i).copied().unwrap_or(0.0) * window[i];
            }
            let _ = fft.process(&mut buf, &mut spec);
            let (mut f_all, mut f_low) = (0.0f32, 0.0f32);
            let mut e3 = [0.0f32; 3];
            for k in 0..bins {
                let mag = spec[k].norm();
                let l = (1.0 + 10.0 * mag).ln();
                let d = (l - prev[k]).max(0.0);
                prev[k] = l;
                f_all += d;
                let p = mag * mag;
                if k < low_bin {
                    f_low += d;
                    e3[0] += p;
                } else if k < mid_bin {
                    e3[1] += p;
                } else {
                    e3[2] += p;
                }
            }
            let mut sb = [0.0f32; BANDS];
            for (b, v) in sb.iter_mut().enumerate() {
                let (lo, hi) = (edges[b], edges[b + 1].max(edges[b] + 1));
                let e: f32 = (lo..hi.min(bins)).map(|k| spec[k].norm_sqr()).sum();
                *v = (1e-6 + e / (hi - lo) as f32).ln();
            }
            // Kicks carry the beat in most dance music: weight the lows.
            flux.push(f_all + 4.0 * f_low);
            flux_low.push(f_low);
            bands3.push(e3);
            spectrum.push(sb);
        }
        Self {
            hop_s: hop as f64 / rate,
            t0: N as f64 / 2.0 / rate,
            onset: detrend(flux),
            low_onset: detrend(flux_low),
            bands3,
            spectrum,
            duration: audio.frames.len() as f64 / sr,
        }
    }

    /// `env` at time `t` (linear interpolation; 0 outside the track).
    fn at(&self, env: &[f32], t: f64) -> f32 {
        let x = (t - self.t0) / self.hop_s;
        if x < 0.0 {
            return 0.0;
        }
        let i = x as usize;
        let f = (x - i as f64) as f32;
        match (env.get(i), env.get(i + 1)) {
            (Some(a), Some(b)) => a + (b - a) * f,
            (Some(a), None) => *a,
            _ => 0.0,
        }
    }

    /// Mean of `env` on a comb of period `60/bpm` starting at `phase`.
    fn comb(&self, env: &[f32], bpm: f64, phase: f64) -> f32 {
        let spb = 60.0 / bpm;
        let (mut sum, mut n) = (0.0f32, 0u32);
        let mut t = phase;
        while t < self.duration {
            sum += self.at(env, t);
            n += 1;
            t += spb;
        }
        if n == 0 {
            0.0
        } else {
            sum / n as f32
        }
    }

    /// Comb score of `bpm` within `[from, to)` seconds (best of 48 phases).
    fn score_between(&self, bpm: f64, from: f64, to: f64) -> f32 {
        let spb = 60.0 / bpm;
        (0..48)
            .map(|s| {
                let mut t = from + s as f64 / 48.0 * spb;
                let (mut sum, mut n) = (0.0, 0);
                while t < to {
                    sum += self.at(&self.onset, t);
                    n += 1;
                    t += spb;
                }
                sum / n.max(1) as f32
            })
            .fold(f32::MIN, f32::max)
    }

    /// Best comb score on the onset envelope over `steps` phases, and that phase.
    fn best_phase(&self, bpm: f64, steps: usize) -> (f32, f64) {
        self.best_phase_of(&self.onset, bpm, steps)
    }

    fn best_phase_of(&self, env: &[f32], bpm: f64, steps: usize) -> (f32, f64) {
        let spb = 60.0 / bpm;
        (0..steps)
            .map(|s| {
                let phase = s as f64 / steps as f64 * spb;
                (self.comb(env, bpm, phase), phase)
            })
            .fold((f32::MIN, 0.0), |a, b| if b.0 > a.0 { b } else { a })
    }
}

/// Subtract a 1 s moving average, rectify, normalise to mean 1.
fn detrend(mut flux: Vec<f32>) -> Vec<f32> {
    if let Some(f) = flux.first_mut() {
        *f = 0.0; // no previous frame
    }
    let w = 100; // frames of 10 ms
    let mut csum = vec![0.0f64; flux.len() + 1];
    for (i, v) in flux.iter().enumerate() {
        csum[i + 1] = csum[i] + *v as f64;
    }
    let mut out: Vec<f32> = (0..flux.len())
        .map(|i| {
            let (a, b) = (i.saturating_sub(w / 2), (i + w / 2 + 1).min(flux.len()));
            let mean = (csum[b] - csum[a]) / (b - a) as f64;
            (flux[i] as f64 - mean).max(0.0) as f32
        })
        .collect();
    let mean = out.iter().map(|v| *v as f64).sum::<f64>() / out.len().max(1) as f64;
    if mean > 0.0 {
        out.iter_mut().for_each(|v| *v /= mean as f32);
    }
    out
}

/// A grid from tags checked against the audio: for every 8 bars, where the beats (kicks first)
/// really land within ±0.3 beat of the grid's; a line through those gives how far the grid is off
/// and how fast it drifts. Taggers' grids can be a fraction of a beat early or late, or a hair off
/// in tempo (Push The Feeling On: 6 ms a minute), and two tracks locked by such grids flam.
/// Returns the corrected grid when it's audibly off (8 ms or more anywhere in the track) and the
/// measurement is clear; `None` when the grid fits or the beat isn't clear enough to tell.
pub fn refine_grid(env: &Envelope, grid: &BeatGrid) -> Option<BeatGrid> {
    refine_grid_within(env, grid, 0.008)
}

/// [`refine_grid`], correcting when the grid is `tolerance` seconds off or more somewhere.
fn refine_grid_within(env: &Envelope, grid: &BeatGrid, tolerance: f64) -> Option<BeatGrid> {
    let spb = grid.seconds_per_beat();
    if !(spb > 0.0) || env.duration < 30.0 {
        return None;
    }
    const WIN: usize = 32; // beats
    let step = 0.002;
    let reach = (0.3 * spb / step) as i64;
    let offsets: Vec<f64> = (-reach..=reach).map(|i| i as f64 * step).collect();
    // The beat lands on the kicks: measure on the low band when it has a clear pulse (offbeat
    // hats and claps are as strong as the beat in the full band), else on the full band.
    let measure = |env_band: &[f32]| -> (Vec<(f64, f64, f64)>, usize) {
        let mut pts: Vec<(f64, f64, f64)> = Vec::new(); // (time, offset, weight)
        let mut windows = 0usize;
        let mut b = (-grid.anchor_secs / spb).ceil();
        loop {
            let t0 = grid.secs_at_beat(b);
            if t0 + WIN as f64 * spb > env.duration {
                break;
            }
            windows += 1;
            let scores: Vec<f32> = offsets.iter().map(|d| (0..WIN).map(|k| env.at(env_band, t0 + k as f64 * spb + d)).sum::<f32>() / WIN as f32).collect();
            let mean = scores.iter().sum::<f32>() / scores.len() as f32;
            let (i, best) = scores.iter().copied().enumerate().fold((0, f32::MIN), |a, c| if c.1 > a.1 { c } else { a });
            let clarity = best / mean.max(1e-6);
            if clarity > 1.4 {
                // Between the 2 ms steps: the peak of a parabola through the best one and its
                // neighbours.
                let frac = match (i.checked_sub(1).map(|j| scores[j]), scores.get(i + 1)) {
                    (Some(a), Some(&c)) => {
                        let den = a - 2.0 * best + c;
                        if den < 0.0 { (0.5 * (a - c) / den) as f64 } else { 0.0 }
                    }
                    _ => 0.0,
                };
                pts.push((t0 + WIN as f64 * spb / 2.0, offsets[i] + frac.clamp(-0.5, 0.5) * step - ENVELOPE_LAG, clarity as f64));
            }
            b += WIN as f64;
        }
        (pts, windows)
    };
    let (low, windows) = measure(&env.low_onset);
    let (pts, windows) = if low.len() * 2 >= windows && low.iter().map(|p| p.2).sum::<f64>() / low.len().max(1) as f64 > 1.8 { (low, windows) } else { measure(&env.onset) };
    if pts.len() < 4 || pts.len() * 2 < windows {
        return None;
    }
    // Weighted line fit; again without the windows far off the line (breaks, fills).
    let fit = |pts: &[(f64, f64, f64)]| -> (f64, f64, f64) {
        let w: f64 = pts.iter().map(|p| p.2).sum();
        let tm = pts.iter().map(|p| p.0 * p.2).sum::<f64>() / w;
        let om = pts.iter().map(|p| p.1 * p.2).sum::<f64>() / w;
        let var = pts.iter().map(|p| p.2 * (p.0 - tm).powi(2)).sum::<f64>();
        let k = if var > 0.0 { pts.iter().map(|p| p.2 * (p.0 - tm) * (p.1 - om)).sum::<f64>() / var } else { 0.0 };
        (tm, om, k)
    };
    let (tm, om, k) = fit(&pts);
    let kept: Vec<_> = pts.iter().copied().filter(|p| (p.1 - (om + k * (p.0 - tm))).abs() < 0.025).collect();
    if kept.len() < 4 || kept.len() * 3 < pts.len() * 2 {
        return None;
    }
    let (tm, om, k) = fit(&kept);
    if k.abs() > 0.003 {
        return None; // not a tempo a tagger gets this wrong: something else is going on
    }
    let off = |t: f64| om + k * (t - tm);
    let worst = off(0.0).abs().max(off(env.duration).abs());
    if worst < tolerance {
        return None;
    }
    // Beat n moves to where it really is: anchor + n·spb + off(anchor + n·spb).
    Some(BeatGrid { bpm: grid.bpm / (1.0 + k), anchor_secs: grid.anchor_secs + off(grid.anchor_secs) })
}

/// How much later than a sound's start [`Envelope`]'s onsets appear (its frames are centred on
/// a 46 ms window), measured on clean kicks (see `refine_grid_measures_kicks` below).
const ENVELOPE_LAG: f64 = -0.0097;

/// Detected tempo and beat grid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TempoGrid {
    pub grid: BeatGrid,
    /// How clearly the beat stands out (peak comb score / median); below ~1.5 is doubtful.
    pub confidence: f32,
}

/// Tempo (folded into `FOLD_MIN..FOLD_MAX`), the beat phase and the bar starts. `None` for silence or
/// audio without a steady pulse.
pub fn detect_grid(env: &Envelope) -> Option<TempoGrid> {
    if env.onset.iter().all(|v| *v == 0.0) || env.duration < 8.0 {
        return None;
    }
    // Coarse scan.
    let coarse: Vec<(f64, f32)> = (0..=((BPM_MAX - BPM_MIN) * 2.0) as usize)
        .map(|i| BPM_MIN + i as f64 * 0.5)
        .map(|b| (b, env.best_phase(b, 24).0))
        .collect();
    let mut sorted: Vec<f32> = coarse.iter().map(|c| c.1).collect();
    sorted.sort_by(f32::total_cmp);
    let median = sorted[sorted.len() / 2].max(1e-6);
    let score = |b: f64| env.best_phase(b, 24).0;
    let (mut bpm, _) = coarse.iter().copied().fold((0.0, f32::MIN), |a, b| if b.1 > a.1 { b } else { a });
    // Octave: a comb at half the tempo scores as well (every other beat still hits) and offbeat
    // hats can make the double tempo win, so fold into the DJ range FOLD_MIN..FOLD_MAX (as DJ
    // software's "BPM range" does): 64 -> 128, 190 -> 95, 174 stays.
    while bpm >= FOLD_MAX {
        bpm /= 2.0;
    }
    while bpm < FOLD_MIN {
        bpm *= 2.0;
    }
    // A tie between a tempo and its double (kicks on every beat score the same on both) goes
    // to the double, if it stays in range and the kicks (low band) land on its beats too.
    let low = |b: f64| env.best_phase_of(&env.low_onset, b, 24).0;
    while bpm * 2.0 < FOLD_MAX && score(bpm * 2.0) >= 0.8 * score(bpm) && low(bpm * 2.0) >= 0.75 * low(bpm) {
        bpm *= 2.0;
    }
    let mut best = score(bpm);
    // Refine: ±1 BPM in 0.05 steps, then ±0.05 in 0.005 steps (a long comb is sharp).
    for (span, step, phases) in [(1.0, 0.05, 48), (0.05, 0.005, 96)] {
        let n = (2.0 * span / step) as i32;
        let (b, s) = (0..=n)
            .map(|i| bpm - span + i as f64 * step)
            .map(|b| (b, env.best_phase(b, phases).0))
            .fold((bpm, f32::MIN), |a, c| if c.1 > a.1 { c } else { a });
        bpm = b;
        best = s;
    }
    // Most dance tracks have a whole-number tempo.
    let whole = bpm.round();
    if (whole - bpm).abs() < 0.05 && env.best_phase(whole, 96).0 >= 0.97 * best {
        bpm = whole;
    } else {
        bpm = (bpm * 100.0).round() / 100.0;
    }
    // Live drummers drift (e.g. 122.5 → 124.5 → 122.5 BPM): the whole-track comb then locks
    // onto the steadiest stretch. Measure 30 s windows; when they disagree, use their weighted
    // mean, which keeps the grid closest overall.
    let windows: Vec<(f64, f32)> = (0..)
        .map(|w| w as f64 * 30.0)
        .take_while(|from| from + 30.0 <= env.duration)
        .map(|from| {
            (0..=300)
                .map(|i| bpm * (0.97 + 0.0002 * i as f64))
                .map(|b| (b, env.score_between(b, from, from + 30.0)))
                .fold((bpm, f32::MIN), |a, c| if c.1 > a.1 { c } else { a })
        })
        .collect();
    let total: f32 = windows.iter().map(|w| w.1).sum();
    if windows.len() >= 3 && total > 0.0 {
        let mean = windows.iter().map(|w| w.0 * w.1 as f64).sum::<f64>() / total as f64;
        let sd = (windows.iter().map(|w| (w.0 - mean).powi(2) * w.1 as f64).sum::<f64>() / total as f64).sqrt();
        if sd > 0.25 {
            bpm = (mean * 100.0).round() / 100.0;
            best = env.best_phase(bpm, 96).0;
        }
    }
    let confidence = best / median;
    if confidence < 1.15 {
        return None;
    }
    let spb = 60.0 / bpm;
    // The beat lands on the kicks: phase from the low band when it has a pulse (offbeat hats
    // and claps are as strong in the full-band flux), else from the full band.
    let steps = (spb / 0.002) as usize;
    let (low_score, low_phase) = env.best_phase_of(&env.low_onset, bpm, steps);
    let low_median = {
        let mut v: Vec<f32> = (0..24).map(|s| env.comb(&env.low_onset, bpm, s as f64 / 24.0 * spb)).collect();
        v.sort_by(f32::total_cmp);
        v[12].max(1e-6)
    };
    let phase = if low_score / low_median > 1.5 { low_phase } else { env.best_phase(bpm, steps).1 };
    let bar = find_bar_phase(env, bpm, phase);
    // (The envelope reads onsets a little early: see ENVELOPE_LAG.)
    // (The envelope reads onsets a little early: see ENVELOPE_LAG.)
    let anchor = (phase - ENVELOPE_LAG + bar as f64 * spb).rem_euclid(4.0 * spb);
    // A tempo a hair off (rounded to 0.01) tilts the grid across the track: fit it to the beats.
    let grid = BeatGrid { bpm, anchor_secs: anchor };
    let grid = refine_grid_within(env, &grid, 0.002).filter(|g| (g.anchor_secs - anchor).abs() < 0.25 * spb).unwrap_or(grid);
    Some(TempoGrid { grid, confidence })
}

/// Which of the four beats starts the bars: the one where the sound changes most (new chords,
/// bass notes, phrases and fills land on bar starts).
fn find_bar_phase(env: &Envelope, bpm: f64, phase: f64) -> usize {
    let spb = 60.0 / bpm;
    let beats = ((env.duration - phase) / spb) as usize;
    let frame = |t: f64| (((t - env.t0) / env.hop_s).max(0.0) as usize).min(env.spectrum.len().saturating_sub(1));
    // Mean log spectrum per beat.
    let per_beat: Vec<[f32; BANDS]> = (0..beats)
        .map(|k| {
            let (a, b) = (frame(phase + k as f64 * spb), frame(phase + (k + 1) as f64 * spb).max(frame(phase + k as f64 * spb) + 1));
            let mut m = [0.0f32; BANDS];
            for s in &env.spectrum[a..b.min(env.spectrum.len())] {
                for (x, v) in m.iter_mut().zip(s) {
                    *x += v;
                }
            }
            let n = (b - a).max(1) as f32;
            m.iter_mut().for_each(|x| *x /= n);
            m
        })
        .collect();
    let mut score = [0.0f32; 4];
    for k in 1..per_beat.len() {
        let change: f32 = per_beat[k].iter().zip(&per_beat[k - 1]).map(|(a, b)| (a - b).abs()).sum();
        score[k % 4] += change;
    }
    (0..4).max_by(|&a, &b| score[a].total_cmp(&score[b])).unwrap_or(0)
}

/// A rough 1–10 energy level (Mixed In Key's scale is its own; this tracks loudness,
/// brightness and tempo).
pub fn estimate_energy(audio: &AudioData, env: &Envelope, bpm: Option<f64>) -> u8 {
    let sr = audio.sample_rate as usize;
    // Loudness of the loud half of the track, in 1 s blocks.
    let mut blocks: Vec<f32> = audio
        .frames
        .chunks(sr.max(1))
        .map(|c| 10.0 * (c.iter().map(|f| 0.5 * (f[0] * f[0] + f[1] * f[1])).sum::<f32>() / c.len() as f32 + 1e-10).log10())
        .collect();
    blocks.sort_by(f32::total_cmp);
    let loud = blocks[blocks.len() / 2..].iter().sum::<f32>() / blocks[blocks.len() / 2..].len().max(1) as f32;
    let (lo, hi): (f32, f32) = env.bands3.iter().fold((0.0, 0.0), |a, b| (a.0 + b[0] + b[1], a.1 + b[2]));
    let bright = hi / (lo + hi + 1e-9);
    let clamp01 = |x: f32| x.clamp(0.0, 1.0);
    let e = 0.5 * clamp01((loud + 22.0) / 14.0) + 0.25 * clamp01((bright - 0.03) / 0.2) + 0.25 * clamp01((bpm.unwrap_or(110.0) as f32 - 80.0) / 60.0);
    (1.0 + (9.0 * e).round()) as u8
}

// ------------------------------------------------------------------ cue points

/// The kind of section a detected cue starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Intro,
    Drop,
    Break,
    Vocal,
    Outro,
    Phrase,
}

impl Section {
    pub fn name(self) -> &'static str {
        match self {
            Section::Intro => "Intro",
            Section::Drop => "Drop",
            Section::Break => "Break",
            Section::Vocal => "Vocal",
            Section::Outro => "Outro",
            Section::Phrase => "Phrase",
        }
    }

    fn color(self) -> [u8; 3] {
        match self {
            Section::Intro => [0x9a, 0x9a, 0xa8],
            Section::Drop => [0xe5, 0x3e, 0x3e],
            Section::Break => [0x3b, 0x7b, 0xe6],
            Section::Vocal => [0x2e, 0xc4, 0x6b],
            Section::Outro => [0x8b, 0x5c, 0xf6],
            Section::Phrase => [0xf5, 0x9e, 0x0b],
        }
    }
}

/// Per-bar features: [intensity, vocals, low, mid, high, drums-activity], each normalised to
/// the track (1 ≈ its loud parts).
fn bar_features(audio: &AudioData, env: &Envelope, bar_times: &[f64], stems: Option<&StemData>) -> Vec<[f32; 6]> {
    let bars = bar_times.len().saturating_sub(1);
    let frame = |t: f64| (((t - env.t0) / env.hop_s).max(0.0) as usize).min(env.onset.len());
    let sr = audio.sample_rate as f64;
    let mut raw: Vec<[f32; 6]> = Vec::with_capacity(bars);
    for j in 0..bars {
        let (t0, t1) = (bar_times[j], bar_times[j + 1]);
        let (a, b) = (frame(t0), frame(t1).max(frame(t0) + 1).min(env.onset.len()));
        let n = (b - a).max(1) as f32;
        let mut e3 = [0.0f32; 3];
        for e in &env.bands3[a..b] {
            for (x, v) in e3.iter_mut().zip(e) {
                *x += v;
            }
        }
        let e3 = e3.map(|v| (v / n).sqrt());
        let onset = env.onset[a..b].iter().sum::<f32>() / n;
        let (drums, bass, vocals) = match stems {
            Some(s) => {
                let (i0, i1) = ((t0 * sr) as usize, ((t1 * sr) as usize).min(s.len()));
                let rms = |v: &Vec<[i16; 2]>| {
                    let e: f64 = v[i0.min(i1)..i1].iter().step_by(4).map(|f| (f[0] as f64).powi(2) + (f[1] as f64).powi(2)).sum();
                    (e / ((i1 - i0.min(i1)) / 4).max(1) as f64).sqrt() as f32 * s.scale
                };
                (rms(&s.drums), rms(&s.bass), rms(&s.vocals))
            }
            None => (onset, e3[0], 0.0),
        };
        raw.push([drums, bass, vocals, e3[0], e3[1], e3[2]]);
    }
    // Normalise each feature by its 90th percentile.
    let mut feats = raw.clone();
    for c in 0..6 {
        let mut v: Vec<f32> = raw.iter().map(|r| r[c]).collect();
        v.sort_by(f32::total_cmp);
        let p = v.get(v.len() * 9 / 10).copied().unwrap_or(0.0).max(1e-9);
        for f in feats.iter_mut() {
            f[c] = (f[c] / p).min(1.5);
        }
    }
    // [intensity (drums + bass), vocals, low, mid, high, drums]
    feats.iter().map(|f| [0.5 * (f[0] + f[1]), f[2], f[3], f[4], f[5], f[0]]).collect()
}

/// Follow the actual beats through the track, starting from the grid: each next beat is
/// expected one (locally adapted) beat later and snapped to a clear hit within ±8 % of a beat.
/// The tempo may wander ±3 % from the grid (live drummers). Returns bar start times: every
/// 4th beat from the grid's downbeat.
pub fn track_bars(env: &Envelope, grid: &BeatGrid) -> Vec<f64> {
    let spb = grid.seconds_per_beat();
    // The earliest beat at or after 0, and its position in the bar.
    let k = (grid.anchor_secs / spb).floor();
    let mut t = grid.anchor_secs - k * spb;
    let mut in_bar = (-(k as i64)).rem_euclid(4) as usize;
    let mut local = spb;
    let frame = |t: f64| ((t - env.t0) / env.hop_s).round().max(0.0) as usize;
    let mut bars = Vec::new();
    while t < env.duration {
        if in_bar == 0 {
            bars.push(t);
        }
        let predicted = t + local;
        let w = 0.08 * spb;
        let (a, b) = (frame(predicted - w), frame(predicted + w).min(env.onset.len().saturating_sub(1)));
        let next = match (a..=b).filter(|_| a <= b).max_by(|&x, &y| env.onset[x].total_cmp(&env.onset[y])) {
            Some(i) if env.onset[i] > 1.5 => env.t0 + i as f64 * env.hop_s,
            _ => predicted,
        };
        local = (0.8 * local + 0.2 * (next - t)).clamp(0.97 * spb, 1.03 * spb);
        t = next;
        in_bar = (in_bar + 1) % 4;
    }
    bars.push(env.duration.max(bars.last().copied().unwrap_or(0.0) + 4.0 * spb));
    bars
}

/// Cue points from the track's structure: section changes on phrase boundaries (multiples of
/// 4 bars), named Intro / Drop / Break / Vocal / Outro / Phrase. At most 8, in time order.
/// With `stems`, drums, bass and vocals are measured directly; without, from the mix.
pub fn detect_cues(audio: &AudioData, env: &Envelope, grid: &BeatGrid, stems: Option<&StemData>) -> Vec<Cue> {
    let bar_times = track_bars(env, grid);
    let bars = bar_features(audio, env, &bar_times, stems);
    if bars.len() < 16 {
        return Vec::new();
    }
    // Where the music starts (as Mixxx's intro detection: the first sound above -60 dBFS):
    // the bar it falls in, or the next bar when it is only a short pickup before it.
    let first_sound = audio.frames.iter().position(|f| f[0].abs().max(f[1].abs()) >= 0.001).unwrap_or(0) as f64 / audio.sample_rate as f64;
    let spb = grid.seconds_per_beat();
    let start = bar_times
        .windows(2)
        .position(|w| w[1] > first_sound + 0.25 * spb)
        .map(|j| if first_sound > bar_times[j] + 2.0 * spb { j + 1 } else { j })
        .unwrap_or(0)
        .min(bars.len().saturating_sub(1));
    let mean = |a: usize, b: usize, c: usize| bars[a..b].iter().map(|f| f[c]).sum::<f32>() / (b - a).max(1) as f32;
    // Novelty at each phrase boundary: change between the 4 bars before and after.
    let block = 4;
    let mut cands: Vec<(usize, f32)> = Vec::new();
    let mut j = start + block;
    while j + block <= bars.len() {
        let nov: f32 = [0usize, 1, 2, 3, 4, 5].iter().map(|&c| (mean(j, j + block, c) - mean(j - block, j, c)).abs()).sum();
        // 8- and 16-bar phrase starts are more likely section starts.
        let rel = j - start;
        let bonus = if rel % 16 == 0 { 1.15 } else if rel % 8 == 0 { 1.07 } else { 1.0 };
        cands.push((j, nov * bonus));
        j += block;
    }
    if cands.is_empty() {
        return Vec::new();
    }
    let avg = cands.iter().map(|c| c.1).sum::<f32>() / cands.len() as f32;
    // Boundaries: local maxima (±8 bars) clearly above average.
    let mut bounds: Vec<usize> = cands
        .iter()
        .filter(|(j, n)| *n > 1.3 * avg && *n > 0.25 && cands.iter().all(|(k, m)| k == j || (*k as i64 - *j as i64).abs() > 8 || m < n))
        .map(|c| c.0)
        .collect();
    bounds.sort_unstable();

    // Sections between boundaries; label each boundary by how intensity and vocals change.
    let mut edges = vec![start];
    edges.extend(&bounds);
    edges.push(bars.len());
    let sec = |i: usize| (mean(edges[i], edges[i + 1], 0), mean(edges[i], edges[i + 1], 1));
    let n_bars = bars.len() as f32;
    let mut cues: Vec<(usize, Section, f32)> = Vec::new();
    let (i0, _) = sec(0);
    cues.push((start, if i0 < 0.6 { Section::Intro } else { Section::Phrase }, f32::MAX));
    for s in 1..edges.len() - 1 {
        let (ip, vp) = sec(s - 1);
        let (is, vs) = sec(s);
        let pos = edges[s] as f32 / n_bars;
        let label = if is - ip > 0.25 && is > 0.6 {
            Section::Drop
        } else if ip - is > 0.2 && pos > 0.75 {
            Section::Outro
        } else if ip - is > 0.25 && is < 0.55 {
            Section::Break
        } else if stems.is_some() && vs - vp > 0.3 {
            Section::Vocal
        } else {
            Section::Phrase
        };
        let rank = match label {
            Section::Drop => 4.0,
            Section::Break => 3.0,
            Section::Outro | Section::Vocal => 2.0,
            _ => 1.0,
        } + cands.iter().find(|c| c.0 == edges[s]).map_or(0.0, |c| c.1 / (avg + 1e-9) * 0.1);
        cues.push((edges[s], label, rank));
    }
    // Keep the 8 most telling, in time order.
    cues.sort_by(|a, b| b.2.total_cmp(&a.2));
    cues.truncate(8);
    cues.sort_by_key(|c| c.0);
    cues.into_iter()
        .map(|(bar, label, _)| Cue {
            kind: CueKind::Hot,
            secs: bar_times[bar].max(0.0),
            end_secs: None,
            name: label.name().to_string(),
            color: label.color(),
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const SR: u32 = 44_100;

    /// A synthetic dance track: per bar, which parts play (kick on every beat, offbeat hats,
    /// a bass note per bar that changes every bar, a pad chord that changes every bar).
    pub(crate) fn track(bpm: f64, offset: f64, bars: &[(bool, bool, bool, bool)]) -> AudioData {
        let spb = 60.0 / bpm;
        let len = ((offset + bars.len() as f64 * 4.0 * spb + 1.0) * SR as f64) as usize;
        let mut frames = vec![[0.0f32; 2]; len];
        let mut seed = 7u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        for (j, &(kick, hats, bass, pad)) in bars.iter().enumerate() {
            let bar_t = offset + j as f64 * 4.0 * spb;
            let note = [55.0f32, 65.4, 49.0, 73.4][j % 4];
            for beat in 0..4 {
                let bt = bar_t + beat as f64 * spb;
                let s0 = (bt * SR as f64) as usize;
                for k in 0..(spb * SR as f64) as usize {
                    let Some(f) = frames.get_mut(s0 + k) else { break };
                    let t = k as f32 / SR as f32;
                    let mut v = 0.0;
                    if kick && k < 6000 {
                        v += 0.6 * (t * 50.0 * std::f32::consts::TAU).sin() * (-(k as f32) / 1500.0).exp();
                    }
                    let half = (spb * SR as f64 / 2.0) as usize;
                    if hats && k >= half && k < half + 1500 {
                        v += 0.15 * noise() * (-((k - half) as f32) / 400.0).exp();
                    }
                    let gt = (bt - bar_t) as f32 + t;
                    if bass {
                        v += 0.25 * (gt * note * std::f32::consts::TAU).sin();
                    }
                    if pad {
                        for m in [4.0f32, 5.0, 6.0] {
                            v += 0.04 * (gt * note * m * std::f32::consts::TAU).sin();
                        }
                    }
                    f[0] += v;
                    f[1] += v;
                }
            }
        }
        AudioData { sample_rate: SR, frames }
    }

    fn full(n: usize) -> Vec<(bool, bool, bool, bool)> {
        vec![(true, true, true, true); n]
    }

    #[test]
    fn finds_tempo_phase_and_bar_starts() {
        for (bpm, offset) in [(128.0, 0.31), (124.0, 1.02), (174.0, 0.5), (95.0, 0.2)] {
            let a = track(bpm, offset, &full(48));
            let env = Envelope::compute(&a);
            let g = detect_grid(&env).expect("a grid").grid;
            assert!((g.bpm - bpm).abs() < 0.02, "bpm {} vs {bpm}", g.bpm);
            // The anchor is a bar start: offset + whole bars.
            let bar = 4.0 * 60.0 / bpm;
            let err = ((g.anchor_secs - offset) / bar).rem_euclid(1.0);
            let err_s = err.min(1.0 - err) * bar;
            assert!(err_s < 0.005, "{bpm} BPM: anchor {} vs bar start at {offset} (off by {err_s:.3} s)", g.anchor_secs);
        }
    }

    #[test]
    fn rejects_silence() {
        let a = AudioData { sample_rate: SR, frames: vec![[0.0; 2]; SR as usize * 20] };
        assert!(detect_grid(&Envelope::compute(&a)).is_none());
    }

    #[test]
    fn cues_mark_sections() {
        // Intro (hats + pad) 16 bars, drop 32, break (pad only) 16, drop 32, outro (kick + hats) 16.
        let intro = vec![(false, true, false, true); 16];
        let brk = vec![(false, false, false, true); 16];
        let outro = vec![(true, true, false, false); 16];
        let bars: Vec<_> = [intro, full(32), brk, full(32), outro].concat();
        let bpm = 128.0;
        let a = track(bpm, 0.0, &bars);
        let env = Envelope::compute(&a);
        let grid = BeatGrid { bpm, anchor_secs: 0.0 };
        let cues = detect_cues(&a, &env, &grid, None);
        let bar = 4.0 * 60.0 / bpm;
        let at = |name: &str| cues.iter().filter(|c| c.name == name).map(|c| (c.secs / bar).round() as i64).collect::<Vec<_>>();
        let summary: Vec<String> = cues.iter().map(|c| format!("{}@{:.0}", c.name, c.secs / bar)).collect();
        assert_eq!(at("Intro"), vec![0], "{summary:?}");
        assert_eq!(at("Drop"), vec![16, 64], "{summary:?}");
        assert_eq!(at("Break"), vec![48], "{summary:?}");
        assert_eq!(at("Outro"), vec![96], "{summary:?}");
    }

    #[test]
    fn energy_is_in_range_and_follows_loudness() {
        let loud = track(128.0, 0.0, &full(32));
        let quiet = track(128.0, 0.0, &vec![(false, false, false, true); 32]);
        let e_loud = estimate_energy(&loud, &Envelope::compute(&loud), Some(128.0));
        let e_quiet = estimate_energy(&quiet, &Envelope::compute(&quiet), Some(128.0));
        assert!((1..=10).contains(&e_loud) && (1..=10).contains(&e_quiet));
        assert!(e_loud > e_quiet, "{e_loud} vs {e_quiet}");
    }

    #[test]
    fn refine_grid_measures_kicks() {
        // 48 bars of kicks, hats, bass at 124 BPM, the first beat at 0.5 s.
        let audio = track(124.0, 0.5, &[(true, true, true, false); 48]);
        let env = Envelope::compute(&audio);
        let spb = 60.0 / 124.0;
        let true_grid = BeatGrid { bpm: 124.0, anchor_secs: 0.5 };
        // A tagger's grid 60 ms late: moved onto the kicks.
        let late = refine_grid(&env, &BeatGrid { bpm: 124.0, anchor_secs: 0.56 }).expect("off by 60 ms");
        eprintln!("late -> {:?}", late);
        assert!((late.anchor_secs - 0.5).abs() < 0.003, "{late:?}");
        assert!((late.bpm - 124.0).abs() < 0.002, "{late:?}");
        // A grid a hair slow in tempo (drifting 30 ms over the track): its tempo is corrected.
        let n = 48.0 * 4.0;
        let slow = BeatGrid { bpm: 124.0 * (1.0 - 0.03 / (n * spb)), anchor_secs: 0.5 };
        let fixed = refine_grid(&env, &slow).expect("drifts 30 ms");
        eprintln!("slow -> {:?}", fixed);
        assert!((fixed.bpm - 124.0).abs() < 0.003, "{fixed:?}");
        assert!((fixed.secs_at_beat(n - 4.0) - true_grid.secs_at_beat(n - 4.0)).abs() < 0.004, "{fixed:?}");
        // The right grid stays.
        assert_eq!(refine_grid(&env, &true_grid), None);
    }
}
