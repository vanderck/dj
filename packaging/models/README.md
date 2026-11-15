# Bundled stem-separation model

`cargo xtask package-macos` puts `stems-model.onnx` from this folder into
`Collective.app/Contents/Resources/`. Collective uses it whenever no model is set in
Settings → Stems or `STEMS_MODEL`. Pass `--no-model` to leave it out, or `--model other.onnx`
to bundle a different one.

## Current model: "Demo model (synthetic)"

**This is a pipeline demo, not a usable separator for real music.**

| | |
|---|---|
| Architecture | `StemsNet` from [vanderck/stems](https://github.com/vanderck/stems) (`models/`), default size: 1.8 M parameters, dim 64, dilations 4,2,2,1 |
| Contract | v1: 44.1 kHz, FFT 4096, 19 context frames, 3 lookahead frames (~209 ms total plugin latency) |
| Training data | Synthetic toy stems only (`stems_train.synth`: sine bass, noise drums, harmonic "vocals", pads) |
| Training | 200 steps on CPU, batch 4, 2 s segments, compressed-L1 + 0.05 × negative-SDR loss |
| Quality | +4.7 dB mean SDR on synthetic validation (equal-split floor +1.2 dB, oracle ceiling +11.9 dB). Untested on real music; expect heavy bleed. |
| Licence | Trained only on generated audio, no third-party data or weights |

It exists so the full path (ONNX Runtime on Apple Silicon, background separation, caching, deck
controls) can be tried in a packaged build. Replace it with a model trained on real stems: see
`models/README.md` in the stems repository (licensed multitracks, or distillation from
HT-Demucs FT — check that teacher's licence before shipping).

To replace: export with `python -m stems_train.export best.pt stems-model.onnx --name "…"` and
overwrite the file here. Intel Macs cannot run models (no prebuilt ONNX Runtime); they use the
placeholder separator.
