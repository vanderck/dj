# Model contract (v1)

The small, fast separation models dj uses as a fallback when HT-Demucs is not set
(Settings → Stems → Model) follow this contract: "masks in the STFT domain". `stems-core` owns
the STFT; the model only estimates how much of each time–frequency bin belongs to each stem.

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
  the stream. The test `frames_match_the_documented_stft` in `crates/stems-core/src/offline.rs`
  checks this.
- Before the first `T` frames exist, the missing (older) frames are zeros.

## Output

| | |
|---|---|
| dtype | `float32` |
| shape | `[1, 4, F]` (any shape with `4·F` elements is accepted) |

- Soft masks for the frame at index `T − 1 − lookahead_frames` of the input window.
- Stem order: **drums, bass, vocals, other**.
- Values are clamped to `[0, 1]` and renormalised to sum to 1 per bin, so a
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

Set them with `onnx.helper.set_model_props(model, {...})`.

## Sample rate

Tracks at other rates are resampled to `stems.sample_rate` and the stems back.

## Training

The models v4/v5 were trained with a PyTorch pipeline (distilled from HT-Demucs), which lived in
the `models/` folder of the former vanderck/stems repository; it is in this repository's history
(the merge of stems-core). Any architecture works if it is exported with a fixed batch of 1, the
shapes and metadata above (opset ≥ 17), and only looks `lookahead_frames` frames past the
target frame.
