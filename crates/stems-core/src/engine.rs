use std::sync::Arc;

use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

use crate::focus::focus_weight;
use crate::soundfield::{band_for_hz, BandAccum, SoundfieldFrame, SOUNDFIELD_BANDS};
use crate::{Complex32, FocusMode, HeuristicSeparator, Separator, Settings, StemMasks, NUM_STEMS};

const NO_BAND: u8 = u8::MAX;

/// Streaming STFT stem-separation engine.
///
/// Signal flow per hop (`fft_size / 4` samples):
///
/// ```text
/// input ─► sqrt-Hann ─► FFT ─► Separator ─► masks ─┐
///                         │                        ▼
///                         └─► spectrum delay ─► Σ gain·mask^iso ─► Focus EQ split
///                                                   │                    │
///                                     main = IFFT ◄─┘     aux = IFFT ◄───┘
/// ```
///
/// Latency is `fft_size + separator.lookahead_frames() * hop` samples.
pub struct StemEngine {
    sample_rate: f32,
    fft_size: usize,
    hop: usize,
    num_bins: usize,

    fft: Arc<dyn RealToComplex<f32>>,
    ifft: Arc<dyn ComplexToReal<f32>>,
    fft_scratch: Vec<Complex32>,
    ifft_scratch: Vec<Complex32>,
    window: Vec<f32>,
    /// `1 / (fft_size * overlap-add gain of window²)`.
    synth_norm: f32,

    /// Last `fft_size` input samples per channel.
    in_buf: [Vec<f32>; 2],
    /// Overlap-add accumulators, `fft_size` long.
    acc_main: [Vec<f32>; 2],
    acc_aux: [Vec<f32>; 2],
    /// Finished output samples for the current hop, `hop` long.
    ready_main: [Vec<f32>; 2],
    ready_aux: [Vec<f32>; 2],
    /// Position inside the current hop.
    pos: usize,

    time: Vec<f32>,
    spec_tmp: Vec<Complex32>,
    /// Ring of the last `lookahead + 1` stereo spectra.
    spec_hist: Vec<[Vec<Complex32>; 2]>,
    hist_write: usize,

    separator: Box<dyn Separator>,
    masks: StemMasks,
    main_gain: Vec<f32>,
    aux_gain: Vec<f32>,

    bin_hz: Vec<f32>,
    bin_band: Vec<u8>,
    analysis_enabled: bool,
    soundfield: SoundfieldFrame,
    frames_processed: u64,
}

impl StemEngine {
    /// FFT size used for a given sample rate (keeps ~93 ms windows / ~10.8 Hz bins).
    pub fn default_fft_size(sample_rate: f32) -> usize {
        if sample_rate <= 50_000.0 {
            4096
        } else if sample_rate <= 100_000.0 {
            8192
        } else {
            16384
        }
    }

    /// Engine with the placeholder [`HeuristicSeparator`].
    pub fn new(sample_rate: f32) -> Self {
        let fft_size = Self::default_fft_size(sample_rate);
        let sep = HeuristicSeparator::new(sample_rate, fft_size);
        Self::with_separator(sample_rate, fft_size, Box::new(sep))
    }

    /// Engine with a custom separator. `fft_size` must be a power of two >= 64. Allocates.
    pub fn with_separator(
        sample_rate: f32,
        fft_size: usize,
        separator: Box<dyn Separator>,
    ) -> Self {
        assert!(fft_size.is_power_of_two() && fft_size >= 64);
        let hop = fft_size / 4;
        let num_bins = fft_size / 2 + 1;

        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(fft_size);
        let ifft = planner.plan_fft_inverse(fft_size);

        // Periodic sqrt-Hann for both analysis and synthesis => Hann after both windows.
        let window: Vec<f32> = (0..fft_size)
            .map(|i| {
                let x = std::f32::consts::TAU * i as f32 / fft_size as f32;
                (0.5 - 0.5 * x.cos()).sqrt()
            })
            .collect();
        let ola_gain: f32 = (0..fft_size / hop).map(|k| window[k * hop].powi(2)).sum();

        let bin_hz_step = sample_rate / fft_size as f32;
        let bin_hz: Vec<f32> = (0..num_bins).map(|k| k as f32 * bin_hz_step).collect();
        let bin_band = bin_hz
            .iter()
            .map(|&f| band_for_hz(f).map_or(NO_BAND, |b| b as u8))
            .collect();

        let hist_len = separator.lookahead_frames() + 1;
        let zeros_c = || vec![Complex32::new(0.0, 0.0); num_bins];

        Self {
            sample_rate,
            fft_size,
            hop,
            num_bins,
            fft_scratch: fft.make_scratch_vec(),
            ifft_scratch: ifft.make_scratch_vec(),
            fft,
            ifft,
            window,
            synth_norm: 1.0 / (fft_size as f32 * ola_gain),
            in_buf: [vec![0.0; fft_size], vec![0.0; fft_size]],
            acc_main: [vec![0.0; fft_size], vec![0.0; fft_size]],
            acc_aux: [vec![0.0; fft_size], vec![0.0; fft_size]],
            ready_main: [vec![0.0; hop], vec![0.0; hop]],
            ready_aux: [vec![0.0; hop], vec![0.0; hop]],
            pos: 0,
            time: vec![0.0; fft_size],
            spec_tmp: zeros_c(),
            spec_hist: (0..hist_len).map(|_| [zeros_c(), zeros_c()]).collect(),
            hist_write: 0,
            separator,
            masks: StemMasks::new(num_bins),
            main_gain: vec![0.0; num_bins],
            aux_gain: vec![0.0; num_bins],
            bin_hz,
            bin_band,
            analysis_enabled: false,
            soundfield: SoundfieldFrame::default(),
            frames_processed: 0,
        }
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    pub fn fft_size(&self) -> usize {
        self.fft_size
    }

    pub fn hop_size(&self) -> usize {
        self.hop
    }

    /// Total processing delay in samples; report this to the host.
    pub fn latency_samples(&self) -> usize {
        self.fft_size + self.separator.lookahead_frames() * self.hop
    }

    /// Switch between real-time and offline (bounce) processing. See [`Separator::set_offline`].
    pub fn set_offline(&mut self, offline: bool) {
        self.separator.set_offline(offline);
    }

    /// Enable/disable the soundfield analysis (skip it while no UI is open).
    pub fn set_analysis_enabled(&mut self, enabled: bool) {
        self.analysis_enabled = enabled;
    }

    /// Latest soundfield analysis. Updated once per hop while analysis is enabled.
    pub fn soundfield(&self) -> &SoundfieldFrame {
        &self.soundfield
    }

    /// Increments once per processed STFT frame.
    pub fn frames_processed(&self) -> u64 {
        self.frames_processed
    }

    /// Clear all audio state. Real-time safe.
    pub fn reset(&mut self) {
        for ch in 0..2 {
            self.in_buf[ch].fill(0.0);
            self.acc_main[ch].fill(0.0);
            self.acc_aux[ch].fill(0.0);
            self.ready_main[ch].fill(0.0);
            self.ready_aux[ch].fill(0.0);
        }
        for frame in &mut self.spec_hist {
            for ch in frame.iter_mut() {
                ch.fill(Complex32::new(0.0, 0.0));
            }
        }
        self.pos = 0;
        self.hist_write = 0;
        self.separator.reset();
        self.soundfield = SoundfieldFrame::default();
    }

    /// Process one host block in place.
    ///
    /// `main` has 1 (mono) or 2 (stereo) channels of equal length. `aux`, if present, receives
    /// the Focus EQ's complementary signal and must have the same shape. Real-time safe.
    pub fn process(
        &mut self,
        main: &mut [&mut [f32]],
        mut aux: Option<&mut [&mut [f32]]>,
        settings: &Settings,
    ) {
        let Some(len) = main.first().map(|c| c.len()) else {
            return;
        };
        let stereo = main.len() > 1;
        let write_base = self.fft_size - self.hop;

        for i in 0..len {
            let l = main[0][i];
            let r = if stereo { main[1][i] } else { l };
            self.in_buf[0][write_base + self.pos] = l;
            self.in_buf[1][write_base + self.pos] = r;

            let (ol, or) = (self.ready_main[0][self.pos], self.ready_main[1][self.pos]);
            write_frame(main, i, ol, or);
            if let Some(aux) = aux.as_deref_mut() {
                let (al, ar) = (self.ready_aux[0][self.pos], self.ready_aux[1][self.pos]);
                write_frame(aux, i, al, ar);
            }

            self.pos += 1;
            if self.pos == self.hop {
                self.process_frame(settings);
                self.pos = 0;
            }
        }
    }

    fn process_frame(&mut self, settings: &Settings) {
        let (n, hop, bins) = (self.fft_size, self.hop, self.num_bins);

        // Analysis.
        let w = self.hist_write;
        for ch in 0..2 {
            for ((t, &x), &win) in self.time.iter_mut().zip(&self.in_buf[ch]).zip(&self.window) {
                *t = x * win;
            }
            let _ = self.fft.process_with_scratch(
                &mut self.time,
                &mut self.spec_hist[w][ch],
                &mut self.fft_scratch,
            );
        }

        // Separation. With lookahead the masks belong to the oldest spectrum in the ring.
        self.separator.separate(
            &self.spec_hist[w][0],
            &self.spec_hist[w][1],
            &mut self.masks,
        );
        let d = (w + 1) % self.spec_hist.len();
        self.hist_write = d;

        // Per-bin gains: stem mix with isolation, then the Focus EQ split.
        let gains = settings.effective_gains();
        let exps = settings.isolation_exponents();
        let focus = &settings.focus;
        let out_gain = settings.output_gain;
        let [xl, xr] = &self.spec_hist[d];
        for k in 0..bins {
            let mut m = 0.0;
            for s in 0..NUM_STEMS {
                if gains[s] != 0.0 {
                    let mk = self.masks.masks[s][k];
                    m += gains[s] * if exps[s] == 1.0 { mk } else { mk.powf(exps[s]) };
                }
            }
            let (main, aux) = match focus.mode {
                FocusMode::Off => (m, 0.0),
                mode => {
                    let (al, ar) = (xl[k].norm(), xr[k].norm());
                    let pan = (ar - al) / (al + ar + 1e-12);
                    let f = focus_weight(focus, self.bin_hz[k], pan);
                    if mode == FocusMode::Isolate {
                        (m * f, m * (1.0 - f))
                    } else {
                        (m * (1.0 - f), m * f)
                    }
                }
            };
            self.main_gain[k] = main * out_gain;
            self.aux_gain[k] = aux * out_gain;
        }

        if self.analysis_enabled {
            self.analyze(d);
        }

        // Synthesis.
        let aux_active = focus.mode != FocusMode::Off;
        for ch in 0..2 {
            let x = &self.spec_hist[d][ch];
            let mut synth = |gains: &[f32], acc: &mut [f32]| {
                for ((y, &x), &g) in self.spec_tmp.iter_mut().zip(x).zip(gains) {
                    *y = x * g;
                }
                self.spec_tmp[0].im = 0.0;
                self.spec_tmp[bins - 1].im = 0.0;
                let _ = self.ifft.process_with_scratch(
                    &mut self.spec_tmp,
                    &mut self.time,
                    &mut self.ifft_scratch,
                );
                for ((a, &t), &win) in acc.iter_mut().zip(&self.time).zip(&self.window) {
                    *a += t * win * self.synth_norm;
                }
            };
            synth(&self.main_gain, &mut self.acc_main[ch]);
            if aux_active {
                synth(&self.aux_gain, &mut self.acc_aux[ch]);
            }
        }

        // Emit one hop and advance all buffers by one hop.
        for ch in 0..2 {
            for (acc, ready) in [
                (&mut self.acc_main[ch], &mut self.ready_main[ch]),
                (&mut self.acc_aux[ch], &mut self.ready_aux[ch]),
            ] {
                ready.copy_from_slice(&acc[..hop]);
                acc.copy_within(hop.., 0);
                acc[n - hop..].fill(0.0);
            }
            self.in_buf[ch].copy_within(hop.., 0);
        }

        self.frames_processed = self.frames_processed.wrapping_add(1);
    }

    /// Per-stem energy and stereo position in log-spaced bands.
    fn analyze(&mut self, frame: usize) {
        let mut acc = [[BandAccum::default(); SOUNDFIELD_BANDS]; NUM_STEMS];
        let [xl, xr] = &self.spec_hist[frame];
        for k in 0..self.num_bins {
            let band = self.bin_band[k];
            if band == NO_BAND {
                continue;
            }
            let (pl, pr) = (xl[k].norm_sqr(), xr[k].norm_sqr());
            let energy = pl + pr;
            let pan = (pr.sqrt() - pl.sqrt()) / (pl.sqrt() + pr.sqrt() + 1e-12);
            for (stem_acc, mask) in acc.iter_mut().zip(&self.masks.masks) {
                let e = energy * mask[k];
                let a = &mut stem_acc[band as usize];
                a.energy += e;
                a.pan_energy += e * pan;
            }
        }

        // A full-scale sine peaks at ~fft_size / π in a single bin with a sqrt-Hann window.
        let reference = (self.fft_size as f32 / std::f32::consts::PI).powi(2);
        for (points, acc) in self.soundfield.points.iter_mut().zip(&acc) {
            for (p, a) in points.iter_mut().zip(acc) {
                p.level_db = 10.0 * (a.energy / reference + 1e-12).log10();
                p.pan = if a.energy > 0.0 {
                    a.pan_energy / a.energy
                } else {
                    0.0
                };
            }
        }
    }
}

#[inline]
fn write_frame(buf: &mut [&mut [f32]], i: usize, l: f32, r: f32) {
    match buf {
        [] => {}
        [mono] => mono[i] = 0.5 * (l + r),
        [left, right, ..] => {
            left[i] = l;
            right[i] = r;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FocusSettings, Stem};

    const SR: f32 = 44_100.0;

    fn test_signal(len: usize) -> (Vec<f32>, Vec<f32>) {
        let mut seed = 7u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        (0..len)
            .map(|i| {
                let t = i as f32 / SR;
                let tone = 0.3 * (std::f32::consts::TAU * 220.0 * t).sin();
                (tone + 0.2 * noise(), 0.5 * tone + 0.2 * noise())
            })
            .unzip()
    }

    /// Runs the engine in irregular block sizes, like a host would.
    fn run(engine: &mut StemEngine, settings: &Settings, l: &[f32], r: &[f32]) -> [Vec<f32>; 4] {
        let (mut ml, mut mr) = (l.to_vec(), r.to_vec());
        let (mut al, mut ar) = (vec![0.0; l.len()], vec![0.0; l.len()]);
        let mut start = 0;
        for block in [1usize, 37, 512, 1000, 64].iter().cycle() {
            if start >= l.len() {
                break;
            }
            let end = (start + block).min(l.len());
            let mut main = [&mut ml[start..end], &mut mr[start..end]];
            let mut aux = [&mut al[start..end], &mut ar[start..end]];
            engine.process(&mut main, Some(&mut aux), settings);
            start = end;
        }
        [ml, mr, al, ar]
    }

    fn max_err(a: &[f32], b: &[f32]) -> f32 {
        a.iter()
            .zip(b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0, f32::max)
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn unity_settings_reconstruct_delayed_input() {
        let mut engine = StemEngine::new(SR);
        let lat = engine.latency_samples();
        let (l, r) = test_signal(SR as usize);
        let [ol, or, al, _] = run(&mut engine, &Settings::default(), &l, &r);

        assert!(
            ol[..lat].iter().all(|&x| x.abs() < 1e-6),
            "output before latency"
        );
        assert!(max_err(&ol[lat..], &l[..l.len() - lat]) < 1e-4);
        assert!(max_err(&or[lat..], &r[..r.len() - lat]) < 1e-4);
        assert!(
            al.iter().all(|&x| x == 0.0),
            "aux must be silent with Focus off"
        );
    }

    #[test]
    fn focus_main_plus_aux_is_the_input() {
        for mode in [FocusMode::Isolate, FocusMode::Extract] {
            let mut engine = StemEngine::new(SR);
            let lat = engine.latency_samples();
            let settings = Settings {
                focus: FocusSettings {
                    mode,
                    low_hz: 150.0,
                    high_hz: 400.0,
                    pan_center: 0.3,
                    pan_width: 0.6,
                },
                ..Default::default()
            };
            let (l, r) = test_signal(SR as usize);
            let [ol, or, al, ar] = run(&mut engine, &settings, &l, &r);
            let sum_l: Vec<f32> = ol.iter().zip(&al).map(|(a, b)| a + b).collect();
            let sum_r: Vec<f32> = or.iter().zip(&ar).map(|(a, b)| a + b).collect();
            assert!(
                max_err(&sum_l[lat..], &l[..l.len() - lat]) < 1e-4,
                "{mode:?}"
            );
            assert!(
                max_err(&sum_r[lat..], &r[..r.len() - lat]) < 1e-4,
                "{mode:?}"
            );
            assert!(rms(&al[lat..]) > 1e-3, "{mode:?}: aux should carry signal");
        }
    }

    #[test]
    fn muting_everything_is_silent() {
        let mut engine = StemEngine::new(SR);
        let mut settings = Settings::default();
        for s in &mut settings.stems {
            s.mute = true;
        }
        let (l, r) = test_signal(SR as usize / 2);
        let [ol, or, ..] = run(&mut engine, &settings, &l, &r);
        assert!(ol.iter().chain(&or).all(|&x| x.abs() < 1e-6));
    }

    #[test]
    fn soloing_bass_keeps_lows_and_drops_highs() {
        let sine = |hz: f32| -> Vec<f32> {
            (0..SR as usize)
                .map(|i| 0.5 * (std::f32::consts::TAU * hz * i as f32 / SR).sin())
                .collect()
        };
        let mut settings = Settings::default();
        settings.stems[Stem::Bass as usize].solo = true;

        let level = |hz: f32| {
            let mut engine = StemEngine::new(SR);
            let lat = engine.latency_samples();
            let x = sine(hz);
            let [ol, ..] = run(&mut engine, &settings, &x, &x);
            rms(&ol[2 * lat..]) / rms(&x[2 * lat..])
        };
        assert!(level(60.0) > 0.6, "bass solo should pass 60 Hz");
        assert!(level(3_000.0) < 0.05, "bass solo should remove 3 kHz");
    }

    #[test]
    fn mono_io_works() {
        let mut engine = StemEngine::new(SR);
        let lat = engine.latency_samples();
        let (x, _) = test_signal(SR as usize / 2);
        let mut y = x.clone();
        engine.process(&mut [&mut y[..]], None, &Settings::default());
        assert!(max_err(&y[lat..], &x[..x.len() - lat]) < 1e-4);
    }

    #[test]
    fn soundfield_tracks_pan() {
        let mut engine = StemEngine::new(SR);
        engine.set_analysis_enabled(true);
        let x: Vec<f32> = (0..SR as usize)
            .map(|i| 0.5 * (std::f32::consts::TAU * 1_000.0 * i as f32 / SR).sin())
            .collect();
        let silent = vec![0.0; x.len()];
        run(&mut engine, &Settings::default(), &silent, &x); // hard right
        let band = crate::soundfield::band_for_hz(1_000.0).unwrap();
        let p = engine.soundfield().points[Stem::Other as usize][band];
        assert!(p.pan > 0.95, "pan = {}", p.pan);
        assert!(p.level_db > -30.0, "level = {}", p.level_db);
    }
}
