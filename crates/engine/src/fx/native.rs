//! Built-in effects. All tempo-aware effects lock to the master clock.

use std::f32::consts::{PI, TAU};

use serde::{Deserialize, Serialize};

use super::{Effect, FxCtx};
use crate::dsp::{db_to_gain, undenormal, Biquad, Frame, Smoother, Svf};

#[derive(Clone, Copy, Debug)]
pub enum ParamKind {
    Percent,
    /// Linear dB range.
    Db(f32, f32),
    /// Logarithmic frequency range.
    Hz(f32, f32),
    /// Linear millisecond range.
    Ms(f32, f32),
    /// Plain linear range with a unit suffix.
    Linear(f32, f32, &'static str),
    /// Stepped list of labels.
    Choice(&'static [&'static str]),
}

#[derive(Clone, Copy, Debug)]
pub struct ParamSpec {
    pub name: &'static str,
    pub default: f32,
    pub kind: ParamKind,
}

impl ParamSpec {
    const fn new(name: &'static str, default: f32, kind: ParamKind) -> Self {
        Self {
            name,
            default,
            kind,
        }
    }

    pub fn steps(&self) -> Option<usize> {
        match self.kind {
            ParamKind::Choice(c) => Some(c.len()),
            _ => None,
        }
    }

    pub fn format(&self, v: f32) -> String {
        let v = v.clamp(0.0, 1.0);
        match self.kind {
            ParamKind::Percent => format!("{:.0}%", v * 100.0),
            ParamKind::Db(a, b) => {
                let db = a + (b - a) * v;
                if db <= -59.9 {
                    "-inf dB".into()
                } else {
                    format!("{db:+.1} dB")
                }
            }
            ParamKind::Hz(a, b) => {
                let hz = log_map(a, b, v);
                if hz >= 1000.0 {
                    format!("{:.2} kHz", hz / 1000.0)
                } else {
                    format!("{hz:.0} Hz")
                }
            }
            ParamKind::Ms(a, b) => format!("{:.1} ms", a + (b - a) * v),
            ParamKind::Linear(a, b, unit) => format!("{:.2}{unit}", a + (b - a) * v),
            ParamKind::Choice(c) => c[choice_index(v, c.len())].to_string(),
        }
    }
}

#[inline]
pub fn log_map(a: f32, b: f32, v: f32) -> f32 {
    a * (b / a).powf(v.clamp(0.0, 1.0))
}

#[inline]
pub fn choice_index(v: f32, n: usize) -> usize {
    ((v.clamp(0.0, 1.0) * (n - 1) as f32).round() as usize).min(n - 1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NativeFx {
    Echo,
    Reverb,
    Flanger,
    Phaser,
    AutoFilter,
    Bitcrusher,
    TranceGate,
    BeatRepeat,
    Distortion,
    Compressor,
    Utility,
}

const NOTE_DIVS: &[&str] = &[
    "1/32", "1/16", "1/8", "1/8D", "1/4", "1/4D", "1/2", "1 bar", "2 bars",
];
const NOTE_BEATS: &[f64] = &[0.125, 0.25, 0.5, 0.75, 1.0, 1.5, 2.0, 4.0, 8.0];
const LFO_DIVS: &[&str] = &[
    "1/4", "1/2", "1 beat", "2 beats", "1 bar", "2 bars", "4 bars", "8 bars",
];
const LFO_BEATS: &[f64] = &[0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0];

impl NativeFx {
    pub const ALL: [NativeFx; 11] = [
        NativeFx::Echo,
        NativeFx::Reverb,
        NativeFx::Flanger,
        NativeFx::Phaser,
        NativeFx::AutoFilter,
        NativeFx::Bitcrusher,
        NativeFx::TranceGate,
        NativeFx::BeatRepeat,
        NativeFx::Distortion,
        NativeFx::Compressor,
        NativeFx::Utility,
    ];

    pub fn name(self) -> &'static str {
        match self {
            NativeFx::Echo => "Echo",
            NativeFx::Reverb => "Reverb",
            NativeFx::Flanger => "Flanger",
            NativeFx::Phaser => "Phaser",
            NativeFx::AutoFilter => "Auto Filter",
            NativeFx::Bitcrusher => "Bitcrusher",
            NativeFx::TranceGate => "Trance Gate",
            NativeFx::BeatRepeat => "Beat Repeat",
            NativeFx::Distortion => "Distortion",
            NativeFx::Compressor => "Compressor",
            NativeFx::Utility => "Utility",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            NativeFx::Echo => "echo",
            NativeFx::Reverb => "reverb",
            NativeFx::Flanger => "flanger",
            NativeFx::Phaser => "phaser",
            NativeFx::AutoFilter => "autofilter",
            NativeFx::Bitcrusher => "bitcrusher",
            NativeFx::TranceGate => "trancegate",
            NativeFx::BeatRepeat => "beatrepeat",
            NativeFx::Distortion => "distortion",
            NativeFx::Compressor => "compressor",
            NativeFx::Utility => "utility",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.id() == id)
    }

    pub fn category(self) -> &'static str {
        match self {
            NativeFx::Echo | NativeFx::Reverb => "Delay / Reverb",
            NativeFx::Flanger | NativeFx::Phaser => "Modulation",
            NativeFx::AutoFilter => "Filter",
            NativeFx::Bitcrusher | NativeFx::Distortion => "Distortion",
            NativeFx::TranceGate | NativeFx::BeatRepeat => "Rhythmic",
            NativeFx::Compressor | NativeFx::Utility => "Dynamics / Utility",
        }
    }

    pub fn params(self) -> &'static [ParamSpec] {
        match self {
            NativeFx::Echo => {
                #[allow(unused_imports)]
                use ParamKind::*;
                const P: &[ParamSpec] = &[
                    ParamSpec::new("Time", 4.0 / 8.0, Choice(NOTE_DIVS)),
                    ParamSpec::new("Feedback", 0.45, Percent),
                    ParamSpec::new("Tone", 0.5, Linear(-1.0, 1.0, "")),
                    ParamSpec::new("Ping-Pong", 1.0, Choice(&["Off", "On"])),
                    ParamSpec::new("Level", 0.6, Percent),
                ];
                P
            }
            NativeFx::Reverb => {
                #[allow(unused_imports)]
                use ParamKind::*;
                const P: &[ParamSpec] = &[
                    ParamSpec::new("Size", 0.7, Percent),
                    ParamSpec::new("Damping", 0.5, Percent),
                    ParamSpec::new("Width", 1.0, Percent),
                    ParamSpec::new("Level", 0.4, Percent),
                    ParamSpec::new("Low Cut", 0.2, Hz(20.0, 1000.0)),
                ];
                P
            }
            NativeFx::Flanger => {
                #[allow(unused_imports)]
                use ParamKind::*;
                const P: &[ParamSpec] = &[
                    ParamSpec::new("Rate", 4.0 / 7.0, Choice(LFO_DIVS)),
                    ParamSpec::new("Depth", 0.7, Percent),
                    ParamSpec::new("Feedback", 0.75, Linear(-0.95, 0.95, "")),
                    ParamSpec::new("Delay", 0.3, Ms(0.5, 10.0)),
                ];
                P
            }
            NativeFx::Phaser => {
                #[allow(unused_imports)]
                use ParamKind::*;
                const P: &[ParamSpec] = &[
                    ParamSpec::new("Rate", 4.0 / 7.0, Choice(LFO_DIVS)),
                    ParamSpec::new("Depth", 0.8, Percent),
                    ParamSpec::new("Feedback", 0.5, Percent),
                    ParamSpec::new("Stages", 1.0 / 3.0, Choice(&["2", "4", "6", "8"])),
                ];
                P
            }
            NativeFx::AutoFilter => {
                #[allow(unused_imports)]
                use ParamKind::*;
                const P: &[ParamSpec] = &[
                    ParamSpec::new("Cutoff", 0.6, Hz(40.0, 18000.0)),
                    ParamSpec::new("Resonance", 0.4, Percent),
                    ParamSpec::new("Mode", 0.0, Choice(&["Low-pass", "Band-pass", "High-pass"])),
                    ParamSpec::new("LFO Depth", 0.5, Percent),
                    ParamSpec::new("LFO Rate", 2.0 / 7.0, Choice(LFO_DIVS)),
                    ParamSpec::new(
                        "LFO Shape",
                        0.0,
                        Choice(&["Sine", "Triangle", "Saw Down", "Square"]),
                    ),
                ];
                P
            }
            NativeFx::Bitcrusher => {
                #[allow(unused_imports)]
                use ParamKind::*;
                const P: &[ParamSpec] = &[
                    ParamSpec::new("Bits", 0.5, Linear(2.0, 16.0, " bit")),
                    ParamSpec::new("Downsample", 0.2, Linear(1.0, 32.0, "x")),
                ];
                P
            }
            NativeFx::TranceGate => {
                #[allow(unused_imports)]
                use ParamKind::*;
                const P: &[ParamSpec] = &[
                    ParamSpec::new("Division", 1.0 / 8.0, Choice(NOTE_DIVS)),
                    ParamSpec::new("Depth", 1.0, Percent),
                    ParamSpec::new("Duty", 0.5, Percent),
                    ParamSpec::new("Smooth", 0.2, Ms(0.5, 30.0)),
                    ParamSpec::new(
                        "Pattern",
                        0.0,
                        Choice(&["Straight", "Offbeat", "Tresillo", "Skip", "Build"]),
                    ),
                ];
                P
            }
            NativeFx::BeatRepeat => {
                #[allow(unused_imports)]
                use ParamKind::*;
                const P: &[ParamSpec] = &[
                    ParamSpec::new("Slice", 2.0 / 8.0, Choice(NOTE_DIVS)),
                    ParamSpec::new("Decay", 0.0, Percent),
                    ParamSpec::new("Pitch", 0.5, Linear(-12.0, 12.0, " st")),
                ];
                P
            }
            NativeFx::Distortion => {
                #[allow(unused_imports)]
                use ParamKind::*;
                const P: &[ParamSpec] = &[
                    ParamSpec::new("Drive", 0.3, Db(0.0, 36.0)),
                    ParamSpec::new("Tone", 0.8, Hz(800.0, 18000.0)),
                    ParamSpec::new("Output", 0.75, Db(-24.0, 6.0)),
                    ParamSpec::new("Shape", 0.0, Choice(&["Soft", "Hard", "Fold"])),
                ];
                P
            }
            NativeFx::Compressor => {
                #[allow(unused_imports)]
                use ParamKind::*;
                const P: &[ParamSpec] = &[
                    ParamSpec::new("Threshold", 0.6, Db(-48.0, 0.0)),
                    ParamSpec::new("Ratio", 0.2, Linear(1.0, 20.0, ":1")),
                    ParamSpec::new("Attack", 0.1, Ms(0.1, 100.0)),
                    ParamSpec::new("Release", 0.2, Ms(10.0, 1000.0)),
                    ParamSpec::new("Makeup", 0.0, Db(0.0, 24.0)),
                ];
                P
            }
            NativeFx::Utility => {
                #[allow(unused_imports)]
                use ParamKind::*;
                const P: &[ParamSpec] = &[
                    ParamSpec::new("Gain", 60.0 / 72.0, Db(-60.0, 12.0)),
                    ParamSpec::new("Width", 0.5, Linear(0.0, 2.0, "x")),
                    ParamSpec::new("Pan", 0.5, Linear(-1.0, 1.0, "")),
                    ParamSpec::new("Bass Mono", 0.0, Choice(&["Off", "On"])),
                ];
                P
            }
        }
    }

    pub fn create(self, sample_rate: f32) -> Box<dyn Effect> {
        let mut fx: Box<dyn Effect> = match self {
            NativeFx::Echo => Box::new(Echo::new(sample_rate)),
            NativeFx::Reverb => Box::new(Reverb::new(sample_rate)),
            NativeFx::Flanger => Box::new(Flanger::new(sample_rate)),
            NativeFx::Phaser => Box::new(Phaser::new(sample_rate)),
            NativeFx::AutoFilter => Box::new(AutoFilter::new(sample_rate)),
            NativeFx::Bitcrusher => Box::new(Bitcrusher::default()),
            NativeFx::TranceGate => Box::new(TranceGate::new(sample_rate)),
            NativeFx::BeatRepeat => Box::new(BeatRepeat::new(sample_rate)),
            NativeFx::Distortion => Box::new(Distortion::new(sample_rate)),
            NativeFx::Compressor => Box::new(Compressor::new(sample_rate)),
            NativeFx::Utility => Box::new(Utility::new(sample_rate)),
        };
        for (i, p) in self.params().iter().enumerate() {
            fx.set_param(i, p.default);
        }
        fx
    }
}

fn lfo_phase(ctx: &FxCtx, i: usize, cycle_beats: f64) -> f32 {
    let b = ctx.beat + i as f64 * ctx.beats_per_frame;
    (b / cycle_beats).rem_euclid(1.0) as f32
}

// ---------------------------------------------------------------- Echo

struct Echo {
    sr: f32,
    buf: Vec<Frame>,
    pos: usize,
    beats: f64,
    time: Smoother,
    feedback: f32,
    tone: f32,
    ping: bool,
    level: f32,
    lp: Biquad,
    hp: Biquad,
}

impl Echo {
    fn new(sr: f32) -> Self {
        Self {
            sr,
            buf: vec![[0.0; 2]; (sr * 8.5) as usize],
            pos: 0,
            beats: 1.0,
            time: Smoother::new(0.0, 120.0, sr),
            feedback: 0.4,
            tone: 0.0,
            ping: true,
            level: 0.6,
            lp: Biquad::lowpass(sr, 18000.0, 0.7),
            hp: Biquad::highpass(sr, 20.0, 0.7),
        }
    }

    fn update_tone(&mut self) {
        // Negative tone darkens the repeats, positive thins them out.
        let lp = if self.tone < 0.0 {
            log_map(18000.0, 1200.0, -self.tone)
        } else {
            18000.0
        };
        let hp = if self.tone > 0.0 {
            log_map(20.0, 1500.0, self.tone)
        } else {
            20.0
        };
        self.lp.set_lowpass(self.sr, lp, 0.7);
        self.hp.set_highpass(self.sr, hp, 0.7);
    }
}

impl Effect for Echo {
    fn set_param(&mut self, i: usize, v: f32) {
        match i {
            0 => self.beats = NOTE_BEATS[choice_index(v, NOTE_BEATS.len())],
            1 => self.feedback = v * 0.95,
            2 => {
                self.tone = v * 2.0 - 1.0;
                self.update_tone();
            }
            3 => self.ping = v >= 0.5,
            4 => self.level = v,
            _ => {}
        }
    }

    fn process(&mut self, buf: &mut [Frame], ctx: &FxCtx) {
        let n = self.buf.len();
        let target = (self.beats * ctx.frames_per_beat()).clamp(16.0, (n - 4) as f64) as f32;
        if self.time.current == 0.0 {
            self.time.snap(target);
        }
        self.time.set(target);
        for f in buf.iter_mut() {
            let d = self.time.next();
            let rp = self.pos as f32 + n as f32 - d;
            let i0 = rp as usize % n;
            let i1 = (i0 + 1) % n;
            let t = rp.fract();
            let a = self.buf[i0];
            let b = self.buf[i1];
            let wet = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
            let fb = self.hp.tick_frame(self.lp.tick_frame(wet));
            let input = *f;
            let w = if self.ping {
                // Cross-feed for ping-pong; mono-sum input enters left only.
                [
                    undenormal((input[0] + input[1]) * 0.5 + fb[1] * self.feedback),
                    undenormal(fb[0] * self.feedback),
                ]
            } else {
                [
                    undenormal(input[0] + fb[0] * self.feedback),
                    undenormal(input[1] + fb[1] * self.feedback),
                ]
            };
            self.buf[self.pos] = [w[0].clamp(-4.0, 4.0), w[1].clamp(-4.0, 4.0)];
            self.pos = (self.pos + 1) % n;
            f[0] = input[0] + wet[0] * self.level;
            f[1] = input[1] + wet[1] * self.level;
        }
    }

    fn reset(&mut self) {
        self.buf.fill([0.0; 2]);
    }
}

// ---------------------------------------------------------------- Reverb (Freeverb topology)

struct Comb {
    buf: Vec<f32>,
    pos: usize,
    store: f32,
}

impl Comb {
    fn new(len: usize) -> Self {
        Self {
            buf: vec![0.0; len],
            pos: 0,
            store: 0.0,
        }
    }

    #[inline]
    fn tick(&mut self, x: f32, feedback: f32, damp: f32) -> f32 {
        let out = self.buf[self.pos];
        self.store = undenormal(out * (1.0 - damp) + self.store * damp);
        self.buf[self.pos] = x + self.store * feedback;
        self.pos = (self.pos + 1) % self.buf.len();
        out
    }
}

struct Allpass {
    buf: Vec<f32>,
    pos: usize,
}

impl Allpass {
    fn new(len: usize) -> Self {
        Self {
            buf: vec![0.0; len],
            pos: 0,
        }
    }

    #[inline]
    fn tick(&mut self, x: f32) -> f32 {
        let b = self.buf[self.pos];
        let out = b - x;
        self.buf[self.pos] = undenormal(x + b * 0.5);
        self.pos = (self.pos + 1) % self.buf.len();
        out
    }
}

struct Reverb {
    sr: f32,
    combs: [Vec<Comb>; 2],
    aps: [Vec<Allpass>; 2],
    room: f32,
    damp: f32,
    width: f32,
    level: f32,
    hp: Biquad,
}

impl Reverb {
    fn new(sr: f32) -> Self {
        const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
        const APS: [usize; 4] = [556, 441, 341, 225];
        let s = sr / 44100.0;
        let mk = |spread: usize| -> (Vec<Comb>, Vec<Allpass>) {
            (
                COMBS
                    .iter()
                    .map(|&l| Comb::new(((l + spread) as f32 * s) as usize))
                    .collect(),
                APS.iter()
                    .map(|&l| Allpass::new(((l + spread) as f32 * s) as usize))
                    .collect(),
            )
        };
        let (cl, al) = mk(0);
        let (cr, ar) = mk(23);
        Self {
            sr,
            combs: [cl, cr],
            aps: [al, ar],
            room: 0.84,
            damp: 0.2,
            width: 1.0,
            level: 0.4,
            hp: Biquad::highpass(sr, 100.0, 0.7),
        }
    }
}

impl Effect for Reverb {
    fn set_param(&mut self, i: usize, v: f32) {
        match i {
            0 => self.room = 0.7 + v * 0.28,
            1 => self.damp = v * 0.4,
            2 => self.width = v,
            3 => self.level = v,
            4 => self.hp.set_highpass(self.sr, log_map(20.0, 1000.0, v), 0.7),
            _ => {}
        }
    }

    fn process(&mut self, buf: &mut [Frame], _ctx: &FxCtx) {
        let g = 0.015;
        let w1 = self.width * 0.5 + 0.5;
        let w2 = (1.0 - self.width) * 0.5;
        for f in buf.iter_mut() {
            let input = self.hp.tick_frame(*f);
            let x = (input[0] + input[1]) * g;
            let mut out = [0.0f32; 2];
            for ch in 0..2 {
                let mut acc = 0.0;
                for c in self.combs[ch].iter_mut() {
                    acc += c.tick(x, self.room, self.damp);
                }
                for a in self.aps[ch].iter_mut() {
                    acc = a.tick(acc);
                }
                out[ch] = acc;
            }
            let l = out[0] * w1 + out[1] * w2;
            let r = out[1] * w1 + out[0] * w2;
            f[0] += l * self.level * 3.0;
            f[1] += r * self.level * 3.0;
        }
    }

    fn reset(&mut self) {
        for ch in 0..2 {
            for c in self.combs[ch].iter_mut() {
                c.buf.fill(0.0);
                c.store = 0.0;
            }
            for a in self.aps[ch].iter_mut() {
                a.buf.fill(0.0);
            }
        }
    }
}

// ---------------------------------------------------------------- Flanger

struct Flanger {
    sr: f32,
    buf: Vec<Frame>,
    pos: usize,
    rate: f64,
    depth: f32,
    feedback: f32,
    delay_ms: f32,
}

impl Flanger {
    fn new(sr: f32) -> Self {
        Self {
            sr,
            buf: vec![[0.0; 2]; (sr * 0.03) as usize],
            pos: 0,
            rate: 4.0,
            depth: 0.7,
            feedback: 0.7,
            delay_ms: 3.0,
        }
    }
}

impl Effect for Flanger {
    fn set_param(&mut self, i: usize, v: f32) {
        match i {
            0 => self.rate = LFO_BEATS[choice_index(v, LFO_BEATS.len())],
            1 => self.depth = v,
            2 => self.feedback = -0.95 + v * 1.9,
            3 => self.delay_ms = 0.5 + v * 9.5,
            _ => {}
        }
    }

    fn process(&mut self, buf: &mut [Frame], ctx: &FxCtx) {
        let n = self.buf.len();
        let base = self.delay_ms * 0.001 * self.sr;
        for (i, f) in buf.iter_mut().enumerate() {
            let ph = lfo_phase(ctx, i, self.rate);
            let mut out = [0.0; 2];
            for ch in 0..2 {
                let p = ph + ch as f32 * 0.25;
                let lfo = 0.5 - 0.5 * (TAU * p).cos();
                let d = (base * (1.0 + 2.0 * self.depth * lfo)).clamp(1.0, (n - 2) as f32);
                let rp = self.pos as f32 + n as f32 - d;
                let i0 = rp as usize % n;
                let t = rp.fract();
                let s = self.buf[i0][ch] + (self.buf[(i0 + 1) % n][ch] - self.buf[i0][ch]) * t;
                self.buf[self.pos][ch] = undenormal(f[ch] + s * self.feedback);
                out[ch] = (f[ch] + s) * 0.6;
            }
            self.pos = (self.pos + 1) % n;
            *f = out;
        }
    }
}

// ---------------------------------------------------------------- Phaser

struct Phaser {
    sr: f32,
    state: [[f32; 8]; 2],
    last: [f32; 2],
    rate: f64,
    depth: f32,
    feedback: f32,
    stages: usize,
}

impl Phaser {
    fn new(sr: f32) -> Self {
        Self {
            sr,
            state: [[0.0; 8]; 2],
            last: [0.0; 2],
            rate: 4.0,
            depth: 0.8,
            feedback: 0.5,
            stages: 4,
        }
    }
}

impl Effect for Phaser {
    fn set_param(&mut self, i: usize, v: f32) {
        match i {
            0 => self.rate = LFO_BEATS[choice_index(v, LFO_BEATS.len())],
            1 => self.depth = v,
            2 => self.feedback = v * 0.9,
            3 => self.stages = [2, 4, 6, 8][choice_index(v, 4)],
            _ => {}
        }
    }

    fn process(&mut self, buf: &mut [Frame], ctx: &FxCtx) {
        for (i, f) in buf.iter_mut().enumerate() {
            let ph = lfo_phase(ctx, i, self.rate);
            for ch in 0..2 {
                let lfo = 0.5 - 0.5 * (TAU * (ph + ch as f32 * 0.1)).cos();
                let fc = log_map(200.0, 200.0 + 6000.0 * self.depth.max(0.01), lfo);
                let t = (PI * fc / self.sr).tan();
                let a = (t - 1.0) / (t + 1.0);
                let mut x = f[ch] + self.last[ch] * self.feedback;
                for s in 0..self.stages {
                    let y = a * x + self.state[ch][s];
                    self.state[ch][s] = undenormal(x - a * y);
                    x = y;
                }
                self.last[ch] = undenormal(x);
                f[ch] = (f[ch] + x) * 0.6;
            }
        }
    }
}

// ---------------------------------------------------------------- Auto filter

struct AutoFilter {
    sr: f32,
    svf: Svf,
    cutoff: f32,
    res: f32,
    mode: usize,
    depth: f32,
    rate: f64,
    shape: usize,
}

impl AutoFilter {
    fn new(sr: f32) -> Self {
        Self {
            sr,
            svf: Svf::default(),
            cutoff: 0.6,
            res: 0.4,
            mode: 0,
            depth: 0.5,
            rate: 2.0,
            shape: 0,
        }
    }
}

impl Effect for AutoFilter {
    fn set_param(&mut self, i: usize, v: f32) {
        match i {
            0 => self.cutoff = v,
            1 => self.res = v,
            2 => self.mode = choice_index(v, 3),
            3 => self.depth = v,
            4 => self.rate = LFO_BEATS[choice_index(v, LFO_BEATS.len())],
            5 => self.shape = choice_index(v, 4),
            _ => {}
        }
    }

    fn process(&mut self, buf: &mut [Frame], ctx: &FxCtx) {
        for (i, f) in buf.iter_mut().enumerate() {
            if i % 16 == 0 {
                let p = lfo_phase(ctx, i, self.rate);
                let lfo = match self.shape {
                    0 => (TAU * p).sin(),
                    1 => 1.0 - 4.0 * (p - 0.5).abs(),
                    2 => 1.0 - 2.0 * p,
                    _ => {
                        if p < 0.5 {
                            1.0
                        } else {
                            -1.0
                        }
                    }
                };
                let v = (self.cutoff + lfo * self.depth * 0.5).clamp(0.0, 1.0);
                self.svf
                    .set(self.sr, log_map(40.0, 18000.0, v), self.res * 0.9);
            }
            for ch in 0..2 {
                let o = self.svf.tick(ch, f[ch]);
                f[ch] = match self.mode {
                    0 => o.low,
                    1 => o.band,
                    _ => o.high,
                };
            }
        }
    }

    fn reset(&mut self) {
        self.svf.reset();
    }
}

// ---------------------------------------------------------------- Bitcrusher

#[derive(Default)]
struct Bitcrusher {
    bits: f32,
    down: f32,
    counter: f32,
    hold: Frame,
}

impl Effect for Bitcrusher {
    fn set_param(&mut self, i: usize, v: f32) {
        match i {
            0 => self.bits = 2.0 + v * 14.0,
            1 => self.down = 1.0 + v * 31.0,
            _ => {}
        }
    }

    fn process(&mut self, buf: &mut [Frame], _ctx: &FxCtx) {
        let levels = 2f32.powf(self.bits - 1.0);
        for f in buf.iter_mut() {
            self.counter += 1.0;
            if self.counter >= self.down {
                self.counter -= self.down;
                self.hold = [
                    (f[0] * levels).round() / levels,
                    (f[1] * levels).round() / levels,
                ];
            }
            *f = self.hold;
        }
    }
}

// ---------------------------------------------------------------- Trance gate

struct TranceGate {
    sr: f32,
    div: f64,
    depth: f32,
    duty: f32,
    smooth: Smoother,
    pattern: usize,
}

impl TranceGate {
    fn new(sr: f32) -> Self {
        Self {
            sr,
            div: 0.25,
            depth: 1.0,
            duty: 0.5,
            smooth: Smoother::new(1.0, 5.0, sr),
            pattern: 0,
        }
    }
}

const GATE_PATTERNS: [u16; 5] = [
    0xFFFF,                // straight
    0b0101_0101_0101_0101, // offbeat (steps 1,3,5... when counting from 0)
    0b0100_1001_0100_1001, // tresillo-ish (3-3-2)
    0b1011_0110_1101_1011, // skip
    0b1111_1111_0101_0001, // build (sparse to dense, LSB first)
];

impl Effect for TranceGate {
    fn set_param(&mut self, i: usize, v: f32) {
        match i {
            0 => self.div = NOTE_BEATS[choice_index(v, NOTE_BEATS.len())],
            1 => self.depth = v,
            2 => self.duty = 0.05 + v * 0.9,
            3 => self.smooth.set_time(0.5 + v * 29.5, self.sr),
            4 => self.pattern = choice_index(v, GATE_PATTERNS.len()),
            _ => {}
        }
    }

    fn process(&mut self, buf: &mut [Frame], ctx: &FxCtx) {
        let pat = GATE_PATTERNS[self.pattern];
        for (i, f) in buf.iter_mut().enumerate() {
            let b = ctx.beat + i as f64 * ctx.beats_per_frame;
            let step = (b / self.div).floor();
            let phase = (b / self.div - step) as f32;
            let bit = (step.rem_euclid(16.0)) as u32;
            let on = (pat >> bit) & 1 == 1 && phase < self.duty;
            self.smooth.set(if on { 1.0 } else { 1.0 - self.depth });
            let g = self.smooth.next();
            f[0] *= g;
            f[1] *= g;
        }
    }
}

// ---------------------------------------------------------------- Beat repeat

struct BeatRepeat {
    buf: Vec<Frame>,
    slice: f64,
    decay: f32,
    pitch: f32,
    recording: bool,
    rec_len: usize,
    play_pos: f64,
    level: f32,
    last_slice_idx: i64,
    fade: usize,
}

impl BeatRepeat {
    fn new(sr: f32) -> Self {
        Self {
            buf: vec![[0.0; 2]; (sr * 8.5) as usize],
            slice: 0.5,
            decay: 0.0,
            pitch: 0.0,
            recording: false,
            rec_len: 0,
            play_pos: 0.0,
            level: 1.0,
            last_slice_idx: i64::MIN,
            fade: 64,
        }
    }
}

impl Effect for BeatRepeat {
    fn set_param(&mut self, i: usize, v: f32) {
        match i {
            0 => self.slice = NOTE_BEATS[choice_index(v, NOTE_BEATS.len())],
            1 => self.decay = v * 0.5,
            2 => self.pitch = (v * 24.0 - 12.0).round(),
            _ => {}
        }
    }

    fn process(&mut self, buf: &mut [Frame], ctx: &FxCtx) {
        let fpb = ctx.frames_per_beat();
        let slice_len = ((self.slice * fpb) as usize).clamp(64, self.buf.len());
        let rate = 2f64.powf(self.pitch as f64 / 12.0);
        for (i, f) in buf.iter_mut().enumerate() {
            let b = ctx.beat + i as f64 * ctx.beats_per_frame;
            // Capture at every bar line; repeat the first slice until the next bar.
            let bar = (b / 4.0).floor() as i64;
            if bar != self.last_slice_idx {
                self.last_slice_idx = bar;
                self.recording = true;
                self.rec_len = 0;
                self.play_pos = 0.0;
                self.level = 1.0;
            }
            if self.recording {
                if self.rec_len < slice_len {
                    self.buf[self.rec_len] = *f;
                    self.rec_len += 1;
                    continue; // pass through while capturing
                }
                self.recording = false;
            }
            let len = self.rec_len.max(1);
            let p = self.play_pos as usize % len;
            let mut s = self.buf[p];
            // Short fades at slice edges to avoid clicks.
            let edge = p.min(len - 1 - p);
            if edge < self.fade {
                let g = edge as f32 / self.fade as f32;
                s = [s[0] * g, s[1] * g];
            }
            self.play_pos += rate;
            if self.play_pos >= len as f64 {
                self.play_pos -= len as f64;
                self.level *= 1.0 - self.decay;
            }
            *f = [s[0] * self.level, s[1] * self.level];
        }
    }

    fn reset(&mut self) {
        self.last_slice_idx = i64::MIN;
    }
}

// ---------------------------------------------------------------- Distortion

struct Distortion {
    sr: f32,
    drive: f32,
    out: f32,
    shape: usize,
    lp: Biquad,
}

impl Distortion {
    fn new(sr: f32) -> Self {
        Self {
            sr,
            drive: 1.0,
            out: 1.0,
            shape: 0,
            lp: Biquad::lowpass(sr, 12000.0, 0.7),
        }
    }
}

impl Effect for Distortion {
    fn set_param(&mut self, i: usize, v: f32) {
        match i {
            0 => self.drive = db_to_gain(v * 36.0),
            1 => self
                .lp
                .set_lowpass(self.sr, log_map(800.0, 18000.0, v), 0.7),
            2 => self.out = db_to_gain(-24.0 + v * 30.0),
            3 => self.shape = choice_index(v, 3),
            _ => {}
        }
    }

    fn process(&mut self, buf: &mut [Frame], _ctx: &FxCtx) {
        let comp = 1.0 / self.drive.sqrt();
        for f in buf.iter_mut() {
            for ch in 0..2 {
                let x = f[ch] * self.drive;
                let y = match self.shape {
                    0 => x.tanh(),
                    1 => x.clamp(-1.0, 1.0),
                    _ => (x * 0.5 * PI).sin(),
                };
                f[ch] = self.lp.tick(ch, y) * self.out * comp.max(0.25);
            }
        }
    }
}

// ---------------------------------------------------------------- Compressor

struct Compressor {
    sr: f32,
    thresh_db: f32,
    ratio: f32,
    att: f32,
    rel: f32,
    makeup: f32,
    env_db: f32,
}

impl Compressor {
    fn new(sr: f32) -> Self {
        Self {
            sr,
            thresh_db: -18.0,
            ratio: 4.0,
            att: 0.0,
            rel: 0.0,
            makeup: 1.0,
            env_db: 0.0,
        }
    }

    fn coef(&self, ms: f32) -> f32 {
        (-1.0 / (ms * 0.001 * self.sr)).exp()
    }
}

impl Effect for Compressor {
    fn set_param(&mut self, i: usize, v: f32) {
        match i {
            0 => self.thresh_db = -48.0 + v * 48.0,
            1 => self.ratio = 1.0 + v * 19.0,
            2 => self.att = self.coef(0.1 + v * 99.9),
            3 => self.rel = self.coef(10.0 + v * 990.0),
            4 => self.makeup = db_to_gain(v * 24.0),
            _ => {}
        }
    }

    fn process(&mut self, buf: &mut [Frame], _ctx: &FxCtx) {
        for f in buf.iter_mut() {
            let peak = f[0].abs().max(f[1].abs()).max(1e-6);
            let over = (20.0 * peak.log10() - self.thresh_db).max(0.0);
            let target = over * (1.0 - 1.0 / self.ratio);
            let c = if target > self.env_db {
                self.att
            } else {
                self.rel
            };
            self.env_db = target + (self.env_db - target) * c;
            let g = db_to_gain(-self.env_db) * self.makeup;
            f[0] *= g;
            f[1] *= g;
        }
    }
}

// ---------------------------------------------------------------- Utility

struct Utility {
    gain: Smoother,
    width: f32,
    pan: f32,
    bass_mono: bool,
    xo: crate::dsp::Lr4Crossover,
}

impl Utility {
    fn new(sr: f32) -> Self {
        Self {
            gain: Smoother::new(1.0, 10.0, sr),
            width: 1.0,
            pan: 0.0,
            bass_mono: false,
            xo: crate::dsp::Lr4Crossover::new(sr, 150.0),
        }
    }
}

impl Effect for Utility {
    fn set_param(&mut self, i: usize, v: f32) {
        match i {
            0 => {
                let db = -60.0 + v * 72.0;
                self.gain
                    .set(if db <= -59.9 { 0.0 } else { db_to_gain(db) });
            }
            1 => self.width = v * 2.0,
            2 => self.pan = v * 2.0 - 1.0,
            3 => self.bass_mono = v >= 0.5,
            _ => {}
        }
    }

    fn process(&mut self, buf: &mut [Frame], _ctx: &FxCtx) {
        let pl = (1.0 - self.pan).min(1.0);
        let pr = (1.0 + self.pan).min(1.0);
        for f in buf.iter_mut() {
            let mut x = *f;
            if self.bass_mono {
                let (lo, hi) = self.xo.split(x);
                let m = (lo[0] + lo[1]) * 0.5;
                x = [m + hi[0], m + hi[1]];
            }
            let m = (x[0] + x[1]) * 0.5;
            let s = (x[0] - x[1]) * 0.5 * self.width;
            let g = self.gain.next();
            *f = [(m + s) * g * pl, (m - s) * g * pr];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_native_effect_is_stable() {
        let sr = 48000.0;
        let mut ctx = FxCtx::new(sr);
        ctx.bpm = 128.0;
        ctx.beats_per_frame = 128.0 / 60.0 / sr as f64;
        for fx in NativeFx::ALL {
            let mut e = fx.create(sr);
            // Push every parameter to its extremes, too.
            for (pi, _) in fx.params().iter().enumerate() {
                e.set_param(pi, 1.0);
            }
            let mut rng = crate::dsp::Rng::new(7);
            for block in 0..200 {
                let mut buf: Vec<Frame> = (0..256)
                    .map(|_| [rng.bipolar() * 0.5, rng.bipolar() * 0.5])
                    .collect();
                e.process(&mut buf, &ctx);
                ctx.beat += 256.0 * ctx.beats_per_frame;
                for f in &buf {
                    assert!(
                        f[0].is_finite() && f[1].is_finite(),
                        "{} produced NaN",
                        fx.name()
                    );
                    assert!(
                        f[0].abs() < 16.0,
                        "{} blew up at block {block}: {}",
                        fx.name(),
                        f[0]
                    );
                }
            }
        }
    }

    #[test]
    fn param_formatting() {
        let p = NativeFx::Echo.params()[0];
        assert_eq!(p.format(p.default), "1/4");
        let g = NativeFx::Utility.params()[0];
        assert_eq!(g.format(g.default), "+0.0 dB");
        assert_eq!(g.format(0.0), "-inf dB");
    }
}
