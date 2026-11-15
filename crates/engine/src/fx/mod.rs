//! Channel insert effects (User Guide §7.4, rebuilt and extended).
//!
//! * up to [`MAX_SLOTS`] inserts per channel, each PRE (after EQ/filter, before
//!   the fader, so headphone cue hears it) or POST (after the fader);
//! * per-slot ON, per-slot dry/wet MIX (new), chain-wide BYPASS ALL;
//! * reorder by moving slots;
//! * eight macros per channel; each macro can drive up to [`MACRO_TARGETS`]
//!   parameters across any inserts, each with its own range (new);
//! * click-free bypass ramps;
//! * fault isolation: a panicking effect, or one that outputs NaN/Inf, is
//!   bypassed and flagged instead of taking down the engine or the speakers;
//! * latency reporting so the mixer can compensate across channels.

pub mod native;

use crate::dsp::{Frame, Smoother};
use serde::{Deserialize, Serialize};

pub const MAX_SLOTS: usize = 4;
pub const NUM_MACROS: usize = 8;
pub const MACRO_TARGETS: usize = 4;

/// Timing context passed to effects every block.
#[derive(Clone, Copy, Debug)]
pub struct FxCtx {
    pub sample_rate: f32,
    pub bpm: f64,
    /// Master clock beat at the first frame of the block.
    pub beat: f64,
    pub beats_per_frame: f64,
}

impl FxCtx {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate,
            bpm: 120.0,
            beat: 0.0,
            beats_per_frame: 120.0 / 60.0 / sample_rate as f64,
        }
    }

    pub fn frames_per_beat(&self) -> f64 {
        60.0 / self.bpm.max(1.0) * self.sample_rate as f64
    }
}

/// A realtime audio effect. Lives on the audio thread.
///
/// Parameters are normalized to 0..1. Effects output the full processed
/// signal (like a plugin); the slot applies MIX on top.
pub trait Effect: Send {
    fn set_param(&mut self, index: usize, value: f32);
    fn process(&mut self, buf: &mut [Frame], ctx: &FxCtx);
    /// Latency in samples introduced by this effect.
    fn latency(&self) -> usize {
        0
    }
    fn reset(&mut self) {}
    /// True if the effect reported an error during the last `process`
    /// (e.g. a plugin returning an error code). The slot is then bypassed.
    fn failed(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum InsertPos {
    #[default]
    Pre,
    Post,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SlotHealth {
    #[default]
    Ok,
    /// The effect panicked or produced non-finite output and was bypassed.
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MacroTarget {
    pub slot: u8,
    pub param: u32,
    pub min: f32,
    pub max: f32,
}

impl MacroTarget {
    pub fn map(&self, v: f32) -> f32 {
        (self.min + (self.max - self.min) * v.clamp(0.0, 1.0)).clamp(0.0, 1.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Macro {
    pub value: f32,
    pub targets: [Option<MacroTarget>; MACRO_TARGETS],
}

/// Fixed-capacity delay used for dry-path and latency compensation.
pub struct DelayLine {
    buf: Vec<Frame>,
    pos: usize,
    pub delay: usize,
}

impl DelayLine {
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: vec![[0.0; 2]; capacity.max(1) + 1],
            pos: 0,
            delay: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.buf.len() - 1
    }

    pub fn set_delay(&mut self, d: usize) {
        self.delay = d.min(self.capacity());
    }

    #[inline]
    pub fn tick(&mut self, x: Frame) -> Frame {
        if self.delay == 0 {
            return x;
        }
        let n = self.buf.len();
        self.buf[self.pos] = x;
        let r = (self.pos + n - self.delay) % n;
        self.pos = (self.pos + 1) % n;
        self.buf[r]
    }

    pub fn process(&mut self, buf: &mut [Frame]) {
        if self.delay == 0 {
            return;
        }
        for f in buf {
            *f = self.tick(*f);
        }
    }

    pub fn clear(&mut self) {
        self.buf.fill([0.0; 2]);
    }
}

pub struct FxSlot {
    pub effect: Box<dyn Effect>,
    pub pos: InsertPos,
    pub on: bool,
    pub mix: f32,
    pub health: SlotHealth,
    gain: Smoother,
    dry_delay: DelayLine,
    latency: usize,
    /// Peak output level of the slot (for the chain window meters).
    pub peak: f32,
}

pub enum SlotOutcome {
    Ok,
    Failed,
}

impl FxSlot {
    pub fn new(
        effect: Box<dyn Effect>,
        pos: InsertPos,
        sample_rate: f32,
        max_latency: usize,
    ) -> Self {
        let latency = effect.latency();
        let mut dry_delay = DelayLine::new(max_latency);
        dry_delay.set_delay(latency);
        Self {
            effect,
            pos,
            on: true,
            mix: 1.0,
            health: SlotHealth::Ok,
            gain: Smoother::new(1.0, 8.0, sample_rate),
            dry_delay,
            latency,
            peak: 0.0,
        }
    }

    pub fn set_latency(&mut self, samples: usize) {
        self.latency = samples;
        self.dry_delay.set_delay(samples);
    }

    pub fn latency(&self) -> usize {
        if self.health == SlotHealth::Failed {
            0
        } else {
            self.latency
        }
    }

    fn active(&self, bypass_all: bool) -> bool {
        self.on && !bypass_all && self.health == SlotHealth::Ok
    }

    /// Process in place. `scratch` must be at least `buf.len()` long.
    fn process(&mut self, buf: &mut [Frame], scratch: &mut [Frame], ctx: &FxCtx, bypass_all: bool) {
        let target = if self.active(bypass_all) {
            self.mix
        } else {
            0.0
        };
        self.gain.set(target);
        if self.gain.is_settled() && self.gain.current == 0.0 {
            // Fully bypassed: keep latency consistent so toggling ON doesn't shift time.
            self.dry_delay.process(buf);
            self.peak *= 0.9;
            return;
        }
        let dry = &mut scratch[..buf.len()];
        dry.copy_from_slice(buf);
        self.dry_delay.process(dry);

        let effect = &mut self.effect;
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| effect.process(buf, ctx)));
        let mut ok = result.is_ok() && !self.effect.failed();
        if ok {
            for f in buf.iter() {
                if !f[0].is_finite()
                    || !f[1].is_finite()
                    || f[0].abs() > 1000.0
                    || f[1].abs() > 1000.0
                {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            self.health = SlotHealth::Failed;
            buf.copy_from_slice(dry);
            self.gain.snap(0.0);
            return;
        }
        let mut peak = self.peak * 0.9;
        for (o, d) in buf.iter_mut().zip(dry.iter()) {
            let g = self.gain.next();
            o[0] = d[0] + (o[0] - d[0]) * g;
            o[1] = d[1] + (o[1] - d[1]) * g;
            peak = peak.max(o[0].abs()).max(o[1].abs());
        }
        self.peak = peak;
    }
}

/// The per-channel insert chain.
pub struct FxChain {
    pub slots: [Option<FxSlot>; MAX_SLOTS],
    pub bypass_all: bool,
    pub macros: [Macro; NUM_MACROS],
    scratch: Vec<Frame>,
    sample_rate: f32,
    max_latency: usize,
}

impl FxChain {
    pub fn new(sample_rate: f32, max_block: usize, max_latency: usize) -> Self {
        Self {
            slots: Default::default(),
            bypass_all: false,
            macros: [Macro::default(); NUM_MACROS],
            scratch: vec![[0.0; 2]; max_block],
            sample_rate,
            max_latency,
        }
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    pub fn max_latency(&self) -> usize {
        self.max_latency
    }

    /// Install an effect into a slot, returning whatever was there.
    pub fn insert(
        &mut self,
        slot: usize,
        effect: Box<dyn Effect>,
        pos: InsertPos,
    ) -> Option<FxSlot> {
        let s = FxSlot::new(effect, pos, self.sample_rate, self.max_latency);
        self.slots.get_mut(slot)?.replace(s)
    }

    pub fn install(&mut self, slot: usize, s: FxSlot) -> Option<FxSlot> {
        self.slots.get_mut(slot)?.replace(s)
    }

    pub fn remove(&mut self, slot: usize) -> Option<FxSlot> {
        self.slots.get_mut(slot)?.take()
    }

    /// Move a slot to a new index, shifting the others (drag-to-reorder).
    pub fn move_slot(&mut self, from: usize, to: usize) {
        if from >= MAX_SLOTS || to >= MAX_SLOTS || from == to {
            return;
        }
        if from < to {
            self.slots[from..=to].rotate_left(1);
        } else {
            self.slots[to..=from].rotate_right(1);
        }
        // Macro targets follow their effects.
        for m in self.macros.iter_mut() {
            for t in m.targets.iter_mut().flatten() {
                let s = t.slot as usize;
                t.slot = if s == from {
                    to as u8
                } else if from < to && s > from && s <= to {
                    (s - 1) as u8
                } else if to < from && s >= to && s < from {
                    (s + 1) as u8
                } else {
                    s as u8
                };
            }
        }
    }

    pub fn set_param(&mut self, slot: usize, param: usize, value: f32) {
        if let Some(Some(s)) = self.slots.get_mut(slot) {
            s.effect.set_param(param, value.clamp(0.0, 1.0));
        }
    }

    /// Set a macro and push its value to every target parameter.
    pub fn set_macro(&mut self, idx: usize, value: f32) {
        let Some(m) = self.macros.get_mut(idx) else {
            return;
        };
        m.value = value.clamp(0.0, 1.0);
        let m = *m;
        for t in m.targets.iter().flatten() {
            self.set_param(t.slot as usize, t.param as usize, t.map(m.value));
        }
    }

    pub fn has_inserts(&self) -> bool {
        self.slots.iter().any(|s| s.is_some())
    }

    /// Latency of PRE and POST sections.
    pub fn latency(&self) -> (usize, usize) {
        let mut pre = 0;
        let mut post = 0;
        for s in self.slots.iter().flatten() {
            match s.pos {
                InsertPos::Pre => pre += s.latency(),
                InsertPos::Post => post += s.latency(),
            }
        }
        (pre, post)
    }

    pub fn process(&mut self, pos: InsertPos, buf: &mut [Frame], ctx: &FxCtx) {
        let scratch = &mut self.scratch[..];
        for s in self.slots.iter_mut().flatten() {
            if s.pos == pos {
                s.process(buf, scratch, ctx, self.bypass_all);
            }
        }
    }

    pub fn health(&self) -> [Option<(bool, SlotHealth, f32)>; MAX_SLOTS] {
        let mut out = [None; MAX_SLOTS];
        for (i, s) in self.slots.iter().enumerate() {
            if let Some(s) = s {
                out[i] = Some((s.active(self.bypass_all), s.health, s.peak));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Gain(f32);
    impl Effect for Gain {
        fn set_param(&mut self, _: usize, v: f32) {
            self.0 = v * 2.0;
        }
        fn process(&mut self, buf: &mut [Frame], _: &FxCtx) {
            for f in buf {
                f[0] *= self.0;
                f[1] *= self.0;
            }
        }
    }

    struct Boom;
    impl Effect for Boom {
        fn set_param(&mut self, _: usize, _: f32) {}
        fn process(&mut self, _: &mut [Frame], _: &FxCtx) {
            panic!("plugin crashed");
        }
    }

    struct Nan;
    impl Effect for Nan {
        fn set_param(&mut self, _: usize, _: f32) {}
        fn process(&mut self, buf: &mut [Frame], _: &FxCtx) {
            buf[3] = [f32::NAN, 0.0];
        }
    }

    struct Late(Vec<Frame>, usize);
    impl Effect for Late {
        fn set_param(&mut self, _: usize, _: f32) {}
        fn process(&mut self, buf: &mut [Frame], _: &FxCtx) {
            for f in buf {
                self.0.push(*f);
                *f = if self.0.len() > self.1 {
                    self.0[self.0.len() - 1 - self.1]
                } else {
                    [0.0; 2]
                };
            }
        }
        fn latency(&self) -> usize {
            self.1
        }
    }

    fn ctx() -> FxCtx {
        FxCtx::new(48000.0)
    }

    #[test]
    fn panicking_effect_is_isolated() {
        let mut c = FxChain::new(48000.0, 64, 512);
        c.insert(0, Box::new(Boom), InsertPos::Pre);
        let mut buf = vec![[0.5, 0.5]; 64];
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        c.process(InsertPos::Pre, &mut buf, &ctx());
        std::panic::set_hook(hook);
        assert!(buf.iter().all(|f| f[0] == 0.5));
        assert_eq!(c.slots[0].as_ref().unwrap().health, SlotHealth::Failed);
    }

    #[test]
    fn nan_output_is_caught() {
        let mut c = FxChain::new(48000.0, 64, 512);
        c.insert(0, Box::new(Nan), InsertPos::Post);
        let mut buf = vec![[0.25, 0.25]; 64];
        c.process(InsertPos::Post, &mut buf, &ctx());
        assert!(buf.iter().all(|f| f[0] == 0.25));
        assert_eq!(c.health()[0].unwrap().1, SlotHealth::Failed);
    }

    #[test]
    fn macro_drives_multiple_targets_with_ranges() {
        let mut c = FxChain::new(48000.0, 64, 512);
        c.insert(0, Box::new(Gain(1.0)), InsertPos::Pre);
        c.insert(1, Box::new(Gain(1.0)), InsertPos::Pre);
        c.macros[0].targets[0] = Some(MacroTarget {
            slot: 0,
            param: 0,
            min: 0.5,
            max: 0.5,
        });
        c.macros[0].targets[1] = Some(MacroTarget {
            slot: 1,
            param: 0,
            min: 0.0,
            max: 1.0,
        });
        c.set_macro(0, 0.25);
        let mut buf = vec![[1.0, 1.0]; 64];
        for _ in 0..20 {
            buf.fill([1.0, 1.0]);
            c.process(InsertPos::Pre, &mut buf, &ctx());
        }
        // slot0 gain = 0.5*2 = 1.0; slot1 gain = 0.25*2 = 0.5
        assert!((buf[63][0] - 0.5).abs() < 1e-3, "{:?}", buf[63]);
    }

    #[test]
    fn bypass_all_ramps_to_dry() {
        let mut c = FxChain::new(48000.0, 512, 512);
        c.insert(0, Box::new(Gain(0.0)), InsertPos::Pre);
        c.bypass_all = true;
        let mut buf = vec![[1.0, 1.0]; 512];
        for _ in 0..8 {
            buf.fill([1.0, 1.0]);
            c.process(InsertPos::Pre, &mut buf, &ctx());
        }
        assert!((buf[511][0] - 1.0).abs() < 1e-4);
    }

    #[test]
    fn mix_on_latent_effect_stays_aligned() {
        let mut c = FxChain::new(48000.0, 256, 512);
        c.insert(0, Box::new(Late(Vec::new(), 10)), InsertPos::Pre);
        c.slots[0].as_mut().unwrap().mix = 0.5;
        c.slots[0].as_mut().unwrap().gain.snap(0.5);
        let mut buf: Vec<Frame> = (0..256).map(|i| [i as f32, 0.0]).collect();
        c.process(InsertPos::Pre, &mut buf, &ctx());
        // Both dry and wet are delayed by 10, so output == input delayed by 10.
        assert!((buf[100][0] - 90.0).abs() < 1e-3);
        assert_eq!(c.latency(), (10, 0));
    }

    #[test]
    fn move_slot_keeps_macro_targets() {
        let mut c = FxChain::new(48000.0, 64, 512);
        for i in 0..3 {
            c.insert(i, Box::new(Gain(1.0)), InsertPos::Pre);
        }
        c.macros[0].targets[0] = Some(MacroTarget {
            slot: 0,
            param: 0,
            min: 0.0,
            max: 1.0,
        });
        c.macros[1].targets[0] = Some(MacroTarget {
            slot: 2,
            param: 0,
            min: 0.0,
            max: 1.0,
        });
        c.move_slot(0, 2);
        assert_eq!(c.macros[0].targets[0].unwrap().slot, 2);
        assert_eq!(c.macros[1].targets[0].unwrap().slot, 1);
    }
}
