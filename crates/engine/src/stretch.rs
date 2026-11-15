//! Realtime WSOLA time-stretcher / pitch-shifter used for key lock.
//!
//! The timeline (where the deck *is*) and the read rate (how fast each grain
//! is played back, i.e. the pitch) are independent. Each grain is a
//! Hann-windowed slice of the source centred on the current timeline
//! position; consecutive grains overlap by 50% and each new grain is nudged
//! by up to ±`search` frames to maximise waveform similarity with the natural
//! continuation of the previous grain (WSOLA), which removes the phasey
//! artefacts of plain granular stretching.

use crate::dsp::{hermite, Frame};

/// Random access into source audio (with loop wrapping handled by the
/// implementor).
pub trait Source {
    /// Integer-frame access; out-of-range returns silence.
    fn frame(&self, i: i64) -> Frame;

    /// Interpolated access.
    #[inline]
    fn at(&self, pos: f64) -> Frame {
        let i = pos.floor();
        let t = (pos - i) as f32;
        let i = i as i64;
        let a = self.frame(i - 1);
        let b = self.frame(i);
        let c = self.frame(i + 1);
        let d = self.frame(i + 2);
        [hermite(a[0], b[0], c[0], d[0], t), hermite(a[1], b[1], c[1], d[1], t)]
    }

    #[inline]
    fn mono(&self, i: i64) -> f32 {
        let f = self.frame(i);
        f[0] + f[1]
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Grain {
    start: f64,
    rate: f64,
    idx: usize,
    active: bool,
}

pub struct Stretcher {
    n: usize,
    hop: usize,
    window: Vec<f32>,
    grains: [Grain; 2],
    newest: usize,
    countdown: usize,
    fresh: bool,
    search: i64,
    corr_len: usize,
}

impl Stretcher {
    pub fn new(sample_rate: f32) -> Self {
        // ~42 ms grains at 48 kHz: good compromise for full mixes.
        let n = if sample_rate > 70000.0 { 4096 } else { 2048 };
        let window = (0..n)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos())
            .collect();
        Self {
            n,
            hop: n / 2,
            window,
            grains: [Grain::default(); 2],
            newest: 0,
            countdown: 0,
            fresh: true,
            search: (n / 4) as i64,
            corr_len: n / 2,
        }
    }

    pub fn latency_frames(&self) -> usize {
        0 // grains are centred on the timeline position
    }

    pub fn reset(&mut self) {
        self.grains = [Grain::default(); 2];
        self.countdown = 0;
        self.fresh = true;
    }

    fn best_offset<S: Source + ?Sized>(&self, src: &S, ideal: f64, target: f64, rate: f64) -> f64 {
        // Coarse search (stride 4 in offset, decimate correlation by 4), then refine.
        let corr = |off: i64, dec: usize| -> f32 {
            let mut dot = 0.0f32;
            let mut energy = 1e-9f32;
            let a0 = ideal + off as f64;
            let mut k = 0;
            while k < self.corr_len {
                let pa = (a0 + k as f64 * rate) as i64;
                let pb = (target + k as f64 * rate) as i64;
                let a = src.mono(pa);
                let b = src.mono(pb);
                dot += a * b;
                energy += a * a;
                k += dec;
            }
            dot / energy.sqrt()
        };
        // Start from "no shift" and only move for a genuinely better match, preferring the
        // smaller shift on ties. Otherwise silence (all scores equal) picked the most negative
        // offset every time, which made key-locked audio lag the playhead.
        let better = |c: f32, off: i64, best_c: f32, best: i64| c > best_c || (c == best_c && off.abs() < best.abs());
        let mut best = 0i64;
        let mut best_c = corr(0, 4);
        let mut off = -self.search;
        while off <= self.search {
            let c = corr(off, 4);
            if better(c, off, best_c, best) {
                best_c = c;
                best = off;
            }
            off += 4;
        }
        let coarse = best;
        best_c = corr(coarse, 2);
        for off in (coarse - 3)..=(coarse + 3) {
            if off == coarse {
                continue;
            }
            let c = corr(off, 2);
            if better(c, off, best_c, best) {
                best_c = c;
                best = off;
            }
        }
        best as f64
    }

    /// Render one output frame. `timeline` is the deck position in source
    /// frames, `read_rate` the per-output-frame source increment inside a grain.
    #[inline]
    pub fn tick<S: Source + ?Sized>(&mut self, src: &S, timeline: f64, read_rate: f64) -> Frame {
        if self.countdown == 0 {
            let slot = 1 - self.newest;
            if self.fresh {
                // Start with two perfectly aligned grains (one half-way
                // through) so output is at full level immediately.
                let a = timeline - self.hop as f64 * read_rate;
                self.grains[self.newest] = Grain { start: a, rate: read_rate, idx: self.hop, active: true };
                self.grains[slot] = Grain { start: timeline, rate: read_rate, idx: 0, active: true };
                self.fresh = false;
            } else {
                // The new grain starts *at* the timeline (the previous grain, now half-way through,
                // also reads the timeline at this moment). Starting it a hop earlier made the
                // output lag the playhead and cue markers by a hop (~21 ms) with key lock on.
                let ideal = timeline;
                let prev = self.grains[self.newest];
                let target = prev.start + (prev.idx as f64) * prev.rate;
                let off = self.best_offset(src, ideal, target, read_rate);
                self.grains[slot] = Grain { start: ideal + off, rate: read_rate, idx: 0, active: true };
            }
            self.newest = slot;
            self.countdown = self.hop;
        }
        self.countdown -= 1;

        let mut out = [0.0f32; 2];
        for g in self.grains.iter_mut() {
            if !g.active {
                continue;
            }
            let w = self.window[g.idx];
            let s = src.at(g.start + g.idx as f64 * g.rate);
            out[0] += s[0] * w;
            out[1] += s[1] * w;
            g.idx += 1;
            if g.idx >= self.n {
                g.active = false;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Vec2(Vec<Frame>);
    impl Source for Vec2 {
        fn frame(&self, i: i64) -> Frame {
            if i < 0 || i as usize >= self.0.len() {
                [0.0; 2]
            } else {
                self.0[i as usize]
            }
        }
    }

    fn zero_crossing_freq(x: &[f32], sr: f32) -> f32 {
        let mut zc = 0;
        for w in x.windows(2) {
            if (w[0] <= 0.0) != (w[1] <= 0.0) {
                zc += 1;
            }
        }
        zc as f32 / 2.0 / (x.len() as f32 / sr)
    }

    #[test]
    fn keylock_preserves_pitch_and_level() {
        let sr = 48000.0;
        let src = Vec2(
            (0..sr as usize * 4)
                .map(|i| {
                    let v = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr).sin() * 0.5;
                    [v, v]
                })
                .collect(),
        );
        let mut st = Stretcher::new(sr);
        // Play 25% faster with key lock: pitch must stay at 440 Hz.
        let mut pos = 0.0f64;
        let mut out = Vec::new();
        for _ in 0..(sr as usize * 2) {
            out.push(st.tick(&src, pos, 1.0)[0]);
            pos += 1.25;
        }
        let tail = &out[out.len() / 2..];
        let f = zero_crossing_freq(tail, sr);
        assert!((f - 440.0).abs() < 8.0, "freq {f}");
        let rms = (tail.iter().map(|v| v * v).sum::<f32>() / tail.len() as f32).sqrt();
        assert!((rms - 0.3535).abs() < 0.05, "rms {rms}");
        // Timeline advanced 1.25x.
        assert!((pos - sr as f64 * 2.0 * 1.25).abs() < 1.0);
    }

    #[test]
    fn pitch_shift_up_an_octave() {
        let sr = 48000.0;
        let src = Vec2(
            (0..sr as usize * 4)
                .map(|i| {
                    let v = (2.0 * std::f32::consts::PI * 220.0 * i as f32 / sr).sin();
                    [v, v]
                })
                .collect(),
        );
        let mut st = Stretcher::new(sr);
        let mut pos = 0.0f64;
        let mut out = Vec::new();
        for _ in 0..(sr as usize) {
            out.push(st.tick(&src, pos, 2.0)[0]);
            pos += 1.0;
        }
        let f = zero_crossing_freq(&out[out.len() / 2..], sr);
        assert!((f - 440.0).abs() < 10.0, "freq {f}");
    }
}
