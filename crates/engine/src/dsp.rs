//! Small, allocation-free DSP building blocks used throughout the engine.
//!
//! Everything in here is safe to call from the realtime audio thread.

use std::f32::consts::PI;

pub type Frame = [f32; 2];

#[inline]
pub fn db_to_gain(db: f32) -> f32 {
    if db <= -96.0 {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

#[inline]
pub fn gain_to_db(g: f32) -> f32 {
    if g <= 1e-6 {
        -120.0
    } else {
        20.0 * g.log10()
    }
}

/// Flush denormals / tiny values to zero to avoid CPU spikes in feedback paths.
#[inline]
pub fn undenormal(x: f32) -> f32 {
    if x.abs() < 1e-18 {
        0.0
    } else {
        x
    }
}

/// 4-point, 3rd-order Hermite interpolation.
#[inline]
pub fn hermite(xm1: f32, x0: f32, x1: f32, x2: f32, t: f32) -> f32 {
    let c = (x1 - xm1) * 0.5;
    let v = x0 - x1;
    let w = c + v;
    let a = w + v + (x2 - x0) * 0.5;
    let b = w + a;
    (((a * t) - b) * t + c) * t + x0
}

/// One-pole parameter smoother (avoids zipper noise on knob moves).
#[derive(Clone, Copy, Debug)]
pub struct Smoother {
    pub current: f32,
    pub target: f32,
    coef: f32,
}

impl Smoother {
    pub fn new(value: f32, time_ms: f32, sample_rate: f32) -> Self {
        let mut s = Self { current: value, target: value, coef: 0.0 };
        s.set_time(time_ms, sample_rate);
        s
    }

    pub fn set_time(&mut self, time_ms: f32, sample_rate: f32) {
        let n = (time_ms * 0.001 * sample_rate).max(1.0);
        self.coef = (-1.0 / n).exp();
    }

    #[inline]
    pub fn set(&mut self, v: f32) {
        self.target = v;
    }

    pub fn snap(&mut self, v: f32) {
        self.target = v;
        self.current = v;
    }

    #[inline]
    pub fn next(&mut self) -> f32 {
        self.current = self.target + (self.current - self.target) * self.coef;
        if (self.current - self.target).abs() < 1e-6 {
            self.current = self.target;
        }
        self.current
    }

    #[inline]
    pub fn is_settled(&self) -> bool {
        self.current == self.target
    }
}

/// Transposed direct form II biquad (RBJ cookbook designs).
#[derive(Clone, Copy, Debug, Default)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: [f32; 2],
    z2: [f32; 2],
}

impl Biquad {
    fn set_coefs(&mut self, b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) {
        self.b0 = b0 / a0;
        self.b1 = b1 / a0;
        self.b2 = b2 / a0;
        self.a1 = a1 / a0;
        self.a2 = a2 / a0;
    }

    pub fn lowpass(sr: f32, freq: f32, q: f32) -> Self {
        let mut b = Self::default();
        b.set_lowpass(sr, freq, q);
        b
    }

    pub fn highpass(sr: f32, freq: f32, q: f32) -> Self {
        let mut b = Self::default();
        b.set_highpass(sr, freq, q);
        b
    }

    pub fn set_lowpass(&mut self, sr: f32, freq: f32, q: f32) {
        let w0 = 2.0 * PI * (freq / sr).clamp(1e-5, 0.49);
        let (s, c) = w0.sin_cos();
        let alpha = s / (2.0 * q);
        self.set_coefs((1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0, 1.0 + alpha, -2.0 * c, 1.0 - alpha);
    }

    pub fn set_highpass(&mut self, sr: f32, freq: f32, q: f32) {
        let w0 = 2.0 * PI * (freq / sr).clamp(1e-5, 0.49);
        let (s, c) = w0.sin_cos();
        let alpha = s / (2.0 * q);
        self.set_coefs((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0, 1.0 + alpha, -2.0 * c, 1.0 - alpha);
    }

    pub fn set_allpass(&mut self, sr: f32, freq: f32, q: f32) {
        let w0 = 2.0 * PI * (freq / sr).clamp(1e-5, 0.49);
        let (s, c) = w0.sin_cos();
        let alpha = s / (2.0 * q);
        self.set_coefs(1.0 - alpha, -2.0 * c, 1.0 + alpha, 1.0 + alpha, -2.0 * c, 1.0 - alpha);
    }

    pub fn set_bandpass(&mut self, sr: f32, freq: f32, q: f32) {
        let w0 = 2.0 * PI * (freq / sr).clamp(1e-5, 0.49);
        let (s, c) = w0.sin_cos();
        let alpha = s / (2.0 * q);
        self.set_coefs(alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * c, 1.0 - alpha);
    }

    pub fn set_peaking(&mut self, sr: f32, freq: f32, q: f32, gain_db: f32) {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * (freq / sr).clamp(1e-5, 0.49);
        let (s, c) = w0.sin_cos();
        let alpha = s / (2.0 * q);
        self.set_coefs(1.0 + alpha * a, -2.0 * c, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * c, 1.0 - alpha / a);
    }

    pub fn reset(&mut self) {
        self.z1 = [0.0; 2];
        self.z2 = [0.0; 2];
    }

    #[inline]
    pub fn tick(&mut self, ch: usize, x: f32) -> f32 {
        let y = self.b0 * x + self.z1[ch];
        self.z1[ch] = undenormal(self.b1 * x - self.a1 * y + self.z2[ch]);
        self.z2[ch] = undenormal(self.b2 * x - self.a2 * y);
        y
    }

    #[inline]
    pub fn tick_frame(&mut self, f: Frame) -> Frame {
        [self.tick(0, f[0]), self.tick(1, f[1])]
    }
}

/// 4th-order Linkwitz-Riley crossover. `low + high` is allpass (flat magnitude).
#[derive(Clone, Copy, Debug, Default)]
pub struct Lr4Crossover {
    lp: [Biquad; 2],
    hp: [Biquad; 2],
}

impl Lr4Crossover {
    pub fn new(sr: f32, freq: f32) -> Self {
        let q = std::f32::consts::FRAC_1_SQRT_2;
        Self {
            lp: [Biquad::lowpass(sr, freq, q); 2],
            hp: [Biquad::highpass(sr, freq, q); 2],
        }
    }

    /// Returns (low, high).
    #[inline]
    pub fn split(&mut self, f: Frame) -> (Frame, Frame) {
        let l0 = self.lp[0].tick_frame(f);
        let l = self.lp[1].tick_frame(l0);
        let h0 = self.hp[0].tick_frame(f);
        let h = self.hp[1].tick_frame(h0);
        (l, h)
    }

    pub fn reset(&mut self) {
        for b in self.lp.iter_mut().chain(self.hp.iter_mut()) {
            b.reset();
        }
    }
}

/// Zero-delay-feedback state variable filter (trapezoidal integration, after
/// Andrew Simper's "SvfLinearTrapOptimised2" technical paper).
/// Stays stable and musical under fast modulation and high resonance.
#[derive(Clone, Copy, Debug, Default)]
pub struct Svf {
    a1: f32,
    a2: f32,
    a3: f32,
    k: f32,
    ic1: [f32; 2],
    ic2: [f32; 2],
}

#[derive(Clone, Copy, Debug)]
pub struct SvfOut {
    pub low: f32,
    pub band: f32,
    pub high: f32,
}

impl Svf {
    /// `res` in 0..1 (0 = Q 0.5, 1 = self-oscillation edge).
    pub fn set(&mut self, sr: f32, cutoff: f32, res: f32) {
        let g = (PI * (cutoff / sr).clamp(1e-5, 0.49)).tan();
        let q = 0.5 + res.clamp(0.0, 0.995) * 19.5;
        self.k = 1.0 / q;
        self.a1 = 1.0 / (1.0 + g * (g + self.k));
        self.a2 = g * self.a1;
        self.a3 = g * self.a2;
    }

    #[inline]
    pub fn tick(&mut self, ch: usize, v0: f32) -> SvfOut {
        let v3 = v0 - self.ic2[ch];
        let v1 = self.a1 * self.ic1[ch] + self.a2 * v3;
        let v2 = self.ic2[ch] + self.a2 * self.ic1[ch] + self.a3 * v3;
        self.ic1[ch] = undenormal(2.0 * v1 - self.ic1[ch]);
        self.ic2[ch] = undenormal(2.0 * v2 - self.ic2[ch]);
        SvfOut { low: v2, band: v1, high: v0 - self.k * v1 - v2 }
    }

    pub fn reset(&mut self) {
        self.ic1 = [0.0; 2];
        self.ic2 = [0.0; 2];
    }
}

/// Simple one-pole DC blocker.
#[derive(Clone, Copy, Debug, Default)]
pub struct DcBlock {
    x1: [f32; 2],
    y1: [f32; 2],
}

impl DcBlock {
    #[inline]
    pub fn tick(&mut self, ch: usize, x: f32) -> f32 {
        let y = x - self.x1[ch] + 0.995 * self.y1[ch];
        self.x1[ch] = x;
        self.y1[ch] = undenormal(y);
        y
    }
}

/// Look-ahead peak limiter for the master bus. Transparent until the ceiling
/// is reached, then applies smooth gain reduction with no overshoot.
pub struct Limiter {
    pub enabled: bool,
    pub ceiling: f32,
    delay: Vec<Frame>,
    peaks: Vec<f32>,
    pos: usize,
    gain: f32,
    release: f32,
    attack: f32,
    pub reduction_db: f32,
}

impl Limiter {
    pub fn new(sr: f32) -> Self {
        let look = ((sr * 0.0015) as usize).max(8);
        Self {
            enabled: true,
            ceiling: db_to_gain(-0.3),
            delay: vec![[0.0; 2]; look],
            peaks: vec![0.0; look],
            pos: 0,
            gain: 1.0,
            release: (-1.0 / (0.12 * sr)).exp(),
            attack: (-1.0 / (look as f32 * 0.5)).exp(),
            reduction_db: 0.0,
        }
    }

    pub fn latency(&self) -> usize {
        self.delay.len()
    }

    pub fn process(&mut self, buf: &mut [Frame]) {
        if !self.enabled {
            self.reduction_db = 0.0;
            return;
        }
        let n = self.delay.len();
        let mut min_gain = 1.0f32;
        for f in buf.iter_mut() {
            let peak = f[0].abs().max(f[1].abs());
            self.peaks[self.pos] = peak;
            // Max over the look-ahead window. Window is tiny (~70 samples) so
            // a linear scan is cheap and branch-predictable.
            let mut wmax = 0.0f32;
            for &p in &self.peaks {
                wmax = wmax.max(p);
            }
            let target = if wmax > self.ceiling { self.ceiling / wmax } else { 1.0 };
            if target < self.gain {
                self.gain = target + (self.gain - target) * self.attack;
                // Never let a peak through: hard floor at target when the
                // current sample itself would overshoot.
            } else {
                self.gain = target + (self.gain - target) * self.release;
            }
            let delayed = self.delay[self.pos];
            self.delay[self.pos] = *f;
            self.pos = (self.pos + 1) % n;
            let mut g = self.gain;
            let dp = delayed[0].abs().max(delayed[1].abs());
            if dp * g > self.ceiling {
                g = self.ceiling / dp;
            }
            min_gain = min_gain.min(g);
            *f = [delayed[0] * g, delayed[1] * g];
        }
        self.reduction_db = gain_to_db(min_gain);
    }
}

/// Tiny xorshift RNG, deterministic and allocation-free.
#[derive(Clone, Copy, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform in [0, 1).
    #[inline]
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Uniform in [-1, 1).
    #[inline]
    pub fn bipolar(&mut self) -> f32 {
        self.next_f32() * 2.0 - 1.0
    }
}

/// Peak/RMS meter with hold and decay, as displayed on channel strips.
#[derive(Clone, Copy, Debug, Default)]
pub struct Meter {
    pub peak: [f32; 2],
    pub rms: [f32; 2],
    acc: [f32; 2],
    n: u32,
}

impl Meter {
    pub fn process(&mut self, buf: &[Frame], decay: f32) {
        self.peak[0] *= decay;
        self.peak[1] *= decay;
        for f in buf {
            for ch in 0..2 {
                let a = f[ch].abs();
                if a > self.peak[ch] {
                    self.peak[ch] = a;
                }
                self.acc[ch] += f[ch] * f[ch];
            }
        }
        self.n += buf.len() as u32;
        if self.n >= 2048 {
            for ch in 0..2 {
                self.rms[ch] = (self.acc[ch] / self.n as f32).sqrt();
                self.acc[ch] = 0.0;
            }
            self.n = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(sr: f32, f: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (2.0 * PI * f * i as f32 / sr).sin()).collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn lr4_sums_flat() {
        let sr = 48000.0;
        for &f in &[50.0, 250.0, 1000.0, 8000.0] {
            let mut x = Lr4Crossover::new(sr, 250.0);
            let s = sine(sr, f, 48000);
            let out: Vec<f32> = s
                .iter()
                .map(|&v| {
                    let (l, h) = x.split([v, v]);
                    l[0] + h[0]
                })
                .collect();
            let r = rms(&out[24000..]) / rms(&s[24000..]);
            assert!((r - 1.0).abs() < 0.02, "freq {f}: ratio {r}");
        }
    }

    #[test]
    fn svf_lowpass_attenuates_highs() {
        let sr = 48000.0;
        let mut f = Svf::default();
        f.set(sr, 500.0, 0.0);
        let s = sine(sr, 8000.0, 9600);
        let out: Vec<f32> = s.iter().map(|&v| f.tick(0, v).low).collect();
        assert!(rms(&out[4800..]) < 0.02);
    }

    #[test]
    fn limiter_holds_ceiling() {
        let sr = 48000.0;
        let mut lim = Limiter::new(sr);
        let mut buf: Vec<Frame> = sine(sr, 100.0, 48000).iter().map(|v| [v * 4.0, v * 4.0]).collect();
        lim.process(&mut buf);
        let peak = buf.iter().map(|f| f[0].abs()).fold(0.0, f32::max);
        assert!(peak <= lim.ceiling + 1e-4, "peak {peak}");
        assert!(peak > 0.8);
    }

    #[test]
    fn hermite_is_exact_on_samples() {
        assert_eq!(hermite(0.0, 1.0, 2.0, 3.0, 0.0), 1.0);
        assert!((hermite(0.0, 1.0, 2.0, 3.0, 0.5) - 1.5).abs() < 1e-6);
    }
}
