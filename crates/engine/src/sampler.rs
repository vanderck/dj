//! Sampler deck: an eight-lane, clock-synced step sequencer / drum machine.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::dsp::{Frame, Rng};
use crate::track::AudioData;

pub const LANES: usize = 8;
pub const STEPS: usize = 16;
pub const PATTERNS: usize = 8;
const VOICES: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Direction {
    #[default]
    Forward,
    Reverse,
    PingPong,
    Random,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Rhythm {
    #[default]
    Normal,
    Dotted,
    Triplet,
}

impl Rhythm {
    /// Step length in beats (sixteenth notes and their variants).
    pub fn step_beats(self) -> f64 {
        match self {
            Rhythm::Normal => 0.25,
            Rhythm::Dotted => 0.375,
            Rhythm::Triplet => 0.25 * 2.0 / 3.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LaneParams {
    /// Attack, 0..1 → 0..500 ms.
    pub attack: f32,
    /// Hold, 0..1 → fraction of the sample before the release starts.
    pub hold: f32,
    /// Release, 0..1 → 0..2000 ms.
    pub release: f32,
    /// Release curve, 0 = linear, 1 = strongly exponential.
    pub curve: f32,
    pub pitch: f32,
    pub volume: f32,
    pub steps: u8,
    pub direction: Direction,
    pub rhythm: Rhythm,
    pub mono: bool,
    pub mute: bool,
}

impl Default for LaneParams {
    fn default() -> Self {
        Self {
            attack: 0.0,
            hold: 1.0,
            release: 0.1,
            curve: 0.5,
            pitch: 0.0,
            volume: 0.75,
            steps: 16,
            direction: Direction::Forward,
            rhythm: Rhythm::Normal,
            mono: true,
            mute: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub on: bool,
    pub velocity: f32,
}

impl Default for Step {
    fn default() -> Self {
        Self { on: false, velocity: 1.0 }
    }
}

pub type Pattern = [[Step; STEPS]; LANES];

#[derive(Clone, Copy, Debug, Default)]
struct Voice {
    active: bool,
    lane: u8,
    pos: f64,
    rate: f64,
    gain: f32,
    env: f32,
    attack_inc: f32,
    releasing: bool,
    release_mul: f32,
    release_lin: f32,
    hold_end: f64,
    /// Frames to wait before starting (sample-accurate triggering).
    delay: u32,
    age: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SamplerView {
    pub playing: bool,
    pub pattern: u8,
    pub current_step: [i8; LANES],
    /// −1 when idle, else 0..1 progress of the newest voice on the lane.
    pub sample_progress: [f32; LANES],
    pub lane_has_sample: [bool; LANES],
    pub lane_peak: [f32; LANES],
}

pub struct Sampler {
    out_rate: f64,
    pub lanes: [LaneParams; LANES],
    samples: [Option<Arc<AudioData>>; LANES],
    pub patterns: Box<[Pattern; PATTERNS]>,
    pub pattern: usize,
    pub swing: f32,
    pub global_pitch: f32,
    pub speed: f64,
    playing: bool,
    start_beat: f64,
    pending_start: Option<f64>,
    voices: [Voice; VOICES],
    age: u64,
    rng: Rng,
    current_step: [i8; LANES],
    lane_peak: [f32; LANES],
}

impl Sampler {
    pub fn new(out_rate: f64) -> Self {
        Self {
            out_rate,
            lanes: [LaneParams::default(); LANES],
            samples: Default::default(),
            patterns: Box::new([[[Step::default(); STEPS]; LANES]; PATTERNS]),
            pattern: 0,
            swing: 0.0,
            global_pitch: 0.0,
            speed: 1.0,
            playing: false,
            start_beat: 0.0,
            pending_start: None,
            voices: [Voice::default(); VOICES],
            age: 0,
            rng: Rng::new(0x5eed),
            current_step: [-1; LANES],
            lane_peak: [0.0; LANES],
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Swap a lane's sample; returns the old one for off-thread disposal.
    pub fn set_sample(&mut self, lane: usize, s: Option<Arc<AudioData>>) -> Option<Arc<AudioData>> {
        if lane >= LANES {
            return s;
        }
        for v in self.voices.iter_mut() {
            if v.active && v.lane as usize == lane {
                v.active = false;
            }
        }
        std::mem::replace(&mut self.samples[lane], s)
    }

    pub fn take_all_samples(&mut self, mut f: impl FnMut(Arc<AudioData>)) {
        for v in self.voices.iter_mut() {
            v.active = false;
        }
        for s in self.samples.iter_mut() {
            if let Some(a) = s.take() {
                f(a);
            }
        }
    }

    /// Start on the next quantize boundary of the clock (or now).
    pub fn play(&mut self, clock_beat: f64, quantize: Option<f64>) {
        if self.playing {
            return;
        }
        match quantize {
            Some(q) => {
                let at = ((clock_beat - 0.002) / q).ceil() * q;
                self.pending_start = Some(at.max(clock_beat));
            }
            None => {
                self.playing = true;
                self.start_beat = clock_beat;
            }
        }
    }

    pub fn stop(&mut self) {
        self.playing = false;
        self.pending_start = None;
        self.current_step = [-1; LANES];
    }

    pub fn toggle(&mut self, clock_beat: f64, quantize: Option<f64>) {
        if self.playing || self.pending_start.is_some() {
            self.stop()
        } else {
            self.play(clock_beat, quantize)
        }
    }

    /// Immediately trigger a lane (pads / finger drumming).
    pub fn trigger(&mut self, lane: usize, velocity: f32) {
        self.start_voice(lane, velocity, 0);
    }

    fn start_voice(&mut self, lane: usize, velocity: f32, delay: u32) {
        let Some(sample) = self.samples.get(lane).and_then(|s| s.as_ref()) else { return };
        let lp = self.lanes[lane];
        let len = sample.frames.len() as f64;
        let rate = sample.sample_rate as f64 / self.out_rate * 2f64.powf((lp.pitch + self.global_pitch) as f64 / 12.0);
        if lp.mono {
            for v in self.voices.iter_mut() {
                if v.active && v.lane as usize == lane && !v.releasing {
                    // Quick 3 ms choke instead of a hard cut.
                    v.releasing = true;
                    v.release_mul = (-1.0 / (0.003 * self.out_rate as f32)).exp();
                    v.release_lin = 0.0;
                }
            }
        }
        // Find a free voice, else steal the oldest.
        let idx = self
            .voices
            .iter()
            .position(|v| !v.active)
            .unwrap_or_else(|| self.voices.iter().enumerate().min_by_key(|(_, v)| v.age).map(|(i, _)| i).unwrap());
        self.age += 1;
        let attack_frames = lp.attack * 0.5 * self.out_rate as f32;
        let release_frames = (lp.release * 2.0 * self.out_rate as f32).max(16.0);
        // Blend of linear and exponential release, controlled by the curve.
        let release_mul = (-5.0 * lp.curve / release_frames).exp();
        let release_lin = (1.0 - lp.curve) / release_frames;
        self.voices[idx] = Voice {
            active: true,
            lane: lane as u8,
            pos: 0.0,
            rate,
            gain: velocity * lp.volume,
            env: if attack_frames < 1.0 { 1.0 } else { 0.0 },
            attack_inc: if attack_frames < 1.0 { 1.0 } else { 1.0 / attack_frames },
            releasing: false,
            release_mul,
            release_lin,
            hold_end: len * lp.hold.clamp(0.0, 1.0) as f64,
            delay,
            age: self.age,
        };
    }

    fn pattern_index(&mut self, lane: usize, k: i64) -> usize {
        let len = self.lanes[lane].steps.clamp(1, STEPS as u8) as i64;
        match self.lanes[lane].direction {
            Direction::Forward => k.rem_euclid(len) as usize,
            Direction::Reverse => (len - 1 - k.rem_euclid(len)) as usize,
            Direction::PingPong => {
                if len == 1 {
                    0
                } else {
                    let period = 2 * len - 2;
                    let p = k.rem_euclid(period);
                    (if p < len { p } else { period - p }) as usize
                }
            }
            Direction::Random => (self.rng.next_u64() % len as u64) as usize,
        }
    }

    /// Render a block, mixing into `out` (which is cleared first).
    pub fn process(&mut self, out: &mut [Frame], clock_beat: f64, clock_beats_per_frame: f64) {
        out.fill([0.0; 2]);
        let n = out.len();
        let b1 = clock_beat + n as f64 * clock_beats_per_frame;

        if let Some(at) = self.pending_start {
            if at < b1 {
                self.pending_start = None;
                self.playing = true;
                self.start_beat = at;
            }
        }

        if self.playing {
            let bpf = clock_beats_per_frame * self.speed;
            let r0 = (clock_beat - self.start_beat) * self.speed;
            let r1 = r0 + n as f64 * bpf;
            for lane in 0..LANES {
                let sb = self.lanes[lane].rhythm.step_beats();
                let swing = self.swing as f64 * sb * 0.5;
                let first = ((r0 - swing) / sb).floor() as i64 - 1;
                let mut k = first.max(0);
                loop {
                    let t = k as f64 * sb + if k % 2 == 1 { swing } else { 0.0 };
                    if t >= r1 {
                        break;
                    }
                    if t >= r0 {
                        let pi = self.pattern_index(lane, k);
                        self.current_step[lane] = pi as i8;
                        let step = self.patterns[self.pattern][lane][pi];
                        if step.on && !self.lanes[lane].mute {
                            let off = ((t - r0) / bpf - 1e-7).ceil().clamp(0.0, (n - 1) as f64) as u32;
                            self.start_voice(lane, step.velocity, off);
                        }
                    }
                    k += 1;
                }
            }
        }

        self.lane_peak = [0.0; LANES];
        for v in self.voices.iter_mut() {
            if !v.active {
                continue;
            }
            let Some(sample) = self.samples[v.lane as usize].as_ref() else {
                v.active = false;
                continue;
            };
            let frames = &sample.frames;
            let len = frames.len();
            let mut peak = 0.0f32;
            for o in out.iter_mut() {
                if v.delay > 0 {
                    v.delay -= 1;
                    continue;
                }
                let i = v.pos as usize;
                if i + 1 >= len {
                    v.active = false;
                    break;
                }
                let t = (v.pos - i as f64) as f32;
                let a = frames[i];
                let b = frames[i + 1];
                if !v.releasing {
                    if v.env < 1.0 {
                        v.env = (v.env + v.attack_inc).min(1.0);
                    }
                    if v.pos >= v.hold_end {
                        v.releasing = true;
                    }
                } else {
                    v.env = v.env * v.release_mul - v.release_lin;
                    if v.env <= 0.0005 {
                        v.active = false;
                        break;
                    }
                }
                let g = v.env * v.gain;
                let l = (a[0] + (b[0] - a[0]) * t) * g;
                let r = (a[1] + (b[1] - a[1]) * t) * g;
                o[0] += l;
                o[1] += r;
                peak = peak.max(l.abs()).max(r.abs());
                v.pos += v.rate;
            }
            let lp = &mut self.lane_peak[v.lane as usize];
            *lp = lp.max(peak);
        }
    }

    pub fn view(&self) -> SamplerView {
        let mut progress = [-1.0f32; LANES];
        let mut newest = [0u64; LANES];
        for v in self.voices.iter().filter(|v| v.active) {
            let l = v.lane as usize;
            if v.age >= newest[l] {
                newest[l] = v.age;
                let len = self.samples[l].as_ref().map(|s| s.frames.len()).unwrap_or(1) as f64;
                progress[l] = (v.pos / len) as f32;
            }
        }
        SamplerView {
            playing: self.playing,
            pattern: self.pattern as u8,
            current_step: self.current_step,
            sample_progress: progress,
            lane_has_sample: std::array::from_fn(|i| self.samples[i].is_some()),
            lane_peak: self.lane_peak,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn click() -> Arc<AudioData> {
        let mut frames = vec![[0.0; 2]; 4800];
        frames[0] = [1.0, 1.0];
        for f in frames.iter_mut().take(100) {
            *f = [1.0, 1.0];
        }
        Arc::new(AudioData { sample_rate: 48000, frames })
    }

    #[test]
    fn four_on_the_floor_lands_on_beats() {
        let sr = 48000.0;
        let mut s = Sampler::new(sr);
        s.set_sample(0, Some(click()));
        s.lanes[0].release = 0.0;
        for st in [0, 4, 8, 12] {
            s.patterns[0][0][st].on = true;
        }
        s.play(0.0, None);
        let bpm = 120.0;
        let bpf = bpm / 60.0 / sr;
        let mut onsets = Vec::new();
        let mut beat = 0.0;
        let mut frame = 0usize;
        let mut prev = 0.0;
        for _ in 0..(48000 * 2 / 256) {
            let mut buf = vec![[0.0; 2]; 256];
            s.process(&mut buf, beat, bpf);
            for (i, f) in buf.iter().enumerate() {
                if f[0] > 0.3 && prev <= 0.3 {
                    onsets.push(frame + i);
                }
                prev = f[0];
            }
            beat += 256.0 * bpf;
            frame += 256;
        }
        // 120 BPM → one beat every 24000 frames.
        assert_eq!(&onsets[..4], &[0, 24000, 48000, 72000][..onsets.len().min(4)]);
        assert!(onsets.len() >= 4);
    }

    #[test]
    fn pingpong_order() {
        let mut s = Sampler::new(48000.0);
        s.lanes[0].steps = 4;
        s.lanes[0].direction = Direction::PingPong;
        let seq: Vec<usize> = (0..8).map(|k| s.pattern_index(0, k)).collect();
        assert_eq!(seq, vec![0, 1, 2, 3, 2, 1, 0, 1]);
    }
}
