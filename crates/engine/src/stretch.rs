//! Realtime time-stretch and pitch-shift for key lock and transposition, with Signalsmith
//! Stretch (a phase vocoder that keeps transients and polyphony clean; MIT).
//!
//! The deck owns the timeline (where it *is*) and the pitch (how fast the audio would be read at
//! that speed); the stretcher turns the source into audio at that position and pitch. Signalsmith
//! works in blocks and has latency (about 120 ms of analysis and synthesis), so the stretcher reads
//! ahead in the source, which is in memory: the input it is fed runs `lead` frames ahead of the
//! timeline, and what comes out is heard exactly at the timeline. Small drifts in speed are evened
//! out chunk by chunk (more or less input per chunk); jumps restart it from the new place, primed
//! with the audio before it so it sounds right at once. Loop wraps are followed without a restart:
//! the source wraps reads past the loop end, so the read-ahead simply continues in the loop.

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


/// Output frames rendered at a time.
const CHUNK: usize = 128;
/// Most input frames per chunk (8× speed, with room to catch up).
const MAX_IN: usize = CHUNK * 16;

pub struct Stretcher {
    st: signalsmith::Stretch,
    /// The current chunk (interleaved) and the next frame of it to play.
    out: Vec<f32>,
    next: usize,
    input: Vec<f32>,
    seek: Vec<f32>,
    /// The source frame the next input starts at (in the timeline's units, unwrapped: past a loop
    /// end the source wraps it).
    fed: f64,
    primed: bool,
    /// Timeline and advance when the current chunk was rendered (to notice loop wraps).
    last: (f64, f64),
}

impl Stretcher {
    pub fn new(sample_rate: f32) -> Self {
        let st = signalsmith::Stretch::new(sample_rate);
        let seek = vec![0.0; st.seek_frames() * 2];
        Self { st, out: vec![0.0; CHUNK * 2], next: CHUNK, input: vec![0.0; MAX_IN * 2], seek, fed: 0.0, primed: false, last: (0.0, 0.0) }
    }

    /// Start again from wherever the next tick says (after a jump).
    pub fn reset(&mut self) {
        self.primed = false;
        self.next = CHUNK;
    }

    /// The source frame it has read up to (it reads ahead of the timeline).
    pub fn read_ahead(&self) -> f64 {
        if self.primed {
            self.fed
        } else {
            f64::NEG_INFINITY
        }
    }

    /// Input frames ahead of the timeline at an advance of `advance` source frames per output
    /// frame: what goes in now is heard this much later.
    fn lead(&self, advance: f64) -> f64 {
        self.st.input_latency() as f64 + self.st.output_latency() as f64 * advance
    }

    fn feed<S: Source + ?Sized>(input: &mut [f32], src: &S, from: f64) {
        let from = from.round() as i64;
        for (k, f) in input.chunks_exact_mut(2).enumerate() {
            let s = src.frame(from + k as i64);
            f[0] = s[0];
            f[1] = s[1];
        }
    }

    /// Start at `timeline`: prime with the audio before it, then run the output latency through
    /// (discarded) so the first chunk is real audio, heard at the timeline.
    fn prime<S: Source + ?Sized>(&mut self, src: &S, timeline: f64, advance: f64, transpose: f32) {
        self.st.reset();
        self.st.set_transpose(transpose);
        let warm = self.st.output_latency();
        // (So that after the warm-up the input has reached `lead` past the frame before.)
        let start = timeline + self.st.input_latency() as f64 - advance;
        let n_seek = self.seek.len() / 2;
        Self::feed(&mut self.seek, src, start - n_seek as f64);
        self.st.seek(&mut self.seek, advance);
        self.fed = start.round();
        let mut left = warm;
        while left > 0 {
            let n_out = left.min(CHUNK);
            let n_in = ((self.fed + n_out as f64 * advance) - self.fed).round().clamp(0.0, MAX_IN as f64) as usize;
            Self::feed(&mut self.input[..n_in * 2], src, self.fed);
            self.st.process(&mut self.input[..n_in * 2], &mut self.out[..n_out * 2]);
            self.fed += n_in as f64;
            left -= n_out;
        }
        self.primed = true;
        self.last = (timeline - CHUNK as f64 * advance, advance);
    }

    /// Render one output frame. `timeline`: where the deck is (source frames); `advance`: how far
    /// it moves per output frame; `transpose`: the pitch, as the source frames read per output
    /// frame at that pitch (1 = as recorded at the output rate).
    #[inline]
    pub fn tick<S: Source + ?Sized>(&mut self, src: &S, timeline: f64, advance: f64, transpose: f64) -> Frame {
        if self.next == CHUNK {
            self.render(src, timeline, advance, transpose as f32);
        }
        let f = [self.out[2 * self.next], self.out[2 * self.next + 1]];
        self.next += 1;
        f
    }

    fn render<S: Source + ?Sized>(&mut self, src: &S, timeline: f64, advance: f64, transpose: f32) {
        self.next = 0;
        if self.primed {
            // A loop wrap (the timeline jumped back by whole loops): the read-ahead carries on in
            // the loop, at the same distance.
            let expected = self.last.0 + CHUNK as f64 * self.last.1;
            let jump = timeline - expected;
            if jump.abs() > 16.0 {
                self.fed += jump;
            }
        }
        // Where the input should have reached once this chunk's last frame is out.
        let target = timeline + (CHUNK - 1) as f64 * advance + self.lead(advance);
        let n_in = (target - self.fed).round();
        if !self.primed || !(0.0..=MAX_IN as f64).contains(&n_in) {
            self.prime(src, timeline, advance, transpose);
            return self.render(src, timeline, advance, transpose);
        }
        let n_in = n_in as usize;
        self.st.set_transpose(transpose);
        Self::feed(&mut self.input[..n_in * 2], src, self.fed);
        self.st.process(&mut self.input[..n_in * 2], &mut self.out);
        self.fed += n_in as f64;
        self.last = (timeline, advance);
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
            out.push(st.tick(&src, pos, 1.25, 1.0)[0]);
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
            out.push(st.tick(&src, pos, 1.0, 2.0)[0]);
            pos += 1.0;
        }
        let f = zero_crossing_freq(&out[out.len() / 2..], sr);
        assert!((f - 440.0).abs() < 10.0, "freq {f}");
    }
}
