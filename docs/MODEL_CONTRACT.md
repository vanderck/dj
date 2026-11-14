# Model contract (v1)

Any ONNX file that follows this contract can be dropped into the plugin. The contract is
deliberately "masks in the STFT domain": the engine owns the STFT, the model only estimates how
much of each time–frequency bin belongs to each stem.

## Input

| | |
|---|---|
| name | any (the first graph input is used) |
| dtype | `float32` |
| shape | `[1, T, 4, F]` |

- `F = fft_size / 2 + 1` frequency bins.
- `T = context_frames`, oldest frame first. The window ends at the newest frame.
- The 4 planes are `L.re, L.im, R.re, R.im` of an **unnormalised** real FFT
  (`numpy.fft.rfft` convention) of `x * sqrt_hann`, where
  `sqrt_hann[n] = sqrt(0.5 - 0.5 cos(2πn / fft_size))` (periodic).
- Hop size is always `fft_size / 4`.
- Frame `t` covers input samples `[(t+1)·hop − fft_size, (t+1)·hop)`, zeros before the start of
  the stream. `models/tools/stft.py` reproduces this exactly and
  `crates/stems-core/tests/contract.rs` checks the engine against it.
- Before the first `T` frames exist, the missing (older) frames are zeros.

## Output

| | |
|---|---|
| dtype | `float32` |
| shape | `[1, 4, F]` (any shape with `4·F` elements is accepted) |

- Soft masks for the frame at index `T − 1 − lookahead_frames` of the input window.
- Stem order: **drums, bass, vocals, other**.
- Values are clamped to `[0, 1]` and renormalised to sum to 1 per bin by the engine, so a
  `Softmax` over the stem axis is the natural last layer. Non-finite values become 0.
- Masks are applied to both channels.

## Metadata (ONNX `metadata_props`)

| key | example | meaning |
|---|---|---|
| `stems.contract` | `1` | contract version (required) |
| `stems.sample_rate` | `44100` | training sample rate (required) |
| `stems.fft_size` | `4096` | STFT size, power of two (required) |
| `stems.context_frames` | `8` | `T` (required) |
| `stems.lookahead_frames` | `2` | future frames in the window, `< T` (required) |
| `stems.name` | `my-model v3` | shown in the UI (optional) |

Set them with `onnx.helper.set_model_props(model, {...})`. See `models/tools/make_test_model.py`.

## Latency

```
latency = fft_size + (lookahead_frames + pipeline_frames) · hop
```

`pipeline_frames` (default 2, env `STEMS_PIPELINE_FRAMES`) is the time budget given to the
inference worker. Examples at 44.1 kHz:

| fft_size | lookahead | pipeline | latency |
|---|---|---|---|
| 4096 | 2 | 2 | 8192 samples ≈ 186 ms (test model) |
| 2048 | 4 | 2 | 5120 samples ≈ 116 ms |
| 4096 | 4 | 2 | 10240 samples ≈ 232 ms (≈ PEEL STEMS 2's 245 ms) |

Per-frame compute budget: one hop (`fft_size/4 / sample_rate`, e.g. 23 ms for 4096 at 44.1 kHz)
on average, with up to `pipeline_frames` hops of slack for spikes.

## Sample rate

The model runs only when the host rate is within 1 % of `stems.sample_rate`; otherwise the
plugin falls back to the placeholder separator and says why in the UI. Internal resampling is on
the roadmap.

## Training checklist

1. Compute input frames with `models/tools/stft.py` (`stft_frames` + `context_windows`).
2. Targets: ideal ratio masks or, better, train end-to-end through the mask application:
   `stem_hat = mask_s · mixture_spec`, loss on the reconstructed stem spectra/waveforms.
3. Respect causality: the network may only see `lookahead_frames` frames after the target.
4. Export with a fixed batch of 1 and the shapes above (opset ≥ 17), add the metadata, then run
   `cargo test -p stems-core --features onnx` against it (copy one of the tests in
   `crates/stems-core/tests/onnx.rs`) and the benchmark:
   `cargo run --release -p stems-core --features onnx --example bench -- model.onnx`.
