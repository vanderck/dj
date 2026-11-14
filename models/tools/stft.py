"""Reference STFT matching stems-core exactly. Use it in training pipelines.

The engine computes, per hop of fft_size // 4 samples, for each channel:
    X = rfft(x[n : n + fft_size] * sqrt_hann)        (unnormalised, numpy convention)
and feeds the model frames shaped [4, F] = [L.re, L.im, R.re, R.im].

Engine frame t covers input samples [(t + 1) * hop - fft_size, (t + 1) * hop), with zeros before
the start of the stream. `stft_frames` reproduces that exactly (it left-pads by fft_size - hop),
so frame t here is the frame the engine hands the model at its t-th hop. This is checked by
crates/stems-core/tests/contract.rs.
"""
import numpy as np


def sqrt_hann(fft_size: int) -> np.ndarray:
    n = np.arange(fft_size)
    return np.sqrt(0.5 - 0.5 * np.cos(2.0 * np.pi * n / fft_size)).astype(np.float32)


def stft_frames(stereo: np.ndarray, fft_size: int) -> np.ndarray:
    """stereo: [2, samples] float32 -> [frames, 4, fft_size // 2 + 1] float32."""
    hop = fft_size // 4
    win = sqrt_hann(fft_size)
    stereo = np.pad(stereo, ((0, 0), (fft_size - hop, 0)))
    n_frames = 1 + (stereo.shape[1] - fft_size) // hop
    out = np.empty((n_frames, 4, fft_size // 2 + 1), dtype=np.float32)
    for t in range(n_frames):
        seg = stereo[:, t * hop : t * hop + fft_size] * win
        spec = np.fft.rfft(seg, axis=-1)
        out[t, 0], out[t, 1] = spec[0].real, spec[0].imag
        out[t, 2], out[t, 3] = spec[1].real, spec[1].imag
    return out


def context_windows(frames: np.ndarray, context: int) -> np.ndarray:
    """[frames, 4, F] -> [frames, context, 4, F]; window i ends at frame i (zero-padded), the
    model input for the engine's i-th frame."""
    pad = np.zeros((context - 1,) + frames.shape[1:], dtype=frames.dtype)
    padded = np.concatenate([pad, frames])
    return np.stack([padded[i : i + context] for i in range(frames.shape[0])])
