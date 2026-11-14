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

## Plugging in an ML model

Implement `stems_core::Separator`:

```rust
pub trait Separator: Send {
    fn lookahead_frames(&self) -> usize { 0 }
    fn reset(&mut self) {}
    fn separate(&mut self, left: &[Complex32], right: &[Complex32], masks: &mut StemMasks);
}
```

Recommended shape for a neural model:

1. **Export** a mask-estimating network to ONNX (or Core ML) that takes a window of magnitude
   spectra (`T × bins × 2`) and returns masks for the frame `lookahead_frames` in the past.
   Keep the STFT outside the model — it is already done here, and STFT ops export poorly.
2. **Worker thread**: inference runtimes allocate and their latency jitters, so do not call them
   on the audio thread. `separate` pushes the newest frame into a lock-free SPSC ring (e.g.
   `rtrb`) and pops the masks that are due; the worker batches frames and runs `ort::Session`.
3. **Budget**: the worker has `hop / sample_rate` (≈ 23 ms at 44.1 kHz, hop 1024) per frame on
   average; batching several hops trades latency for throughput. Add the batch size to
   `lookahead_frames` so the host compensates for it.
4. **Underruns**: if masks are not ready in time, fall back to the last masks (or pass-through)
   rather than blocking, and surface a "CPU overload" flag in the UI.
5. **Execution providers**: Core ML (Apple Neural Engine) on macOS, DirectML or OpenVINO (NPU) on
   Windows, CPU everywhere else. Choose the block size per machine — slower hardware needs
   bigger batches and therefore more latency.
