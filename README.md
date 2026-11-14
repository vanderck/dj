# Stems

Real-time stem separation plugin for DAWs, written in Rust. Insert it on a mixed track and get
live control over four stems — **Drums, Bass, Vocals, Other** — plus a Focus EQ that isolates or
removes a frequency range and stereo region, with the complement routed to a second output.

> **Status: early.** The full signal path, parameters, latency reporting, UI, host integration
> and the **ONNX model runtime** work. There is no trained separation model yet: without one the
> plugin uses a placeholder heuristic, and the bundled test model is hand-written and untrained.
> Expect heavy bleed between stems until a real model is trained (see [Roadmap](#roadmap)).

## Features

| Feature | Status |
|---|---|
| 4 stems with level, mute, solo | ✅ |
| Per-stem **Isolation** (0 = transparent, higher = stricter separation) | ✅ |
| **Focus EQ**: frequency range × stereo position, *Isolate* or *Extract* | ✅ |
| **Dual output**: Focus complement on a stereo aux port named "Focus" | ✅ (host must support plugin aux outputs) |
| **Soundfield display**: each stem's energy by frequency and stereo position | ✅ |
| Latency reported to the host for automatic delay compensation | ✅ (4096 samples ≈ 93 ms at 44.1 kHz) |
| Mono and stereo, 44.1–192 kHz | ✅ |
| CLAP and VST3 | ✅ |
| AU | ⏳ via [clap-wrapper](https://github.com/free-audio/clap-wrapper) |
| AAX | ❌ needs the Avid C++ SDK |
| ONNX model runtime on a worker thread (underrun-safe, offline bounce mode) | ✅ |
| Trained separation model | ⏳ see roadmap |

## Layout

```
crates/stems-core     DSP engine: STFT, Separator trait, stem mixing, Focus EQ, soundfield,
                      threaded model runner, ONNX backend. No plugin dependencies; tested.
crates/stems-plugin   nih-plug glue: parameters, IO layouts, latency, egui editor.
xtask                 `cargo xtask bundle` → .clap / .vst3 bundles.
models/               PyTorch training pipeline, ONNX export, teacher-distillation tool.
docs/ARCHITECTURE.md  Signal flow, real-time rules, threading.
docs/MODEL_CONTRACT.md  Tensor layout, STFT conventions and metadata a model must follow.
```

## Build

Requires stable Rust (1.80+). On Linux, install the GUI headers first:

```bash
sudo apt-get install -y libasound2-dev libgl-dev libjack-dev libx11-xcb-dev libxcb1-dev libxcb-dri2-0-dev libxcb-icccm4-dev libxcursor-dev libxkbcommon-dev libxcb-shape0-dev libxcb-xfixes0-dev
```

Test the engine:

```bash
cargo test --workspace
```

Measure CPU cost:

```bash
cargo run --release -p stems-core --example bench
```

Build plugin bundles into `target/bundled/`:

```bash
cargo xtask bundle stems-plugin --release
```

On macOS, build a universal binary:

```bash
cargo xtask bundle-universal stems-plugin --release
```

Copy `Stems.clap` / `Stems.vst3` into your system's plugin folder and rescan in your DAW.

## Running a model

Build with ONNX support (downloads ONNX Runtime and links it statically):

```bash
cargo xtask bundle stems-plugin --release --features onnx
```

Then either point the plugin at a model file (set before starting the DAW):

```bash
export STEMS_MODEL=/path/to/model.onnx
```

or compile the model into the plugin binary:

```bash
STEMS_EMBED_MODEL=/path/to/model.onnx cargo xtask bundle stems-plugin --release --features onnx
```

The editor shows which separator is active, inference time and underruns; if the model can't be
used (missing, wrong sample rate, invalid) it says why and falls back to the placeholder.

To try the path end to end, use the bundled test model at
`crates/stems-core/tests/data/test_model.onnx` (44.1 kHz only). Regenerate it with:

```bash
pip install -r models/requirements.txt
```

```bash
python models/tools/make_test_model.py crates/stems-core/tests/data/test_model.onnx
```

If ONNX Runtime can't be downloaded at build time, use `--features onnx-dynamic` and set
`ORT_DYLIB_PATH` to an existing `libonnxruntime` (e.g. from the `onnxruntime` pip package).

## Licensing note (read before shipping)

nih-plug itself is ISC, but its **VST3 bindings (`vst3-sys`) are GPLv3**, so any binary built with
the `vst3` feature must be distributed under the GPLv3. For a closed-source product either:

- ship **CLAP only** (`cargo xtask bundle stems-plugin --release --no-default-features`) and wrap to
  VST3/AU with clap-wrapper, or
- replace the VST3 export with bindings under a compatible license.

Also check the license of any pretrained model weights: most public separation models are
trained on MUSDB18, which is for non-commercial research only.

## Roadmap

1. ~~**Model integration**~~ — done: `ThreadedSeparator` + `OnnxModel`, see
   [docs/MODEL_CONTRACT.md](docs/MODEL_CONTRACT.md). Next: Core ML / DirectML execution providers.
2. **Streaming low-latency model** — pipeline in place ([models/README.md](models/README.md)):
   band-split network with ~209 ms total latency, trainable on licensed multitracks or distilled
   from HT-Demucs FT. Next: train on real data and evaluate.
3. **Fixed internal sample rate** — resample to the model rate (e.g. 44.1 kHz) internally so
   quality and latency are independent of the host rate.
4. **AU** via clap-wrapper, code signing and notarization, installers.
5. **UI polish** — custom stem faders, Focus EQ editing by dragging on the soundfield.
