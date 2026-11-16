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


def test_snapshot_dataset_takes_one_fixed_clip_per_track(tmp_path):
    sr = 8000
    for t, seconds in enumerate((10.0, 3.0)):  # the second track is shorter than the offset
        d = tmp_path / "valid" / f"track{t}"
        d.mkdir(parents=True)
        ramp = np.arange(int(sr * seconds)) / sr  # sample value = its time in seconds
        for stem in ("drums", "bass", "vocals", "other"):
            sf.write(d / f"{stem}.wav", np.stack([ramp, ramp], 1) * 0.01, sr, subtype="FLOAT")
    ds = StemFolder(str(tmp_path), "valid", sr, 1.0, length=99, snapshot_seconds=6.0)
    assert len(ds) == 2
    first, second = ds[0], ds[1]
    assert np.isclose(first[0, 0, 0].item(), 0.06)  # starts 6 s in
    assert np.isclose(second[0, 0, 0].item(), 0.02)  # last second of a 3 s track
    assert torch.equal(ds[0], first)


def test_evaluate_matches_whole_set_sdr():
    from stems_train.config import ModelConfig
    from stems_train.losses import apply_masks, spectral_sdr
    from stems_train.model import StemsNet
    from stems_train.train import Collate, evaluate, oracle_masks, step_loss

    torch.manual_seed(0)
    net = StemsNet(ModelConfig(fft_size=512, dim=16)).eval()
    items = [torch.randn(4, 2, 4096) * torch.rand(4, 1, 1) for _ in range(6)]
    collate = Collate(512)
    batches = [collate(items[i : i + 2]) for i in range(0, 6, 2)]
    sdr, base, oracle = evaluate(net, batches, "cpu")

    mix, tgt = collate(items)
    with torch.no_grad():
        _, est, t, m = step_loss(net, mix, tgt)
    assert torch.allclose(sdr, spectral_sdr(est, t), atol=1e-3)
    assert torch.allclose(base, spectral_sdr(0.25 * m[:, None].expand_as(t), t), atol=1e-3)
    assert torch.allclose(oracle, spectral_sdr(apply_masks(oracle_masks(t), m), t), atol=1e-3)


def test_export_survives_silent_and_quiet_bins(tmp_path):
    """Regression: the ONNX optimiser dropped the `+ 1e-8` in the input compression, so silent
    bins gave NaN and quiet ones wrong masks in the exported model (but not in PyTorch)."""
    import onnxruntime as ort
    from stems_train.export import parity_inputs
    from stems_train.model import ExportWrapper

    torch.manual_seed(0)
    net = StemsNet(ModelConfig(fft_size=512, dim=16)).eval()
    export(net, str(tmp_path / "m.onnx"), "quiet")  # its parity check covers these inputs too
    sess = ort.InferenceSession(str(tmp_path / "m.onnx"))
    example = torch.randn(1, net.cfg.context_frames, 4, net.cfg.num_bins) * 10
    for x in parity_inputs(example):
        (got,) = sess.run(None, {"spec": x.numpy()})
        with torch.no_grad():
            want = ExportWrapper(net)(x).numpy()
        assert np.isfinite(got).all()
        assert np.abs(got - want).max() < 1e-3
