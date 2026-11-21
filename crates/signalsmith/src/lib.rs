//! [Signalsmith Stretch](https://signalsmith-audio.co.uk/code/stretch/) (Geraint Luff, MIT):
//! polyphonic time-stretch and pitch-shift, vendored as C++ (`vendor/`) and wrapped for dj's
//! decks: stereo, interleaved, the default preset with its work spread over the calls.

use std::os::raw::c_int;

#[repr(C)]
struct Raw {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn dj_stretch_new(sample_rate: f32) -> *mut Raw;
    fn dj_stretch_free(h: *mut Raw);
    fn dj_stretch_reset(h: *mut Raw);
    fn dj_stretch_input_latency(h: *const Raw) -> c_int;
    fn dj_stretch_output_latency(h: *const Raw) -> c_int;
    fn dj_stretch_block(h: *const Raw) -> c_int;
    fn dj_stretch_transpose(h: *mut Raw, factor: f32);
    fn dj_stretch_seek(h: *mut Raw, input: *mut f32, frames: c_int, rate: f64);
    fn dj_stretch_process(
        h: *mut Raw,
        input: *mut f32,
        in_frames: c_int,
        output: *mut f32,
        out_frames: c_int,
    );
}

/// A stereo stretcher. Allocates only in [`Stretch::new`]; the rest is realtime-safe.
pub struct Stretch {
    raw: *mut Raw,
}

// The C++ object has no thread affinity; `&mut self` keeps it to one thread at a time.
unsafe impl Send for Stretch {}

impl Stretch {
    pub fn new(sample_rate: f32) -> Self {
        let raw = unsafe { dj_stretch_new(sample_rate) };
        assert!(!raw.is_null());
        Self { raw }
    }

    /// Forget everything (silence in, silence out).
    pub fn reset(&mut self) {
        unsafe { dj_stretch_reset(self.raw) }
    }

    /// Input frames between the newest input and the output it becomes.
    pub fn input_latency(&self) -> usize {
        unsafe { dj_stretch_input_latency(self.raw) as usize }
    }

    /// Output frames between processing and hearing it.
    pub fn output_latency(&self) -> usize {
        unsafe { dj_stretch_output_latency(self.raw) as usize }
    }

    /// How much input [`Stretch::seek`] wants: a block and an interval.
    pub fn seek_frames(&self) -> usize {
        unsafe { dj_stretch_block(self.raw) as usize }
    }

    /// Pitch: a frequency multiplier (1 = unchanged).
    pub fn set_transpose(&mut self, factor: f32) {
        unsafe { dj_stretch_transpose(self.raw, factor) }
    }

    /// Start over at a new place: `input` (interleaved stereo) is the audio just before it.
    /// `rate`: input frames per output frame from here on.
    pub fn seek(&mut self, input: &mut [f32], rate: f64) {
        unsafe {
            dj_stretch_seek(
                self.raw,
                input.as_mut_ptr(),
                (input.len() / 2) as c_int,
                rate,
            )
        }
    }

    /// Stretch `input` (interleaved stereo) into `output`: the ratio of their lengths is the
    /// speed.
    pub fn process(&mut self, input: &mut [f32], output: &mut [f32]) {
        unsafe {
            dj_stretch_process(
                self.raw,
                input.as_mut_ptr(),
                (input.len() / 2) as c_int,
                output.as_mut_ptr(),
                (output.len() / 2) as c_int,
            )
        }
    }
}

impl Drop for Stretch {
    fn drop(&mut self) {
        unsafe { dj_stretch_free(self.raw) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f32, n: usize, sr: f32) -> Vec<f32> {
        (0..n)
            .flat_map(|i| {
                let v = 0.5 * (std::f32::consts::TAU * hz * i as f32 / sr).sin();
                [v, v]
            })
            .collect()
    }

    /// Frequency of a (mono) signal from its zero crossings.
    fn pitch(x: &[f32], sr: f32) -> f32 {
        let c: Vec<usize> = (1..x.len())
            .filter(|&i| x[i - 1] < 0.0 && x[i] >= 0.0)
            .collect();
        (c.len() - 1) as f32 * sr / (c[c.len() - 1] - c[0]) as f32
    }

    #[test]
    fn it_stretches_and_transposes() {
        let sr = 48_000.0;
        let mut s = Stretch::new(sr);
        assert!(s.input_latency() > 0 && s.output_latency() > 0);
        let mut input = sine(440.0, 48_000, sr);
        // Half speed, a fifth up.
        s.set_transpose(1.5);
        let mut out = vec![0.0; 96_000 * 2];
        for (i, o) in input.chunks_mut(2 * 256).zip(out.chunks_mut(2 * 512)) {
            s.process(i, o);
        }
        let mono: Vec<f32> = out.chunks(2).map(|f| f[0]).skip(20_000).collect();
        let p = pitch(&mono[..40_000], sr);
        assert!((p - 660.0).abs() < 3.0, "{p}");
    }
}
