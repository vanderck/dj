import json

import numpy as np
import onnx
import soundfile as sf
import torch

from stems_train.config import ModelConfig, TrainConfig
from stems_train.data import StemFolder
from stems_train.export import export, load_checkpoint
from stems_train.model import StemsNet
from stems_train.train import train

TINY = ModelConfig(fft_size=1024, bands_per_group=4, dim=24, dilations=(2, 1), lookahead_frames=1)


def test_export_roundtrip_and_metadata(tmp_path):
    torch.manual_seed(0)
    net = StemsNet(TINY)
    out = tmp_path / "m.onnx"
    props = export(net, str(out), "tiny")
    model = onnx.load(str(out))
    meta = {p.key: p.value for p in model.metadata_props}
    assert meta["stems.contract"] == "1"
    assert meta["stems.fft_size"] == "1024"
    assert meta["stems.context_frames"] == str(TINY.context_frames)
    assert meta["stems.lookahead_frames"] == "1"
    assert float(props["parity_max_abs_err"]) < 1e-4
    assert props["exporter"] == "dynamo"


def test_short_training_run_learns_and_exports(tmp_path):
    cfg = TrainConfig(data="synthetic", out=str(tmp_path / "run"), steps=60, batch_size=4,
                      segment_seconds=1.5, lr=3e-3, warmup_steps=5, num_workers=0, device="cpu",
                      log_every=20, eval_every=60, ckpt_every=60, model=TINY)
    metrics = train(cfg)
    # Beats an untrained model (equal split) and stays below the oracle ceiling.
    assert metrics["baseline_mean_sdr"] < metrics["mean_sdr"] < metrics["oracle_mean_sdr"], metrics
    logged = [json.loads(line) for line in (tmp_path / "run" / "log.jsonl").read_text().splitlines()]
    losses = [r["loss"] for r in logged if "loss" in r]
    assert losses[-1] < losses[0]
    net = load_checkpoint(str(tmp_path / "run" / "last.pt"))
    export(net, str(tmp_path / "trained.onnx"), "trained tiny")


def test_stem_folder_dataset(tmp_path):
    sr = 44100
    for split in ("train", "valid"):
        for t in range(2):
            d = tmp_path / split / f"track{t}"
            d.mkdir(parents=True)
            for i, stem in enumerate(("drums", "bass", "vocals", "other")):
                x = 0.1 * np.sin(2 * np.pi * (100 + 100 * i) * np.arange(sr * 2) / sr)
                sf.write(d / f"{stem}.wav", np.stack([x, x], 1), sr)
    ds = StemFolder(str(tmp_path), "train", sr, 1.0, length=5)
    item = ds[0]
    assert item.shape == (4, 2, sr)
    assert np.abs(item.numpy()).max() <= 1.0
    sf.write(tmp_path / "train" / "track0" / "bass.wav", np.zeros((100, 2)), 22050)
    try:
        StemFolder(str(tmp_path), "train", sr, 1.0, length=5)
        raise AssertionError("expected a sample-rate error")
    except ValueError as e:
        assert "22050" in str(e)
