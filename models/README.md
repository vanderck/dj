# Training the separation model

PyTorch pipeline that trains the real-time mask estimator and exports it to ONNX following the
[model contract](../docs/MODEL_CONTRACT.md). It runs on Apple Silicon (MPS), CUDA or CPU.

```
stems_train/
  config.py   ModelConfig (architecture + STFT + latency) and TrainConfig
  stft.py     STFT identical to the plugin engine (checked against tools/stft.py)
  model.py    StemsNet: band-split, valid dilated temporal convs, band mixing, mask heads
  data.py     synthetic stems, or folders of real / teacher stems (wav or flac)
  losses.py   compressed-spectrum L1 loss, spectral SDR metric
  train.py    training loop with checkpoints, eval and JSON logs
  export.py   ONNX export + metadata + ONNX Runtime parity check
tools/
  make_teacher_stems.py  distillation data from HT-Demucs FT (or another demucs model)
  stft.py                numpy reference of the engine's STFT
  make_test_model.py     the hand-written test model used by the Rust tests
tests/        pytest suite (STFT parity, train/deploy equivalence, export, data, teacher tool)
```

## Setup (macOS)

From the repository root, create a virtual environment:

```bash
python3 -m venv .venv
```

```bash
source .venv/bin/activate
```

```bash
pip install -r models/requirements.txt
```

Check everything works (about a minute):

```bash
cd models && python -m pytest -q
```

## 1. Smoke test on synthetic data

The synthetic generator makes toy drums, bass, vocals and pads. A model trained on it will not
separate real music, but it proves the whole loop (train → export → plugin) on your machine:

```bash
python -m stems_train.train --data synthetic --out runs/synth --steps 2000 --batch-size 8
```

`--device auto` (the default) uses the Apple GPU through MPS when available. Each `eval` line
prints per-stem SDR between two references: the **equal-split floor** (every stem gets a quarter
of the mix — what an untrained model does) and the **oracle ceiling** (ideal ratio masks computed
from the true stems — the best any mask-based model can do). Progress is how far the mean moves
from the floor toward the ceiling; a few hundred CPU steps barely move it, so train for tens of
thousands of steps on a GPU.

## 2. Get training data

The model needs mixtures with their separate stems. Two ways to get them:

**Licensed multitracks.** Put each song in `data/<name>/{train,valid}/<song>/{drums,bass,vocals,other}.wav`
at 44.1 kHz. Stems should sum to the mix. Pre-mix extra stems (guitars, keys, ...) into `other`.

**Distillation from a large offline model.** A big offline separator (the "teacher") separates
music you own, and the real-time model learns to imitate it:

```bash
pip install demucs
```

```bash
python tools/make_teacher_stems.py ~/Music/library data/teacher --excerpt-seconds 90
```

This uses [HT-Demucs FT](https://github.com/facebookresearch/demucs) (the model packaged on
Hugging Face as `StemSplitio/htdemucs-ft-pytorch`; the `demucs` package downloads Meta's original
weights). `--device auto` uses the Apple GPU when available (`--device cpu` if MPS gives errors).
It skips songs it already did, and stores FLAC (about 100 MB per full song, less with
`--excerpt-seconds`). Diversity matters more than song length: prefer 1–2k songs × 90 s over
200 full songs.

> **Licensing.** The Demucs code is MIT-licensed, but the pretrained weights were trained on
> MUSDB18-HQ (non-commercial research licence) plus Meta's internal data. Whether a model
> distilled from it can be sold is a legal question — get advice before shipping. You also need
> the rights to the music you separate. Licensed multitracks avoid both issues.

## 3. Train

```bash
python -m stems_train.train --data data/teacher --out runs/v1 --steps 200000 --batch-size 16 --segment-seconds 6
```

Resume an interrupted run:

```bash
python -m stems_train.train --data data/teacher --out runs/v1 --steps 200000 --resume runs/v1/last.pt
```

`runs/v1/log.jsonl` has the loss curve and eval SDRs; `best.pt` is the best checkpoint so far.

Useful knobs (all CLI flags):

| flag | default | effect |
|---|---|---|
| `--dim` | 64 | model width; quality vs CPU cost |
| `--lookahead-frames` | 3 | future context; +23 ms latency per frame |
| `--dilations` | `4,2,2,1` | temporal receptive field: 19 frames ≈ 440 ms |
| `--fft-size` | 4096 | frequency resolution vs latency |
| `--remix-prob` | 0.5 | mix stems from different songs (strong augmentation) |
| `--sdr-weight` | 0.05 | weight of the negative-SDR loss term (see below) |

## 4. Export and try it in the plugin

```bash
python -m stems_train.export runs/v1/best.pt stems-v1.onnx --name "stems v1"
```

Measure its CPU cost inside the real engine (needs Rust; from the repository root):

```bash
cargo run --release -p stems-core --features onnx --example bench -- models/stems-v1.onnx
```

Keep `last` inference time well under the per-hop budget it prints (23 ms at 44.1 kHz).
Then run the plugin with it:

```bash
export STEMS_MODEL=$PWD/models/stems-v1.onnx
```

## Design notes

- **Training equals deployment.** Every temporal layer is an unpadded dilated convolution, so
  running the network over a long sequence gives exactly the masks the plugin computes window by
  window (`tests/test_model.py::test_sequence_equals_sliding_window`). Normalisation is per
  frame only for the same reason.
- **Lookahead is structural.** The model sees `context_frames − 1 − lookahead` past frames and
  `lookahead` future frames; residual connections carry the target frame through.
- **Loss.** Compressed-spectrum L1 (quiet detail) + 0.05 × negative SDR (loud, audible
  content). The compressed term alone correlated poorly with separation quality: on a
  single-batch overfit test it plateaued at +5 dB while the SDR term reached +13.5 dB in the same
  150 steps. The SDR epsilon scales with the mixture energy so silent stems stay bounded.
- **No complex tensors on device.** STFTs run on CPU in the data-loader workers; the model and
  loss work on real/imaginary planes, which keeps Apple MPS happy.
- **Speed.** Bands of equal width are batched into single matmuls, which cut the exported graph
  from ~1800 to ~550 nodes and inference time by ~2.4×. Default size: 1.8 M parameters, ~6.5 ms
  per hop on a 4-core cloud CPU (budget 23 ms).
