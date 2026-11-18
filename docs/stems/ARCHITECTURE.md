# Architecture

## Signal flow

Everything happens in `stems_core::StemEngine`, once per STFT hop (`fft_size / 4` samples):

```
host block ─► input FIFO ─► sqrt-Hann ─► FFT (L, R) ─► Separator ─► 4 soft masks
                                            │                          │
                                            ▼                          ▼
                                 spectrum delay line ──► per-bin gain  M = Σ_s g_s · m_s^p_s
                                 (lookahead frames)                    │
                                                                       ▼
                                                     Focus EQ weight F(freq, pan)
                                                       main = M·F or M·(1−F)
                                                       aux  = the complement
                                                                       │
                          host block ◄─ output FIFO ◄─ overlap-add ◄─ IFFT · sqrt-Hann
```

- `g_s`: stem gain after mute/solo. `p_s = 1 + 4 · isolation_s`.
- Separators must output masks that sum to 1 per bin. With all gains at unity and isolation 0,
  `M = 1` and the output is exactly the delayed input (tested in `engine.rs`).
- Isolation raises masks to a power, which suppresses low-confidence (bleed) energy. Because
  `Σ m^p < 1` for `p > 1`, isolation is slightly audible even with every stem on — the same
  behaviour PEEL STEMS documents for its Isolation control.
- The Focus EQ splits the *already mixed* stems, so main + aux always equals the stem mix.
- Only one IFFT per channel (two with Focus active): gains are combined in the frequency domain
  instead of synthesising each stem separately.

## Latency

`latency = fft_size + lookahead_frames · hop`

| Host rate | FFT | Latency (placeholder separator) |
|---|---|---|
| 44.1 / 48 kHz | 4096 | 4096 samples (93 / 85 ms) |
| 88.2 / 96 kHz | 8192 | 8192 samples |
| 176.4 / 192 kHz | 16384 | 16384 samples |

The plugin reports this via `set_latency_samples` in `initialize`, so the DAW compensates.
A model with lookahead (e.g. ~6 hops ≈ 140 ms at 44.1 kHz) adds to this; the total for a
PEEL-STEMS-class result is roughly 250 ms.

## Real-time rules

- `StemEngine::new` allocates; it is only called from `Plugin::initialize`.
- `process` / `reset` never allocate, lock or block. Build with
  `--features stems-plugin/assert-no-alloc` to make debug builds panic if they do.
- The soundfield is handed to the GUI through a `triple_buffer` (wait-free on the audio side).
  It is only computed while the editor is open.

## ML model path

```
StemEngine ──► ThreadedSeparator (audio thread side, real-time safe)
                  │  rtrb SPSC rings, frames tagged [generation, index]
                  ▼
               worker thread ──► MaskModel::infer ──► OnnxModel (ONNX Runtime via `ort`)
```

- `MaskModel` (`model.rs`): any model with a `ModelSpec` (sample rate, FFT size, context,
  lookahead) and an `infer(context, masks)` method. Runs on the worker, may allocate.
- `ThreadedSeparator` (`threaded.rs`): implements `Separator`. Each hop it queues the new frame
  and takes the masks for the frame `pipeline_frames` hops earlier. Late masks → the previous
  masks are reused and `SeparatorStats::underruns` increments; the audio thread never waits.
  In **offline** mode (host bounce, `ProcessMode::Offline`) it waits instead, so renders are
  always fully separated.
- `reset()` bumps a generation counter; stale in-flight frames and masks are discarded by tag,
  so no locking with the worker is needed.
- Model errors are latched (`SeparatorStats::failed` / `error`) and the separator degrades to
  neutral masks, which is transparent at unity gains.
- `OnnxModel` (`onnx.rs`): reads `ModelSpec` from ONNX metadata, does a dry run at load time to
  catch shape errors early. Features: `onnx` (download + static link) or `onnx-dynamic`
  (load `ORT_DYLIB_PATH` at runtime; a missing library is an error, not a panic).

The model contract is specified in [`MODEL_CONTRACT.md`](MODEL_CONTRACT.md).

### Next steps for performance

- Execution providers: Core ML (Apple Neural Engine) on macOS, DirectML or OpenVINO (NPU) on
  Windows — `ort` features `coreml`, `directml`, `openvino` plus `SessionBuilder::with_execution_providers`.
- Batching several hops per inference trades latency for throughput on slow machines; raise
  `pipeline_frames` accordingly.
- Stateful (recurrent) models can avoid recomputing the whole context window every hop.
