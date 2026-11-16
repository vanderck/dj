"""Export a checkpoint to an ONNX file that follows the Stems model contract v1.

    python -m stems_train.export runs/v1/best.pt stems-v1.onnx --name "stems v1"

The exported model is checked against PyTorch with ONNX Runtime before it is written.
"""
import argparse
import tempfile
from pathlib import Path

import numpy as np
import onnx
import onnxruntime as ort
import torch

from .config import ModelConfig
from .model import ExportWrapper, StemsNet

OPSET = 18


def _torch_export(wrapper, example, path: str) -> str:
    """New torch.export-based exporter (smaller graphs); falls back to the legacy exporter."""
    common = dict(input_names=["spec"], output_names=["masks"], opset_version=OPSET)
    try:
        torch.onnx.export(wrapper, (example,), path, dynamo=True, external_data=False, **common)
        return "dynamo"
    except Exception as e:  # missing onnxscript, unsupported op, ...
        print(f"dynamo export failed ({type(e).__name__}: {e}); using the legacy exporter")
        torch.onnx.export(wrapper, (example,), path, dynamo=False, **common)
        return "legacy"


def parity_inputs(example: torch.Tensor) -> list:
    """Inputs for the ONNX/PyTorch check. Real music has what loud noise lacks: silent bins
    (digital silence, the empty top octave of an MP3) and very quiet ones."""
    g = torch.Generator().manual_seed(0)
    quiet = torch.randn(example.shape, generator=g) * 1e-3
    lowpassed = example.clone()
    lowpassed[..., example.shape[-1] * 3 // 4 :] = 0.0
    tilt = example * torch.logspace(1, -5, example.shape[-1])  # loud lows, near-silent highs
    return [example, quiet, lowpassed, tilt, torch.zeros_like(example)]


def load_checkpoint(path: str) -> StemsNet:
    ckpt = torch.load(path, map_location="cpu")
    net = StemsNet(ModelConfig.from_dict(ckpt["model_config"]))
    net.load_state_dict(ckpt["state_dict"])
    return net.eval()


def export(net: StemsNet, out: str, name: str, check: bool = True) -> dict:
    cfg = net.cfg
    wrapper = ExportWrapper(net.eval().cpu())
    example = torch.randn(1, cfg.context_frames, 4, cfg.num_bins) * 10.0

    with tempfile.TemporaryDirectory() as tmp:
        raw = Path(tmp) / "raw.onnx"
        exporter = _torch_export(wrapper, example, str(raw))
        model = onnx.load(str(raw))

    model.producer_name = "stems_train"
    props = {
        "stems.contract": "1",
        "stems.name": name,
        "stems.sample_rate": str(cfg.sample_rate),
        "stems.fft_size": str(cfg.fft_size),
        "stems.context_frames": str(cfg.context_frames),
        "stems.lookahead_frames": str(cfg.lookahead_frames),
    }
    del model.metadata_props[:]
    onnx.helper.set_model_props(model, props)
    onnx.checker.check_model(model)

    if check:
        sess = ort.InferenceSession(model.SerializeToString(), providers=["CPUExecutionProvider"])
        err = 0.0
        for x in parity_inputs(example):
            (got,) = sess.run(None, {sess.get_inputs()[0].name: x.numpy()})
            with torch.no_grad():
                want = wrapper(x).numpy()
            if got.shape != (1, 4, cfg.num_bins) or not np.isfinite(got).all():
                raise RuntimeError(f"ONNX model gives shape {got.shape} / non-finite masks")
            err = max(err, float(np.abs(got - want).max()))
        if err > 1e-3:
            raise RuntimeError(f"ONNX/PyTorch mismatch: max abs err {err:.2e}")
        props["parity_max_abs_err"] = f"{err:.2e}"

    onnx.save(model, out)
    props["exporter"] = exporter
    return props


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("checkpoint")
    p.add_argument("out")
    p.add_argument("--name", default=None)
    a = p.parse_args(argv)
    net = load_checkpoint(a.checkpoint)
    props = export(net, a.out, a.name or Path(a.checkpoint).parent.name)
    size = Path(a.out).stat().st_size / 1e6
    lat = net.cfg.latency_samples()
    print(f"wrote {a.out} ({size:.1f} MB)")
    for k, v in props.items():
        print(f"  {k} = {v}")
    print(f"  plugin latency ≈ {lat} samples ({1000 * lat / net.cfg.sample_rate:.0f} ms) with 2 pipeline frames")


if __name__ == "__main__":
    main()
