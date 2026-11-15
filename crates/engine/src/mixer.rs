//! Mixer: channel strips, crossfader / vector mixing, master & cue buses.
//!
//! Channel signal order (as documented in the original's §12.4):
//! TRIM → EQ → FILTER → PRE inserts → (cue tap) → VOLUME → POST inserts → output.

use serde::{Deserialize, Serialize};

use crate::dsp::{db_to_gain, Frame, Lr4Crossover, Meter, Smoother, Svf};
use crate::fx::{DelayLine, FxChain, FxCtx, InsertPos};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FaderCurve {
    /// Even dB travel from −60 dB to 0 dB (the original's LINEAR (dB)).
    #[default]
    LinearDb,
    ConstantPower,
    Dipped,
    FastCut,
}

impl FaderCurve {
    pub fn gain(self, v: f32) -> f32 {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.0 {
            return 0.0;
        }
        match self {
            FaderCurve::LinearDb => db_to_gain(-60.0 + 60.0 * v),
            FaderCurve::ConstantPower => (v * std::f32::consts::FRAC_PI_2).sin(),
            FaderCurve::Dipped => v * v,
            FaderCurve::FastCut => (v * 8.0).min(1.0),
        }
    }

    pub const ALL: [FaderCurve; 4] = [
        FaderCurve::LinearDb,
        FaderCurve::ConstantPower,
        FaderCurve::Dipped,
        FaderCurve::FastCut,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FaderCurve::LinearDb => "Linear (dB)",
            FaderCurve::ConstantPower => "Constant power",
            FaderCurve::Dipped => "Dipped",
            FaderCurve::FastCut => "Fast cut",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Output {
    #[default]
    Master,
    None,
    /// Direct hardware stereo pair (0 = outputs 1/2). Bypasses master volume and limiter.
    Pair(u8),
}

/// Which stages apply on channels routed directly to a hardware pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtStages {
    pub trim: bool,
    pub eq: bool,
    pub filter: bool,
    pub volume: bool,
}

impl Default for ExtStages {
    fn default() -> Self {
        Self {
            trim: false,
            eq: true,
            filter: true,
            volume: false,
        }
    }
}

/// Knob value (0..1, 0.5 = 0 dB) → EQ band gain. 0 = full kill, 1 = +6 dB.
pub fn eq_gain(k: f32) -> f32 {
    let k = k.clamp(0.0, 1.0);
    if k >= 0.5 {
        db_to_gain((k - 0.5) * 12.0)
    } else {
        // Smooth taper down to a true kill.
        let x = k / 0.5;
        x * x * x.sqrt()
    }
}

/// All user-facing channel settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelParams {
    pub trim_db: f32,
    pub eq: [f32; 3],
    /// 0..1, 0.5 = open. Left = low-pass, right = high-pass.
    pub cutoff: f32,
    pub res: f32,
    /// 0..1, 1 = full band.
    pub width: f32,
    pub volume: f32,
    pub cue: bool,
    pub mute: bool,
    pub solo: bool,
    pub output: Output,
}

impl Default for ChannelParams {
    fn default() -> Self {
        Self {
            trim_db: 0.0,
            eq: [0.5; 3],
            cutoff: 0.5,
            res: 0.5,
            width: 1.0,
            volume: 1.0,
            cue: false,
            mute: false,
            solo: false,
            output: Output::Master,
        }
    }
}

pub struct Channel {
    pub params: ChannelParams,
    sr: f32,
    trim: Smoother,
    eq_gain: [Smoother; 3],
    xo_low: Lr4Crossover,
    xo_high: Lr4Crossover,
    /// All-pass compensation for the low band (keeps the EQ sum flat).
    xo_comp: Lr4Crossover,
    lp: Svf,
    hp: Svf,
    cutoff: Smoother,
    res: Smoother,
    width: Smoother,
    filter_counter: usize,
    vol: Smoother,
    pub fx: FxChain,
    pre_meter: Meter,
    post_meter: Meter,
    /// Delay applied to the channel output to align with the slowest channel.
    pub comp_delay: DelayLine,
    /// Delay applied to the cue tap for the same reason.
    pub cue_delay: DelayLine,
    cue_buf: Vec<Frame>,
    /// Crossfader / vector-mixer gain applied on top of the volume.
    pub mix_gain: Smoother,
    pub ext_stages: ExtStages,
}

pub struct ChannelOutput<'a> {
    pub main: &'a [Frame],
    pub cue: Option<&'a [Frame]>,
}

impl Channel {
    pub fn new(sr: f32, max_block: usize, max_latency: usize) -> Self {
        let mut c = Self {
            params: ChannelParams::default(),
            sr,
            trim: Smoother::new(1.0, 20.0, sr),
            eq_gain: [Smoother::new(1.0, 15.0, sr); 3],
            xo_low: Lr4Crossover::new(sr, 250.0),
            xo_high: Lr4Crossover::new(sr, 2500.0),
            xo_comp: Lr4Crossover::new(sr, 2500.0),
            lp: Svf::default(),
            hp: Svf::default(),
            cutoff: Smoother::new(0.5, 30.0, sr),
            res: Smoother::new(0.5, 30.0, sr),
            width: Smoother::new(1.0, 30.0, sr),
            filter_counter: 0,
            vol: Smoother::new(1.0, 15.0, sr),
            fx: FxChain::new(sr, max_block, max_latency),
            pre_meter: Meter::default(),
            post_meter: Meter::default(),
            comp_delay: DelayLine::new(max_latency),
            cue_delay: DelayLine::new(max_latency),
            cue_buf: vec![[0.0; 2]; max_block],
            mix_gain: Smoother::new(1.0, 10.0, sr),
            ext_stages: ExtStages::default(),
        };
        c.update_filter(true);
        c
    }

    pub fn set_crossovers(&mut self, low_hz: f32, high_hz: f32) {
        let high = high_hz.max(low_hz * 1.5);
        self.xo_low = Lr4Crossover::new(self.sr, low_hz);
        self.xo_high = Lr4Crossover::new(self.sr, high);
        self.xo_comp = Lr4Crossover::new(self.sr, high);
    }

    pub fn set_params(&mut self, p: ChannelParams) {
        self.params = p;
    }

    pub fn meters(&self) -> (Meter, Meter) {
        (self.pre_meter, self.post_meter)
    }

    /// Band edges in octaves above 20 Hz for the combined CUTOFF/WIDTH filter.
    fn band(cutoff: f32, width: f32, sr: f32) -> (f32, f32) {
        // Ranges from the original: LP 16 kHz → 100 Hz, HP 50 Hz → 8 kHz,
        // scaled down below 44.1 kHz.
        let scale = (sr / 44100.0).min(1.0);
        let x = (cutoff - 0.5) * 2.0;
        let (mut lo, mut hi) = (15.0f32, 21000.0f32 * scale);
        if x < 0.0 {
            hi = 16000.0 * scale * (100.0 / (16000.0 * scale)).powf(-x);
        } else if x > 0.0 {
            lo = 50.0 * (8000.0 * scale / 50.0).powf(x);
        }
        if width < 0.999 {
            // Narrow the band symmetrically (in octaves) around its centre.
            let (llo, lhi) = (lo.log2(), hi.log2());
            let centre = (llo + lhi) * 0.5;
            let max_span = 0.4 + 10.0 * width;
            let span = (lhi - llo).min(max_span);
            lo = 2f32.powf(centre - span * 0.5);
            hi = 2f32.powf(centre + span * 0.5);
        }
        (lo, hi.min(sr * 0.45))
    }

    fn update_filter(&mut self, force: bool) {
        if !force && self.cutoff.is_settled() && self.res.is_settled() && self.width.is_settled() {
            return;
        }
        let c = self.cutoff.current;
        let w = self.width.current;
        let (lo, hi) = Self::band(c, w, self.sr);
        // Resonance only "speaks" once the filter is engaged, so the open
        // position stays transparent.
        let engaged = (((c - 0.5).abs() * 8.0) + (1.0 - w) * 8.0).min(1.0);
        let r = self.res.current;
        let res = (r * r * r * 0.95) * engaged;
        self.lp.set(self.sr, hi, res);
        self.hp.set(self.sr, lo, res);
    }

    /// Process one block. `buf` holds the source (deck) audio and is
    /// overwritten with the channel output. Returns the cue tap if enabled.
    pub fn process(
        &mut self,
        buf: &mut [Frame],
        ctx: &FxCtx,
        curve: FaderCurve,
        audible: bool,
    ) -> Option<&[Frame]> {
        let p = self.params;
        let direct = matches!(p.output, Output::Pair(_));
        let st = if direct {
            self.ext_stages
        } else {
            ExtStages {
                trim: true,
                eq: true,
                filter: true,
                volume: true,
            }
        };

        self.trim.set(if st.trim {
            db_to_gain(p.trim_db.clamp(-6.0, 6.0))
        } else {
            1.0
        });
        for b in 0..3 {
            self.eq_gain[b].set(if st.eq { eq_gain(p.eq[b]) } else { 1.0 });
        }
        if st.filter {
            self.cutoff.set(p.cutoff);
            self.width.set(p.width);
        } else {
            self.cutoff.set(0.5);
            self.width.set(1.0);
        }
        self.res.set(p.res);
        let fader = if st.volume { curve.gain(p.volume) } else { 1.0 };
        self.vol.set(if audible { fader } else { 0.0 });

        let eq_active = self
            .eq_gain
            .iter()
            .any(|g| !(g.is_settled() && g.current == 1.0));

        for f in buf.iter_mut() {
            let t = self.trim.next();
            let mut x = [f[0] * t, f[1] * t];

            if eq_active {
                let (low, rest) = self.xo_low.split(x);
                let (mid, high) = self.xo_high.split(rest);
                let (l1, l2) = self.xo_comp.split(low);
                let low = [l1[0] + l2[0], l1[1] + l2[1]];
                let gl = self.eq_gain[0].next();
                let gm = self.eq_gain[1].next();
                let gh = self.eq_gain[2].next();
                x = [
                    low[0] * gl + mid[0] * gm + high[0] * gh,
                    low[1] * gl + mid[1] * gm + high[1] * gh,
                ];
            }

            self.cutoff.next();
            self.res.next();
            self.width.next();
            self.filter_counter += 1;
            if self.filter_counter >= 16 {
                self.filter_counter = 0;
                self.update_filter(false);
            }
            let filter_on = (self.cutoff.current - 0.5).abs() > 0.002 || self.width.current < 0.998;
            if filter_on {
                for ch in 0..2 {
                    let y = self.lp.tick(ch, x[ch]).low;
                    x[ch] = self.hp.tick(ch, y).high;
                }
            } else {
                // Keep filter states warm so engaging is click-free.
                for ch in 0..2 {
                    let y = self.lp.tick(ch, x[ch]).low;
                    let _ = self.hp.tick(ch, y);
                }
            }
            *f = x;
        }
        self.pre_meter.process(buf, 0.9);

        self.fx.process(InsertPos::Pre, buf, ctx);

        let cue = if p.cue {
            let n = buf.len();
            self.cue_buf[..n].copy_from_slice(buf);
            self.cue_delay.process(&mut self.cue_buf[..n]);
            true
        } else {
            false
        };

        for f in buf.iter_mut() {
            let g = self.vol.next() * self.mix_gain.next();
            f[0] *= g;
            f[1] *= g;
        }

        self.fx.process(InsertPos::Post, buf, ctx);
        self.comp_delay.process(buf);
        self.post_meter.process(buf, 0.9);
        if cue {
            Some(&self.cue_buf[..buf.len()])
        } else {
            None
        }
    }
}

/// How channel levels are blended on top of the volume dials.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Default)]
pub enum MixMode {
    #[default]
    Off,
    /// Classic crossfader. `pos` −1 = A, +1 = B.
    Crossfader { pos: f32, sharp: bool },
    /// Triangle pad for three channels (barycentric, equal power).
    Delta { a: f32, b: f32, c: f32 },
    /// XY pad blending four channels at the corners (equal power).
    Vector { x: f32, y: f32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum XfAssign {
    A,
    B,
    #[default]
    Thru,
}

/// Per-channel gains from the mix mode. Channels 0..4 are the deck channels.
pub fn mix_gains(mode: MixMode, assign: &[XfAssign; 4]) -> [f32; 4] {
    match mode {
        MixMode::Off => [1.0; 4],
        MixMode::Crossfader { pos, sharp } => {
            let t = (pos.clamp(-1.0, 1.0) + 1.0) * 0.5; // 0 = A, 1 = B
            let (ga, gb) = if sharp {
                ((2.0 - 2.0 * t).min(1.0), (2.0 * t).min(1.0))
            } else {
                // Equal power, but unity at centre so a blend doesn't dip.
                let a = (t * std::f32::consts::FRAC_PI_2).cos();
                let b = (t * std::f32::consts::FRAC_PI_2).sin();
                (
                    (a * std::f32::consts::SQRT_2).min(1.0),
                    (b * std::f32::consts::SQRT_2).min(1.0),
                )
            };
            let mut g = [1.0; 4];
            for i in 0..4 {
                g[i] = match assign[i] {
                    XfAssign::A => ga,
                    XfAssign::B => gb,
                    XfAssign::Thru => 1.0,
                };
            }
            g
        }
        MixMode::Delta { a, b, c } => {
            let s = (a.max(0.0) + b.max(0.0) + c.max(0.0)).max(1e-6);
            [
                (a.max(0.0) / s).sqrt(),
                (b.max(0.0) / s).sqrt(),
                (c.max(0.0) / s).sqrt(),
                1.0,
            ]
        }
        MixMode::Vector { x, y } => {
            let x = x.clamp(0.0, 1.0);
            let y = y.clamp(0.0, 1.0);
            // Corners: 0 top-left, 1 top-right, 2 bottom-left, 3 bottom-right.
            let w = [(1.0 - x) * (1.0 - y), x * (1.0 - y), (1.0 - x) * y, x * y];
            [w[0].sqrt(), w[1].sqrt(), w[2].sqrt(), w[3].sqrt()]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> FxCtx {
        FxCtx::new(48000.0)
    }

    #[test]
    fn neutral_channel_is_transparent() {
        let mut ch = Channel::new(48000.0, 512, 1024);
        let input: Vec<Frame> = (0..512)
            .map(|i| [((i as f32) * 0.05).sin() * 0.5; 2])
            .collect();
        let mut buf = input.clone();
        ch.process(&mut buf, &ctx(), FaderCurve::LinearDb, true);
        for (a, b) in input.iter().zip(&buf) {
            assert!((a[0] - b[0]).abs() < 1e-3);
        }
    }

    #[test]
    fn eq_kill_removes_lows() {
        let sr = 48000.0;
        let mut ch = Channel::new(sr, 1024, 1024);
        ch.params.eq[0] = 0.0;
        let mut energy = 0.0;
        for blk in 0..48 {
            let mut buf: Vec<Frame> = (0..1024)
                .map(|i| {
                    let t = (blk * 1024 + i) as f32 / sr;
                    [(2.0 * std::f32::consts::PI * 60.0 * t).sin(); 2]
                })
                .collect();
            ch.process(&mut buf, &ctx(), FaderCurve::LinearDb, true);
            if blk > 24 {
                energy += buf.iter().map(|f| f[0] * f[0]).sum::<f32>();
            }
        }
        let rms = (energy / (23.0 * 1024.0)).sqrt();
        assert!(rms < 0.03, "rms {rms}");
    }

    #[test]
    fn fader_curves() {
        assert_eq!(FaderCurve::LinearDb.gain(0.0), 0.0);
        assert!((FaderCurve::LinearDb.gain(0.5) - db_to_gain(-30.0)).abs() < 1e-5);
        assert!((FaderCurve::ConstantPower.gain(1.0) - 1.0).abs() < 1e-6);
        assert_eq!(FaderCurve::FastCut.gain(0.2), 1.0);
    }

    #[test]
    fn vector_mixer_is_equal_power() {
        let g = mix_gains(MixMode::Vector { x: 0.5, y: 0.5 }, &[XfAssign::Thru; 4]);
        let p: f32 = g.iter().map(|v| v * v).sum();
        assert!((p - 1.0).abs() < 1e-5);
        let g = mix_gains(MixMode::Vector { x: 0.0, y: 0.0 }, &[XfAssign::Thru; 4]);
        assert_eq!(g, [1.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn crossfader_assign() {
        let assign = [XfAssign::A, XfAssign::B, XfAssign::Thru, XfAssign::Thru];
        let g = mix_gains(
            MixMode::Crossfader {
                pos: -1.0,
                sharp: false,
            },
            &assign,
        );
        assert!((g[0] - 1.0).abs() < 1e-5 && g[1] < 1e-5 && g[2] == 1.0);
        let g = mix_gains(
            MixMode::Crossfader {
                pos: 0.0,
                sharp: false,
            },
            &assign,
        );
        assert!((g[0] - 1.0).abs() < 1e-5 && (g[1] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn filter_band_edges() {
        let (lo, hi) = Channel::band(0.5, 1.0, 48000.0);
        assert!(lo < 20.0 && hi > 20000.0);
        let (_, hi) = Channel::band(0.0, 1.0, 48000.0);
        assert!((hi - 100.0).abs() < 1.0);
        let (lo, _) = Channel::band(1.0, 1.0, 48000.0);
        assert!((lo - 8000.0).abs() < 10.0);
        let (lo, hi) = Channel::band(0.5, 0.0, 48000.0);
        assert!((hi / lo).log2() < 0.5);
    }
}
