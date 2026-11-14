//! Runs a [`MaskModel`] on a worker thread behind the real-time-safe [`Separator`] interface.
//!
//! ```text
//!  audio thread                                   worker thread
//!  ────────────                                   ─────────────
//!  separate(frame k) ──► [gen,k | L,R spectra] ──► context window ──► model.infer()
//!        ▲                    (SPSC ring)                                  │
//!        └──────────── [gen,j | masks for j] ◄───── sanitize ◄──────────────┘
//! ```
//!
//! The audio thread asks for the masks of frame `k - pipeline_frames`, giving the worker
//! `pipeline_frames` hops to finish each inference. If they are late (an *underrun*) the
//! previous masks are reused; nothing on the audio thread ever blocks, unless offline mode is on.
//! Frames carry a generation counter so [`Separator::reset`] can invalidate in-flight work
//! without synchronising with the worker.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use rtrb::{Consumer, Producer, RingBuffer};

use crate::model::{sanitize_masks, MaskModel, ModelError, ModelSpec};
use crate::{Complex32, Separator, StemMasks, NUM_STEMS};

/// `[generation, frame index]`, stored as raw bits in two `f32` slots.
const HEADER: usize = 2;
/// Extra frames of queue space beyond the pipeline depth.
const QUEUE_SLACK_FRAMES: usize = 32;
/// In offline mode, give up waiting for the worker after this long.
const OFFLINE_TIMEOUT: Duration = Duration::from_secs(5);

/// Counters shared with the UI. Written by the audio and worker threads, read from anywhere.
#[derive(Default)]
pub struct SeparatorStats {
    /// Masks that were not ready in time (previous masks reused).
    pub underruns: AtomicU64,
    /// Frames that could not be queued because the worker fell far behind.
    pub dropped_frames: AtomicU64,
    pub inferences: AtomicU64,
    pub last_infer_us: AtomicU32,
    pub max_infer_us: AtomicU32,
    /// Set once the model returned an error; the separator then outputs neutral masks.
    pub failed: AtomicBool,
    pub error: Mutex<Option<String>>,
}

pub struct ThreadedSeparator {
    spec: ModelSpec,
    name: String,
    pipeline_frames: usize,
    in_frame_len: usize,
    out_frame_len: usize,

    to_worker: Producer<f32>,
    from_worker: Consumer<f32>,
    worker_thread: Thread,
    worker: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    stats: Arc<SeparatorStats>,

    generation: u32,
    next_frame: u32,
    /// Masks currently applied, `[NUM_STEMS][num_bins]`.
    current: Vec<f32>,
    offline: bool,
}

impl ThreadedSeparator {
    /// Default worker budget: 2 hops ≈ 46 ms at 44.1 kHz with a 4096-point FFT.
    pub const DEFAULT_PIPELINE_FRAMES: usize = 2;

    /// Start the worker thread. Allocates; call from `initialize`, not from the audio thread.
    pub fn new<M: MaskModel>(model: M, pipeline_frames: usize) -> Result<Self, ModelError> {
        let spec = model.spec();
        spec.validate()?;
        let name = model.name().to_string();

        let in_frame_len = HEADER + spec.frame_len();
        let out_frame_len = HEADER + spec.output_len();
        let queue_frames = pipeline_frames + QUEUE_SLACK_FRAMES;
        let (to_worker, worker_rx) = RingBuffer::new(in_frame_len * queue_frames);
        let (worker_tx, from_worker) = RingBuffer::new(out_frame_len * queue_frames);

        let shutdown = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(SeparatorStats::default());
        let worker = {
            let (shutdown, stats) = (shutdown.clone(), stats.clone());
            thread::Builder::new()
                .name("stems-separator".into())
                .spawn(move || worker_loop(model, worker_rx, worker_tx, &stats, &shutdown))
                .map_err(|e| ModelError::new(format!("failed to spawn worker: {e}")))?
        };

        Ok(Self {
            spec,
            name,
            pipeline_frames,
            in_frame_len,
            out_frame_len,
            to_worker,
            from_worker,
            worker_thread: worker.thread().clone(),
            worker: Some(worker),
            shutdown,
            stats,
            generation: 0,
            next_frame: 0,
            current: vec![1.0 / NUM_STEMS as f32; spec.output_len()],
            offline: false,
        })
    }

    pub fn spec(&self) -> ModelSpec {
        self.spec
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn stats(&self) -> Arc<SeparatorStats> {
        self.stats.clone()
    }

    fn send_frame(&mut self, index: u32, left: &[Complex32], right: &[Complex32]) {
        let header = [f32::from_bits(self.generation), f32::from_bits(index)];
        match self.to_worker.write_chunk_uninit(self.in_frame_len) {
            Ok(chunk) => {
                let payload = left
                    .iter()
                    .map(|c| c.re)
                    .chain(left.iter().map(|c| c.im))
                    .chain(right.iter().map(|c| c.re))
                    .chain(right.iter().map(|c| c.im));
                chunk.fill_from_iter(header.into_iter().chain(payload));
                self.worker_thread.unpark();
            }
            Err(_) => {
                self.stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Pop masks until the ones for frame `wanted` are found. Returns whether they were.
    fn receive_masks(&mut self, wanted: u32) -> bool {
        let deadline = self.offline.then(|| Instant::now() + OFFLINE_TIMEOUT);
        loop {
            let Ok(chunk) = self.from_worker.read_chunk(self.out_frame_len) else {
                // Nothing (complete) queued yet.
                let keep_waiting = deadline.is_some_and(|d| Instant::now() < d)
                    && !self.stats.failed.load(Ordering::Relaxed);
                if keep_waiting {
                    thread::yield_now();
                    continue;
                }
                return false;
            };

            let (a, b) = chunk.as_slices();
            let mut it = a.iter().chain(b);
            let generation = it.next().unwrap().to_bits();
            let index = it.next().unwrap().to_bits();

            if generation != self.generation || index < wanted {
                chunk.commit_all(); // stale
            } else if index == wanted {
                for (dst, src) in self.current.iter_mut().zip(it) {
                    *dst = *src;
                }
                chunk.commit_all();
                return true;
            } else {
                return false; // future frame; leave it queued (uncommitted chunks stay)
            }
        }
    }
}

impl Separator for ThreadedSeparator {
    fn lookahead_frames(&self) -> usize {
        self.spec.lookahead_frames + self.pipeline_frames
    }

    fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.next_frame = 0;
        self.current.fill(1.0 / NUM_STEMS as f32);
    }

    fn set_offline(&mut self, offline: bool) {
        self.offline = offline;
    }

    fn separate(&mut self, left: &[Complex32], right: &[Complex32], masks: &mut StemMasks) {
        let num_bins = self.spec.num_bins();
        debug_assert_eq!(left.len(), num_bins, "engine FFT size must match the model");

        let index = self.next_frame;
        self.next_frame = index.wrapping_add(1);
        self.send_frame(index, left, right);

        // The first `pipeline_frames` calls have no masks due yet; keep the neutral ones.
        if let Some(wanted) = index.checked_sub(self.pipeline_frames as u32) {
            if !self.receive_masks(wanted) {
                self.stats.underruns.fetch_add(1, Ordering::Relaxed);
            }
        }

        for (s, out) in masks.masks.iter_mut().enumerate() {
            out.copy_from_slice(&self.current[s * num_bins..(s + 1) * num_bins]);
        }
    }
}

impl Drop for ThreadedSeparator {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.worker_thread.unpark();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn worker_loop<M: MaskModel>(
    mut model: M,
    mut rx: Consumer<f32>,
    mut tx: Producer<f32>,
    stats: &SeparatorStats,
    shutdown: &AtomicBool,
) {
    let spec = model.spec();
    let (num_bins, frame_len, input_len) = (spec.num_bins(), spec.frame_len(), spec.input_len());
    let in_frame_len = HEADER + frame_len;
    let out_frame_len = HEADER + spec.output_len();

    let mut context = vec![0.0f32; input_len];
    let mut masks = vec![0.0f32; spec.output_len()];
    let mut generation = None;

    while !shutdown.load(Ordering::Acquire) {
        let Ok(chunk) = rx.read_chunk(in_frame_len) else {
            thread::park_timeout(Duration::from_millis(5));
            continue;
        };

        let (a, b) = chunk.as_slices();
        let mut it = a.iter().chain(b);
        let header = [*it.next().unwrap(), *it.next().unwrap()];
        if generation != Some(header[0].to_bits()) {
            generation = Some(header[0].to_bits());
            context.fill(0.0);
        }
        context.copy_within(frame_len.., 0);
        for (dst, src) in context[input_len - frame_len..].iter_mut().zip(it) {
            *dst = *src;
        }
        chunk.commit_all();

        let start = Instant::now();
        let ok = !stats.failed.load(Ordering::Relaxed)
            && match model.infer(&context, &mut masks) {
                Ok(()) => true,
                Err(e) => {
                    stats.failed.store(true, Ordering::Relaxed);
                    *stats.error.lock().unwrap() = Some(e.to_string());
                    false
                }
            };
        if !ok {
            masks.fill(0.0); // sanitised to an even split => transparent at unity gains
        }
        sanitize_masks(&mut masks, num_bins);

        let us = start.elapsed().as_micros().min(u32::MAX as u128) as u32;
        stats.last_infer_us.store(us, Ordering::Relaxed);
        stats.max_infer_us.fetch_max(us, Ordering::Relaxed);
        stats.inferences.fetch_add(1, Ordering::Relaxed);

        // If the audio thread stopped reading (e.g. transport stopped), drop the result.
        if let Ok(out) = tx.write_chunk_uninit(out_frame_len) {
            out.fill_from_iter(header.into_iter().chain(masks.iter().copied()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Settings, Stem, StemEngine};

    const BINS: usize = 33; // fft_size 64

    fn spec(context: usize, lookahead: usize) -> ModelSpec {
        ModelSpec {
            sample_rate: 44_100.0,
            fft_size: 64,
            context_frames: context,
            lookahead_frames: lookahead,
        }
    }

    /// Writes the *target frame's* bin-0 real value into the bass mask of bin 1, so the test can
    /// check which frame the returned masks belong to.
    struct MarkerModel {
        spec: ModelSpec,
        delay: Duration,
    }

    impl MaskModel for MarkerModel {
        fn spec(&self) -> ModelSpec {
            self.spec
        }

        fn infer(&mut self, context: &[f32], masks: &mut [f32]) -> Result<(), ModelError> {
            thread::sleep(self.delay);
            let target = self.spec.context_frames - 1 - self.spec.lookahead_frames;
            let marker = context[target * self.spec.frame_len()]; // plane 0 (L.re), bin 0
            masks.fill(0.0);
            masks[Stem::Bass as usize * BINS + 1] = marker;
            masks[Stem::Other as usize * BINS + 1] = 1.0 - marker;
            for k in [0].into_iter().chain(2..BINS) {
                masks[Stem::Other as usize * BINS + k] = 1.0;
            }
            Ok(())
        }
    }

    fn frame(marker: f32) -> Vec<Complex32> {
        let mut v = vec![Complex32::new(0.0, 0.0); BINS];
        v[0].re = marker;
        v
    }

    fn marker_of(m: &StemMasks) -> f32 {
        m.masks[Stem::Bass as usize][1]
    }

    #[test]
    fn masks_line_up_with_reported_lookahead() {
        let (ctx, la, pipe) = (6, 2, 3);
        let model = MarkerModel {
            spec: spec(ctx, la),
            delay: Duration::ZERO,
        };
        let mut sep = ThreadedSeparator::new(model, pipe).unwrap();
        sep.set_offline(true);
        assert_eq!(sep.lookahead_frames(), la + pipe);

        let mut masks = StemMasks::new(BINS);
        let marker = |k: usize| (k % 97) as f32 / 100.0 + 0.01;
        for k in 0..200 {
            sep.separate(&frame(marker(k)), &frame(0.0), &mut masks);
            if k >= sep.lookahead_frames() {
                let expected = marker(k - sep.lookahead_frames());
                assert!((marker_of(&masks) - expected).abs() < 1e-6, "frame {k}");
            }
        }
        assert_eq!(sep.stats().underruns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn reset_restarts_alignment() {
        let model = MarkerModel {
            spec: spec(4, 1),
            delay: Duration::ZERO,
        };
        let mut sep = ThreadedSeparator::new(model, 2).unwrap();
        sep.set_offline(true);
        let mut masks = StemMasks::new(BINS);
        for k in 0..50 {
            sep.separate(&frame(0.5 + k as f32 / 1000.0), &frame(0.0), &mut masks);
        }
        sep.reset();
        let la = sep.lookahead_frames();
        for k in 0..20 {
            sep.separate(&frame(0.1 + k as f32 / 100.0), &frame(0.0), &mut masks);
            if k >= la {
                let expected = 0.1 + (k - la) as f32 / 100.0;
                assert!((marker_of(&masks) - expected).abs() < 1e-6, "frame {k}");
            } else if k < 2 {
                assert_eq!(
                    marker_of(&masks),
                    0.25,
                    "neutral masks before the pipeline fills"
                );
            }
        }
    }

    #[test]
    fn realtime_mode_never_blocks_on_a_slow_model() {
        let model = MarkerModel {
            spec: spec(4, 1),
            delay: Duration::from_millis(20),
        };
        let mut sep = ThreadedSeparator::new(model, 1).unwrap();
        let mut masks = StemMasks::new(BINS);
        let start = Instant::now();
        for k in 0..20 {
            sep.separate(&frame(k as f32 / 100.0), &frame(0.0), &mut masks);
            for b in 0..BINS {
                let sum: f32 = masks.masks.iter().map(|m| m[b]).sum();
                assert!((sum - 1.0).abs() < 1e-5);
            }
        }
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "audio thread blocked"
        );
        assert!(sep.stats().underruns.load(Ordering::Relaxed) > 0);
    }

    struct FailingModel(ModelSpec);
    impl MaskModel for FailingModel {
        fn spec(&self) -> ModelSpec {
            self.0
        }
        fn infer(&mut self, _: &[f32], _: &mut [f32]) -> Result<(), ModelError> {
            Err(ModelError::new("boom"))
        }
    }

    #[test]
    fn model_errors_degrade_to_transparent() {
        let mut engine = StemEngine::with_separator(
            44_100.0,
            64,
            Box::new(ThreadedSeparator::new(FailingModel(spec(2, 0)), 1).unwrap()),
        );
        engine.set_offline(true);
        let lat = engine.latency_samples();
        let x: Vec<f32> = (0..4000)
            .map(|i| ((i * 7919) % 101) as f32 / 101.0 - 0.5)
            .collect();
        let (mut l, mut r) = (x.clone(), x.clone());
        engine.process(&mut [&mut l, &mut r], None, &Settings::default());
        let err = l[lat..]
            .iter()
            .zip(&x)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(err < 1e-4, "err {err}");
    }

    /// The marker model above, but with a sensible STFT size, through the full engine.
    struct LowpassModel(ModelSpec);
    impl MaskModel for LowpassModel {
        fn spec(&self) -> ModelSpec {
            self.0
        }
        fn infer(&mut self, _: &[f32], masks: &mut [f32]) -> Result<(), ModelError> {
            let f = self.0.num_bins();
            let bin_hz = self.0.sample_rate / self.0.fft_size as f32;
            masks.fill(0.0);
            for k in 0..f {
                let s = if (k as f32 * bin_hz) < 200.0 {
                    Stem::Bass
                } else {
                    Stem::Other
                };
                masks[s as usize * f + k] = 1.0;
            }
            Ok(())
        }
    }

    #[test]
    fn engine_with_threaded_model_reconstructs_and_separates() {
        let spec = ModelSpec {
            sample_rate: 44_100.0,
            fft_size: 2048,
            context_frames: 4,
            lookahead_frames: 1,
        };
        let make = || {
            let sep = ThreadedSeparator::new(LowpassModel(spec), 2).unwrap();
            let mut e = StemEngine::with_separator(44_100.0, 2048, Box::new(sep));
            e.set_offline(true);
            e
        };
        let sine = |hz: f32| -> Vec<f32> {
            (0..44_100)
                .map(|i| (std::f32::consts::TAU * hz * i as f32 / 44_100.0).sin())
                .collect()
        };
        let rms = |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();

        // Transparent at unity.
        let mut e = make();
        let lat = e.latency_samples();
        assert_eq!(lat, 2048 + 3 * 512);
        let x = sine(440.0);
        let (mut l, mut r) = (x.clone(), x.clone());
        e.process(&mut [&mut l, &mut r], None, &Settings::default());
        let err = l[lat..]
            .iter()
            .zip(&x)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(err < 1e-4, "err {err}");

        // Muting bass removes 60 Hz and keeps 2 kHz.
        let mut settings = Settings::default();
        settings.stems[Stem::Bass as usize].mute = true;
        for (hz, keep) in [(60.0, false), (2_000.0, true)] {
            let mut e = make();
            let x = sine(hz);
            let (mut l, mut r) = (x.clone(), x.clone());
            e.process(&mut [&mut l, &mut r], None, &settings);
            let ratio = rms(&l[2 * lat..]) / rms(&x[2 * lat..]);
            assert_eq!(ratio > 0.9, keep, "{hz} Hz ratio {ratio}");
        }
    }
}
