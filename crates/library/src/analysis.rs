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

/// Where the beats really land against `grid`, every 8 bars: (window centre time, offset in
/// seconds, clarity) for the windows with a clear beat, and how many windows there were. On the
/// kick band when it has a clear pulse (offbeat hats and claps are as strong as the beat in the
/// full band), else on the full band.
/// Measured beats: (time or beat, offset in seconds, clarity) per window, and how many windows.
type Offsets = (Vec<(f64, f64, f64)>, usize);

fn beat_offsets(env: &Envelope, grid: &BeatGrid) -> Option<Offsets> {
    let spb = grid.seconds_per_beat();
    if spb.is_nan() || spb <= 0.0 || env.duration < 30.0 {
        return None;
    }
    const WIN: usize = 32; // beats
    let step = 0.002;
    let reach = (0.3 * spb / step) as i64;
    let offsets: Vec<f64> = (-reach..=reach).map(|i| i as f64 * step).collect();
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
    Some(if low.len() * 2 >= windows && low.iter().map(|p| p.2).sum::<f64>() / low.len().max(1) as f64 > 1.8 { (low, windows) } else { measure(&env.onset) })
}

/// When a track's tempo really changes (a live drummer, a speed-up), no steady grid fits: the beats
/// wander off any straight line. Measured against `grid` (already the best steady fit), a tempo
/// map through where the beats are, every 8 bars, when they stray 12 ms or more from it; `None`
/// when the steady grid holds.
pub fn tempo_map(env: &Envelope, grid: &BeatGrid) -> Option<dj_engine::track::TempoMap> {
    let spb = grid.seconds_per_beat();
    if spb.is_nan() || spb <= 0.0 || env.duration < 30.0 {
        return None;
    }
    // Follow the beat window by window (4 bars), each searched near where the previous ones
    // predict it, so a tempo that wanders far from any straight line is still followed; then
    // once more with every window's bend taken from its neighbours. On the kick band when it
    // has a clear pulse.
    const WIN: usize = 16;
    let measure = |band: &[f32], b: f64, predict: f64, slope: f64, reach: f64| -> (f64, f64) {
        // The window's beats moved by `predict + d`, bending by `slope` (seconds per window).
        let score = |d: f64| -> f32 {
            (0..WIN).map(|k| env.at(band, grid.secs_at_beat(b + k as f64) + predict + d + slope * (k as f64 / WIN as f64 - 0.5))).sum::<f32>() / WIN as f32
        };
        let steps = (reach / 0.002) as i64;
        let scores: Vec<(f64, f32)> = (-steps..=steps).map(|i| (i as f64 * 0.002, score(i as f64 * 0.002))).collect();
        let mean = scores.iter().map(|s| s.1).sum::<f32>() / scores.len() as f32;
        let (d, best) = scores.iter().copied().fold((0.0, f32::MIN), |a, c| if c.1 > a.1 { c } else { a });
        (predict + d, (best / mean.max(1e-6)) as f64)
    };
    let follow = |band: &[f32]| -> (Vec<(f64, f64, f64)>, usize) {
        // The windows' first beats; following starts in the middle (where a steady grid fitted to
        // the whole track is closest to the beats) and works outwards both ways.
        let first = (-grid.anchor_secs / spb).ceil();
        let starts: Vec<f64> = (0..).map(|w| first + (w * WIN) as f64).take_while(|b| grid.secs_at_beat(b + WIN as f64) <= env.duration).collect();
        let windows = starts.len();
        if windows == 0 {
            return (Vec::new(), 0);
        }
        let mid = windows / 2;
        let mut found: Vec<Option<(f64, f64, f64)>> = vec![None; windows];
        for dir in [1i64, -1] {
            // Going out from the middle: the last offset found and its change per window.
            let (mut predict, mut slope) = match found[mid] {
                Some(p) => (p.1, 0.0),
                None => (0.0, 0.0),
            };
            let mut last: Option<(f64, f64)> = found[mid].map(|p| (p.0, p.1));
            let mut i = mid as i64 + if dir == 1 || found[mid].is_none() { 0 } else { -1 };
            while i >= 0 && (i as usize) < windows {
                let b = starts[i as usize];
                let reach = if last.is_none() { 0.3 } else { 0.12 } * spb;
                let (off, clarity) = measure(band, b, predict, slope, reach);
                if clarity > 1.4 {
                    let c = b + WIN as f64 / 2.0;
                    if let Some((lb, lo)) = last {
                        slope = (off - lo) * WIN as f64 / (c - lb).abs() * dir as f64;
                    }
                    found[i as usize] = Some((c, off, clarity));
                    last = Some((c, off));
                    predict = off + slope * dir as f64;
                } else {
                    predict += slope * dir as f64;
                }
                i += dir;
            }
        }
        let mut pts: Vec<(f64, f64, f64)> = found.into_iter().flatten().collect();
        // Then a few passes: each window re-measured with the bend its neighbours give (the first
        // windows were measured before the tempo's trend was known).
        for _ in 0..3 {
            let prev = pts.clone();
            for i in 0..pts.len() {
                let (lo, hi) = (prev[i.saturating_sub(1)], prev[(i + 1).min(prev.len() - 1)]);
                if hi.0 > lo.0 {
                    let slope = (hi.1 - lo.1) * WIN as f64 / (hi.0 - lo.0);
                    let (off, clarity) = measure(band, prev[i].0 - WIN as f64 / 2.0, prev[i].1, slope, 0.05 * spb);
                    pts[i] = (prev[i].0, off, clarity);
                }
            }
        }
        (pts, windows)
    };
    let (low, windows) = follow(&env.low_onset);
    let (pts, windows) = if low.len() * 3 >= windows * 2 && low.iter().map(|p| p.2).sum::<f64>() / low.len().max(1) as f64 > 1.8 { (low, windows) } else { follow(&env.onset) };
    if pts.len() < 6 || pts.len() * 3 < windows * 2 {
        return None; // not enough clear beats to follow
    }
    // A fill or a break can mislead a window or two. Robust local smoothing (LOWESS): each window
    // fitted by a small curve through its neighbours, re-weighted so windows far off the trend
    // stop counting; a real tempo curve stays, a fill doesn't pull it.
    let raw: Vec<f64> = pts.iter().map(|p| p.1 - ENVELOPE_LAG).collect();
    let off = lowess(&pts.iter().map(|p| p.0).collect::<Vec<_>>(), &raw, 3);
    // Only a sustained wander makes a map (12 ms or more off for 12 bars in a row); a steady
    // track keeps its steady grid, whatever a fill or two did.
    let longest = off.iter().fold((0, 0), |(run, best), o| if o.abs() >= 0.012 { (run + 1, best.max(run + 1)) } else { (0, best) }).1;
    if longest < 3 {
        return None;
    }
    // As few points as keep within 3 ms of the measured beats: a smooth tempo, not one that
    // jitters every few bars.
    let all: Vec<(f64, f64)> = pts.iter().zip(&off).map(|(p, o)| (p.0, *o)).collect();
    let mut keep = vec![false; all.len()];
    keep[0] = true;
    keep[all.len() - 1] = true;
    fn simplify(v: &[(f64, f64)], from: usize, to: usize, keep: &mut [bool]) {
        let ((b0, o0), (b1, o1)) = (v[from], v[to]);
        let far = (from + 1..to)
            .map(|i| (i, (v[i].1 - (o0 + (o1 - o0) * (v[i].0 - b0) / (b1 - b0))).abs()))
            .fold((0, 0.0), |a, c| if c.1 > a.1 { c } else { a });
        if far.1 > 0.003 {
            keep[far.0] = true;
            simplify(v, from, far.0, keep);
            simplify(v, far.0, to, keep);
        }
    }
    simplify(&all, 0, all.len() - 1, &mut keep);
    dj_engine::track::TempoMap::new(all.iter().zip(&keep).filter(|(_, k)| **k).map(|((b, o), _)| (*b, grid.secs_at_beat(*b) + o)).collect())
}

/// Robust local quadratic smoothing of `y` over `x` (LOWESS): each point fitted from its `half`
/// neighbours each side, with tricube distance weights, twice re-weighted (bisquare) so that
/// points far off the trend stop counting.
fn lowess(x: &[f64], y: &[f64], half: usize) -> Vec<f64> {
    let n = x.len();
    let mut robust = vec![1.0f64; n];
    let mut fit = y.to_vec();
    for round in 0..3 {
        for i in 0..n {
            let (lo, hi) = (i.saturating_sub(half), (i + half + 1).min(n));
            let reach = (lo..hi).map(|j| (x[j] - x[i]).abs()).fold(0.0, f64::max) * 1.0001 + 1e-9;
            // Weighted least squares for y ≈ a + b·d + c·d² (d = x − x[i]); the fit at x[i] is a.
            let mut m = [[0.0f64; 3]; 3];
            let mut v = [0.0f64; 3];
            for j in lo..hi {
                let d = x[j] - x[i];
                let t = (d.abs() / reach).min(1.0);
                let w = (1.0 - t * t * t).powi(3) * robust[j];
                let f = [1.0, d, d * d];
                for r in 0..3 {
                    for c in 0..3 {
                        m[r][c] += w * f[r] * f[c];
                    }
                    v[r] += w * f[r] * y[j];
                }
            }
            fit[i] = solve3(m, v).map_or(y[i], |s| s[0]);
        }
        if round == 2 {
            break;
        }
        // Points far off the fit (in units of the median miss) count less next round.
        let mut res: Vec<f64> = (0..n).map(|i| (y[i] - fit[i]).abs()).collect();
        let mut sorted = res.clone();
        sorted.sort_by(f64::total_cmp);
        let scale = (6.0 * sorted[n / 2]).max(0.002);
        for (r, w) in res.iter_mut().zip(robust.iter_mut()) {
            let u = (*r / scale).min(1.0);
            *w = (1.0 - u * u).powi(2);
        }
    }
    fit
}

/// Solve a 3×3 linear system (Cramer's rule); `None` when singular.
fn solve3(m: [[f64; 3]; 3], v: [f64; 3]) -> Option<[f64; 3]> {
    let det = |a: [[f64; 3]; 3]| {
        a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1]) - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0]) + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0])
    };
    let d = det(m);
    if d.abs() < 1e-12 {
        return None;
    }
    let mut out = [0.0; 3];
    for (c, o) in out.iter_mut().enumerate() {
        let mut a = m;
        for r in 0..3 {
            a[r][c] = v[r];
        }
        *o = det(a) / d;
    }
    Some(out)
}

/// [`refine_grid`], correcting when the grid is `tolerance` seconds off or more somewhere.
fn refine_grid_within(env: &Envelope, grid: &BeatGrid, tolerance: f64) -> Option<BeatGrid> {
    let (pts, windows) = beat_offsets(env, grid)?;
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

/// Cue points from a song's sections (All-In-One: start in seconds and label: "intro",
/// "verse", "chorus", "bridge", "break", "inst", "solo", "outro"), each on the grid's bar nearest
/// its start, named after it. Sections that repeat a label in a row are one. The cues detected
/// from the sound (`found`) name what the labels don't say: a section starting where a drop was
/// found (within a bar) is that drop, and drops between sections are kept. At most 8: the intro,
/// drops, choruses, breaks and outro first, then bridges, instrumentals and verses.
pub fn cues_from_sections(sections: &[(f64, &str)], grid: &BeatGrid, found: &[Cue]) -> Vec<Cue> {
    let spb = grid.seconds_per_beat();
    let on_bar = |t: f64| {
        let b = grid.beat_at(t);
        grid.secs_at_beat((b / 4.0).round() * 4.0).max(0.0)
    };
    let style = |label: &str| -> Option<(&'static str, [u8; 3], u8)> {
        Some(match label {
            "intro" => ("Intro", Section::Intro.color(), 7),
            "chorus" => ("Chorus", [0xec, 0x48, 0x99], 5),
            "break" => ("Break", Section::Break.color(), 5),
            "outro" => ("Outro", Section::Outro.color(), 5),
            "bridge" => ("Bridge", [0x14, 0xb8, 0xa6], 3),
            "inst" => ("Inst", Section::Phrase.color(), 2),
            "solo" => ("Solo", [0xea, 0xb3, 0x08], 2),
            "verse" => ("Verse", Section::Vocal.color(), 1),
            _ => return None,
        })
    };
    let drops: Vec<f64> = found.iter().filter(|c| c.name == "Drop").map(|c| c.secs).collect();
    let near_drop = |t: f64| drops.iter().any(|d| (d - t).abs() <= 4.0 * spb + 0.01);
    let mut cues: Vec<(Cue, u8)> = Vec::new();
    let mut last = "";
    for &(start, label) in sections {
        if label == last {
            continue;
        }
        last = label;
        let Some((name, color, rank)) = style(label) else { continue };
        let secs = on_bar(start);
        if cues.iter().any(|(c, _)| (c.secs - secs).abs() < 0.01) {
            continue;
        }
        let (name, color, rank) = if label != "intro" && near_drop(secs) { ("Drop", Section::Drop.color(), 6) } else { (name, color, rank) };
        cues.push((Cue { kind: CueKind::Hot, secs, end_secs: None, name: name.into(), color }, rank));
    }
    // Drops the sections don't start on.
    for d in &drops {
        if !cues.iter().any(|(c, _)| (c.secs - d).abs() <= 4.0 * spb + 0.01) {
            cues.push((Cue { kind: CueKind::Hot, secs: *d, end_secs: None, name: "Drop".into(), color: Section::Drop.color() }, 6));
        }
    }
    cues.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.secs.total_cmp(&b.0.secs)));
    cues.truncate(8);
    let mut cues: Vec<Cue> = cues.into_iter().map(|(c, _)| c).collect();
    cues.sort_by(|a, b| a.secs.total_cmp(&b.secs));
    cues
}

/// How to move a grid after [`check_beats`]: half a beat (on the offbeats) and whole beats (bars
/// on the wrong beat), unless hot cues on whole beats, most of them on a bar's first beat
/// (Mixed In Key's phrase starts), say the grid's bars are right. Returns (half, bars, cues
/// confirm the bars); beat 0 moves `half + bars` beats later.
pub fn grid_shift(check: &BeatCheck, beat_at: impl Fn(f64) -> f64, cues: &[Option<Cue>]) -> (f64, f64, bool) {
    let half = if check.offbeat { 0.5 } else { 0.0 };
    let on_beats: Vec<i64> = cues
        .iter()
        .flatten()
        .filter(|c| c.kind == CueKind::Hot && c.secs > 0.1)
        .map(|c| beat_at(c.secs) - half)
        .filter(|b| (b - b.round()).abs() < 0.1)
        .map(|b| b.round() as i64)
        .collect();
    let cues_say_bars = on_beats.len() >= 2 && on_beats.iter().filter(|b| b.rem_euclid(4) == 0).count() * 2 > on_beats.len();
    let bars = check.bar_shift.filter(|_| !cues_say_bars).unwrap_or(0) as f64;
    (half, bars, cues_say_bars)
}

/// How a beat grid fits the beats a model hears (All-In-One: beat times, and how surely each
/// starts a bar).
#[derive(Clone, Debug, PartialEq)]
pub struct BeatCheck {
    /// The share of the heard beats that fall on the grid's beats (within 40 ms).
    pub agree: f64,
    /// The grid's beats fall between the heard ones: move beat 0 half a beat later (`bar_shift`
    /// counts from there).
    pub offbeat: bool,
    /// The heard bars start this many beats after the grid's (1–3): move beat 0 that many beats
    /// later.
    pub bar_shift: Option<i64>,
}

/// Check a grid (`beat_at`: its beat at a time; `spb`: seconds per beat) against heard beats
/// (`(secs, downbeat 0..1)`). `None` with too few beats to tell.
pub fn check_beats(beat_at: impl Fn(f64) -> f64, spb: f64, beats: &[(f64, f32)]) -> Option<BeatCheck> {
    if beats.len() < 32 || spb <= 0.0 {
        return None;
    }
    let tol = 0.04 / spb;
    let resid = |b: f64| b - b.round();
    let on = beats.iter().filter(|(t, _)| resid(beat_at(*t)).abs() < tol).count();
    let half = beats.iter().filter(|(t, _)| resid(beat_at(*t) - 0.5).abs() < tol).count();
    let n = beats.len() as f64;
    let offbeat = half as f64 > 0.6 * n && (on as f64) < 0.2 * n;
    let shift = if offbeat { 0.5 } else { 0.0 };
    // Bars: the mean downbeat strength on each beat of the bar (beats on the grid).
    let (mut sum, mut count) = ([0.0f64; 4], [0usize; 4]);
    for (t, d) in beats {
        let b = beat_at(*t) - shift;
        if resid(b).abs() < tol {
            let k = (b.round() as i64).rem_euclid(4) as usize;
            sum[k] += *d as f64;
            count[k] += 1;
        }
    }
    let mean: Vec<f64> = (0..4).map(|k| if count[k] >= 8 { sum[k] / count[k] as f64 } else { 0.0 }).collect();
    let best = (0..4).max_by(|a, b| mean[*a].total_cmp(&mean[*b])).unwrap_or(0);
    let second = (0..4).filter(|k| *k != best).map(|k| mean[k]).fold(0.0, f64::max);
    // (All-In-One's downbeat activations peak around 0.5: what counts is how clearly one beat
    // of the bar stands out.)
    let sure = mean[best] >= 0.2 && mean[best] >= 2.0 * second;
    Some(BeatCheck { agree: on.max(half) as f64 / n, offbeat, bar_shift: (sure && best != 0).then_some(best as i64) })
}

/// A detected grid's tempo checked against heard beats (`(secs, downbeat 0..1)`): when they
/// keep a steady tempo (at least 85 % of the beat intervals within 5 % of their median) more
/// than 4 % from the grid's, so the detector took double, half or 3:2 of the beat, a grid on
/// the heard beats, with its bars where they hear downbeats (see [`check_beats`]).
///
/// On GiantSteps (Beatport's BPMs): of 66 tracks the detector got wrong, this fixed 25; of
/// the tracks it got right, it changed none. (Unsteady heard beats were wrong as often as not.)
pub fn tempo_from_beats(grid: &BeatGrid, beats: &[(f64, f32)]) -> Option<BeatGrid> {
    let g = grid_from_beats(beats)?;
    ((g.bpm / grid.bpm - 1.0).abs() >= 0.04).then_some(g)
}

/// A grid on heard beats (`(secs, downbeat 0..1)`): a straight line through them, its bars
/// where they hear downbeats; `None` when they're too few or not steady (see [`STEADY`]), or
/// the tempo is outside 60–200 BPM.
pub fn grid_from_beats(beats: &[(f64, f32)]) -> Option<BeatGrid> {
    let (med, steady) = steadiness(beats)?;
    if steady < STEADY {
        return None;
    }
    let ibi: Vec<f64> = beats.windows(2).map(|w| w[1].0 - w[0].0).collect();
    // Number the beats (a gap of two intervals is a beat not heard), then a straight line
    // through them, once more without the beats off it by more than 40 ms.
    let mut n = 0.0;
    let mut pts = vec![(0.0, beats[0].0)];
    for (i, w) in ibi.iter().zip(&beats[1..]) {
        n += (i / med).round().max(1.0);
        pts.push((n, w.0));
    }
    let fit = |pts: &[(f64, f64)]| -> Option<(f64, f64)> {
        let k = pts.len() as f64;
        let (mx, my) = (pts.iter().map(|p| p.0).sum::<f64>() / k, pts.iter().map(|p| p.1).sum::<f64>() / k);
        let sxx: f64 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
        let slope = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum::<f64>() / sxx;
        (sxx > 0.0 && slope > 0.0).then_some((my - slope * mx, slope))
    };
    let (a, spb) = fit(&pts)?;
    let near: Vec<(f64, f64)> = pts.iter().copied().filter(|(b, t)| (t - (a + b * spb)).abs() < 0.04).collect();
    let (a, spb) = if near.len() >= 32 { fit(&near)? } else { (a, spb) };
    let bpm = 60.0 / spb;
    if !(60.0..=200.0).contains(&bpm) {
        return None;
    }
    Some(bars_from_beats(BeatGrid { bpm, anchor_secs: a }, beats))
}

/// A grid from tags (its tempo right, but placed off the beats: neither half a beat nor whole
/// beats off, which [`check_beats`] settles) moved onto the heard beats: when it has under half
/// of them, and a grid on them ([`grid_from_beats`]) at its tempo (within 4 %) has 80 % or more.
/// `None` otherwise (the heard beats may be the ones that are wrong).
pub fn regrid_on_beats(grid: &BeatGrid, beats: &[(f64, f32)]) -> Option<BeatGrid> {
    let agree = |g: &BeatGrid| check_beats(|t| g.beat_at(t), g.seconds_per_beat(), beats).map_or(0.0, |c| c.agree);
    if agree(grid) >= 0.5 {
        return None;
    }
    let fitted = grid_from_beats(beats)?;
    ((fitted.bpm / grid.bpm - 1.0).abs() < 0.04 && agree(&fitted) >= 0.8).then_some(fitted)
}

/// Heard beats steady enough to take their tempo: this share of the beat intervals within 5 %
/// of their median.
const STEADY: f64 = 0.85;

/// The heard beats' median interval and the share of intervals within 5 % of it (`None` with
/// fewer than 32 beats).
fn steadiness(beats: &[(f64, f32)]) -> Option<(f64, f64)> {
    if beats.len() < 32 {
        return None;
    }
    let ibi: Vec<f64> = beats.windows(2).map(|w| w[1].0 - w[0].0).collect();
    let mut sorted = ibi.clone();
    sorted.sort_by(f64::total_cmp);
    let med = sorted[sorted.len() / 2];
    (med > 0.0).then(|| (med, ibi.iter().filter(|i| (*i / med - 1.0).abs() < 0.05).count() as f64 / ibi.len() as f64))
}

/// A grid's bars moved to where the heard beats have downbeats, then back by whole bars to the
/// start of the track.
fn bars_from_beats(g: BeatGrid, beats: &[(f64, f32)]) -> BeatGrid {
    let spb = g.seconds_per_beat();
    let a = match check_beats(|t| g.beat_at(t), spb, beats) {
        Some(c) => g.anchor_secs + (grid_shift(&c, |t| g.beat_at(t), &[]).1 + if c.offbeat { 0.5 } else { 0.0 }) * spb,
        None => g.anchor_secs,
    };
    let bar = 4.0 * spb;
    BeatGrid { bpm: g.bpm, anchor_secs: a - (a / bar).floor() * bar }
}

/// Whether the heard beats are there (at least 32) but not steady enough for
/// [`tempo_from_beats`]: then the drum stem's tempo is worth a look ([`tempo_from_drums`]).
pub fn beats_unsteady(beats: &[(f64, f32)]) -> bool {
    steadiness(beats).is_some_and(|(_, steady)| steady < STEADY)
}

/// A detected grid's tempo checked against the drum stem's (`drums`: [`detect_grid`] on the
/// drums alone), when the heard beats are there but unsteady (see [`beats_unsteady`]): a drum
/// tempo more than 4 % from the grid's, on the drums with bars where the heard beats have
/// downbeats. (Without the rest of the music, the detector takes the beat the drums play
/// rather than a bass line's or a vocal's.)
///
/// On GiantSteps: of 42 tracks still wrong after [`tempo_from_beats`], this fixed 5; on the 50
/// right ones, the drums alone gave another tempo once, where the heard beats were steady.
pub fn tempo_from_drums(grid: &BeatGrid, drums: &BeatGrid, beats: &[(f64, f32)]) -> Option<BeatGrid> {
    if !beats_unsteady(beats) || !(60.0..=200.0).contains(&drums.bpm) || (drums.bpm / grid.bpm - 1.0).abs() < 0.04 {
        return None;
    }
    Some(bars_from_beats(*drums, beats))
}

#[cfg(test)]
pub(crate) mod tests {

    #[test]
    fn a_tag_grid_off_the_beats_is_moved_onto_them() {
        // Beats at 124 BPM from 0.20 s (a downbeat every 4th); the tag grid at 124 BPM from 0.33 s
        // (a quarter beat off: neither half a beat nor whole beats).
        let spb = 60.0 / 124.0;
        let beats: Vec<(f64, f32)> = (0..200).map(|k| (0.20 + k as f64 * spb, if k % 4 == 0 { 0.6 } else { 0.05 })).collect();
        let tag = BeatGrid { bpm: 124.0, anchor_secs: 0.33 };
        let g = regrid_on_beats(&tag, &beats).expect("moved");
        assert!((g.bpm - 124.0).abs() < 0.05, "{}", g.bpm);
        let off = ((g.anchor_secs - 0.20) / spb).rem_euclid(4.0);
        assert!(off < 0.02 || off > 3.98, "on the downbeats: {}", g.anchor_secs);
        // On the beats already: left alone.
        assert!(regrid_on_beats(&BeatGrid { bpm: 124.0, anchor_secs: 0.20 }, &beats).is_none());
        // The heard beats at double tempo (wrong): left alone.
        let double: Vec<(f64, f32)> = (0..400).map(|k| (0.20 + k as f64 * spb / 2.0, 0.1)).collect();
        assert!(regrid_on_beats(&tag, &double).is_none());
    }
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

    /// Kicks (and offbeat hats) at the given beat times.
    fn kicks_at(times: &[f64], secs: f64) -> AudioData {
        let len = (secs * SR as f64) as usize;
        let mut frames = vec![[0.0f32; 2]; len];
        for (n, &bt) in times.iter().enumerate() {
            let s0 = (bt * SR as f64) as usize;
            let next = times.get(n + 1).copied().unwrap_or(bt + 0.5);
            let half = ((next - bt) / 2.0 * SR as f64) as usize;
            for k in 0..6000 {
                let Some(f) = frames.get_mut(s0 + k) else { break };
                let t = k as f32 / SR as f32;
                let mut v = 0.6 * (t * 50.0 * std::f32::consts::TAU).sin() * (-(k as f32) / 1500.0).exp();
                if k >= half && k < half + 1200 {
                    v += 0.1 * ((k * 7919 % 1000) as f32 / 500.0 - 1.0) * (-((k - half) as f32) / 300.0).exp();
                }
                f[0] += v;
                f[1] += v;
            }
        }
        AudioData { sample_rate: SR, frames }
    }

    #[test]
    fn a_wandering_tempo_gets_a_map_that_follows_the_beats() {
        // A drummer: 120 BPM, up to 123 by the middle, back to 120 (3 minutes).
        let dur = 180.0;
        let bpm_at = |t: f64| 120.0 + 3.0 * (std::f64::consts::PI * t / dur).sin();
        let mut times = vec![0.5];
        while *times.last().unwrap() < dur - 1.0 {
            let t = *times.last().unwrap();
            times.push(t + 60.0 / bpm_at(t));
        }
        let audio = kicks_at(&times, dur);
        let env = Envelope::compute(&audio);
        // The best steady grid (what a tagger gives, then dj's check).
        let steady = BeatGrid { bpm: (times.len() - 1) as f64 * 60.0 / (times[times.len() - 1] - times[0]), anchor_secs: 0.5 };
        let steady = refine_grid(&env, &steady).unwrap_or(steady);
        let worst_steady = times.iter().enumerate().map(|(n, t)| (steady.secs_at_beat(n as f64) - t).abs()).fold(0.0, f64::max);
        assert!(worst_steady > 0.03, "no steady grid fits: {worst_steady}");
        let map = tempo_map(&env, &steady).expect("a map");
        // Through the body of the track (the first and last 8 bars extrapolate), the map's beats
        // are within 8 ms of the kicks (a flam is heard from about 20).
        for (n, t) in times.iter().enumerate().skip(32).take(times.len() - 64) {
            let e = (map.secs_at_beat(n as f64) - t).abs();
            assert!(e < 0.008, "beat {n}: {:.1} ms off (steady grid: {:.1} ms)", e * 1000.0, (steady.secs_at_beat(n as f64) - t).abs() * 1000.0);
        }
        // A steady track gets none.
        let steady_track = track(124.0, 0.5, &[(true, true, true, false); 64]);
        let env = Envelope::compute(&steady_track);
        assert_eq!(tempo_map(&env, &BeatGrid { bpm: 124.0, anchor_secs: 0.5 }), None);
    }

    #[test]
    fn heard_beats_check_the_grid() {
        // 120 BPM; heard beats every 0.5 s from 0.25 s, bars starting on the 2nd of them.
        let heard: Vec<(f64, f32)> = (0..200).map(|k| (0.25 + 0.5 * k as f64, if k % 4 == 1 { 0.9 } else { 0.1 })).collect();
        let check = |anchor: f64| {
            let g = BeatGrid { bpm: 120.0, anchor_secs: anchor };
            check_beats(|t| g.beat_at(t), 0.5, &heard).unwrap()
        };
        // On the beats, bars from the first: the heard bars are a beat later.
        let c = check(0.25);
        assert_eq!((c.agree, c.offbeat, c.bar_shift), (1.0, false, Some(1)));
        // Already on the bars.
        assert_eq!(check(0.75).bar_shift, None);
        // Half a beat off: the beats fall between; the bars are still found.
        let c = check(0.5);
        assert!(c.offbeat && c.agree == 1.0, "{c:?}");
        assert_eq!(c.bar_shift, None, "half a beat later, beat 0 is at 0.75 s: a downbeat");
        let c = check(0.0);
        assert!(c.offbeat && c.bar_shift == Some(1), "beat 0 at 0.25 s after the move: {c:?}");
        // No clear downbeats: the bars are left alone.
        let flat: Vec<(f64, f32)> = heard.iter().map(|(t, _)| (*t, 0.3)).collect();
        let g = BeatGrid { bpm: 120.0, anchor_secs: 0.25 };
        assert_eq!(check_beats(|t| g.beat_at(t), 0.5, &flat).unwrap().bar_shift, None);
        assert_eq!(check_beats(|t| g.beat_at(t), 0.5, &heard[..10]), None, "too few to tell");
    }

    #[test]
    fn steady_heard_beats_correct_a_detected_tempo() {
        // 128 BPM heard from 0.3 s, at 10 ms frames, two beats missed; the detector said 85.3
        // (2:3 of it) or 64 (half).
        let spb = 60.0 / 128.0;
        // Bars start on the 2nd heard beat.
        let heard: Vec<(f64, f32)> = (0..200).filter(|k| *k != 50 && *k != 120).map(|k| (((0.3 + spb * k as f64) * 100.0).round() / 100.0, if k % 4 == 1 { 0.8 } else { 0.1 })).collect();
        for wrong in [85.33, 64.0] {
            let g = tempo_from_beats(&BeatGrid { bpm: wrong, anchor_secs: 0.1 }, &heard).expect("corrected");
            assert!((g.bpm - 128.0).abs() < 0.05, "{g:?}");
            assert!((g.beat_at(0.3 + spb * 37.0) - 36.0).abs() < 0.02, "on the heard beats and bars: {g:?}");
            assert!((g.anchor_secs - (0.3 + spb)).abs() < 0.01, "the first downbeat: {g:?}");
        }
        // The detector agrees (within 4 %): left alone.
        assert_eq!(tempo_from_beats(&BeatGrid { bpm: 127.0, anchor_secs: 0.3 }, &heard), None);
        // Beats that wander (every third interval 20 % long): not trusted.
        let mut t = 0.3;
        let wobbly: Vec<(f64, f32)> = (0..200)
            .map(|k| {
                t += if k % 3 == 0 { 1.2 * spb } else { spb };
                (t, 0.1)
            })
            .collect();
        assert_eq!(tempo_from_beats(&BeatGrid { bpm: 85.33, anchor_secs: 0.3 }, &wobbly), None);
    }

    #[test]
    fn the_drums_tempo_counts_when_the_heard_beats_wander() {
        // Heard beats around 150 BPM that wander (every third interval 20 % long), downbeats on
        // the 2nd; the detector said 120 on the mix, 150 on the drums.
        let spb = 60.0 / 150.0;
        let mut t = 0.2;
        let wobbly: Vec<(f64, f32)> = (0..200)
            .map(|k| {
                t += if k % 3 == 0 { 1.2 * spb } else { spb };
                (t, 0.1)
            })
            .collect();
        assert!(beats_unsteady(&wobbly));
        let mix = BeatGrid { bpm: 120.0, anchor_secs: 0.1 };
        let drums = BeatGrid { bpm: 150.0, anchor_secs: 1.9 };
        let g = tempo_from_drums(&mix, &drums, &wobbly).expect("the drums' tempo");
        assert_eq!(g.bpm, 150.0);
        assert!((g.beat_at(1.9) - g.beat_at(1.9).round()).abs() < 1e-9 && g.anchor_secs < 4.0 * spb, "on the drums' beats, from the first bar: {g:?}");
        // The drums agree with the mix (within 4 %), or say something out of range: left alone.
        assert_eq!(tempo_from_drums(&mix, &BeatGrid { bpm: 122.0, anchor_secs: 0.1 }, &wobbly), None);
        assert_eq!(tempo_from_drums(&mix, &BeatGrid { bpm: 240.0, anchor_secs: 0.1 }, &wobbly), None);
        // Steady heard beats are trusted over the drums (see tempo_from_beats); too few say nothing.
        let steady: Vec<(f64, f32)> = (0..200).map(|k| (0.1 + 0.5 * k as f64, 0.1)).collect();
        assert!(!beats_unsteady(&steady) && !beats_unsteady(&wobbly[..20]));
        assert_eq!(tempo_from_drums(&mix, &drums, &steady), None);
        assert_eq!(tempo_from_drums(&mix, &drums, &wobbly[..20]), None);
    }

    #[test]
    fn cues_come_from_the_sections() {
        // 120 BPM: bars every 2 s. A section a little off a bar starts on the nearest one.
        let g = BeatGrid { bpm: 120.0, anchor_secs: 0.0 };
        let secs = [(0.0, "intro"), (16.3, "verse"), (31.8, "verse"), (48.1, "chorus"), (64.0, "break"), (80.2, "chorus"), (112.0, "outro"), (127.0, "end")];
        let found = [Cue { kind: CueKind::Hot, secs: 48.0, end_secs: None, name: "Drop".into(), color: [0; 3] }];
        let cues = cues_from_sections(&secs, &g, &found);
        let got: Vec<(f64, &str)> = cues.iter().map(|c| (c.secs, c.name.as_str())).collect();
        assert_eq!(got, vec![(0.0, "Intro"), (16.0, "Verse"), (48.0, "Drop"), (64.0, "Break"), (80.0, "Chorus"), (112.0, "Outro")]);
        // More than 8 sections: verses give way first.
        let many: Vec<(f64, &str)> = (0..12).map(|k| (16.0 * k as f64, ["verse", "chorus"][k % 2])).collect();
        let cues = cues_from_sections(&many, &g, &[]);
        assert_eq!(cues.len(), 8);
        assert_eq!(cues.iter().filter(|c| c.name == "Chorus").count(), 6, "every chorus kept");
    }
}
