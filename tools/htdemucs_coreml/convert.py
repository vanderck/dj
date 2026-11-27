"""HT-Demucs's network as a Core ML model, for the Neural Engine.

The model is the part of HT-Demucs between its inputs and outputs once normalised: dj computes
the spectrogram (STFT), the normalisation, the mask and the inverse STFT itself (stems-core,
`demucs::spec`), so what Core ML runs is only the network (convolutions, the transformer), in
16 bits, as the Neural Engine computes.

    inputs:  spec  [1, 4, 2048, 336]  the mix's spectrogram, complex as channels, normalised
             wave  [1, 2, 343980]     the mix, normalised
    outputs: spec_out [1, 16, 2048, 336]  the four sources' spectrograms, still normalised
             wave_out [1, 8, 343980]      the four sources' waves, still normalised

Usage:
    python convert.py WEIGHTS.th OUT.mlpackage [--onnx OUT.onnx]

WEIGHTS: https://dl.fbaipublicfiles.com/demucs/hybrid_transformer/955717e8-8726e21a.th
(htdemucs). `--onnx` writes the same network as ONNX (for dj's tests on systems without Core ML).
Needs torch, coremltools, demucs (no dependencies needed beyond einops, julius, openunmix).
"""

import argparse
import fractions

import numpy as np
import torch
from einops import rearrange
from demucs.htdemucs import HTDemucs

SEGMENT = 343980
FRAMES = 336
BINS = 2048


def load(path):
    """The model, its weights loaded without running code from the file (weights only, with
    the few plain types the file holds allowed)."""
    from numpy._core.multiarray import scalar

    torch.serialization.add_safe_globals(
        [
            HTDemucs,
            fractions.Fraction,
            (scalar, "numpy.core.multiarray.scalar"),
            np.dtype,
            type(np.dtype("float64")),
            type(np.dtype("int64")),
        ]
    )
    pkg = torch.load(path, map_location="cpu", weights_only=True)
    assert pkg["klass"] is HTDemucs, pkg["klass"]
    model = HTDemucs(*pkg["args"], **pkg["kwargs"])
    model.load_state_dict(pkg["state"])
    return model.eval()


class Network(torch.nn.Module):
    """HTDemucs.forward from the normalised inputs to the outputs before un-normalising
    (demucs 4.1, htdemucs.py), unchanged in between."""

    def __init__(self, m):
        super().__init__()
        self.m = m

    def forward(self, x, xt):
        m = self.m
        saved, saved_t, lengths, lengths_t = [], [], [], []
        for idx, encode in enumerate(m.encoder):
            lengths.append(x.shape[-1])
            inject = None
            if idx < len(m.tencoder):
                lengths_t.append(xt.shape[-1])
                tenc = m.tencoder[idx]
                xt = tenc(xt)
                if not tenc.empty:
                    saved_t.append(xt)
                else:
                    inject = xt
            x = encode(x, inject)
            if idx == 0 and m.freq_emb is not None:
                frs = torch.arange(x.shape[-2], device=x.device)
                emb = m.freq_emb(frs).t()[None, :, :, None].expand_as(x)
                x = x + m.freq_emb_scale * emb
            saved.append(x)
        if m.crosstransformer:
            if m.bottom_channels:
                b, c, f, t = x.shape
                x = rearrange(x, "b c f t-> b c (f t)")
                x = m.channel_upsampler(x)
                x = rearrange(x, "b c (f t)-> b c f t", f=f)
                xt = m.channel_upsampler_t(xt)
            x, xt = m.crosstransformer(x, xt)
            if m.bottom_channels:
                x = rearrange(x, "b c f t-> b c (f t)")
                x = m.channel_downsampler(x)
                x = rearrange(x, "b c (f t)-> b c f t", f=f)
                xt = m.channel_downsampler_t(xt)
        for idx, decode in enumerate(m.decoder):
            skip = saved.pop(-1)
            x, pre = decode(x, skip, lengths.pop(-1))
            offset = m.depth - len(m.tdecoder)
            if idx >= offset:
                tdec = m.tdecoder[idx - offset]
                length_t = lengths_t.pop(-1)
                if tdec.empty:
                    pre = pre[:, :, 0]
                    xt, _ = tdec(pre, None, length_t)
                else:
                    skip = saved_t.pop(-1)
                    xt, _ = tdec(xt, skip, length_t)
        return x, xt


def precision(fp32_ops):
    """16 bits, but for the op types in `fp32_ops` (comma-separated)."""
    import coremltools as ct

    keep = {o for o in fp32_ops.split(",") if o}
    if not keep:
        return ct.precision.FLOAT16
    return ct.transform.FP16ComputePrecision(op_selector=lambda op: op.op_type not in keep)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("weights")
    ap.add_argument("out")
    ap.add_argument("--onnx")
    ap.add_argument("--fp32", action="store_true", help="32-bit (the CPU and GPU only)")
    ap.add_argument(
        "--fp32-ops",
        default="",
        help="comma-separated MIL op types kept 32-bit in a 16-bit model",
    )
    a = ap.parse_args()

    torch.backends.mha.set_fastpath_enabled(False)  # plain attention ops, not the fused kernel
    net = Network(load(a.weights)).eval()
    spec = torch.randn(1, 4, BINS, FRAMES)
    wave = torch.randn(1, 2, SEGMENT)
    with torch.no_grad():
        # (No trace check: torch 2.7 finds the graph differs on a second call, where torch 2.15
        # doesn't; dj's CI checks the converted model's stems against Mixxx's model instead.)
        traced = torch.jit.trace(net, (spec, wave), check_trace=False)
        if a.onnx:
            torch.onnx.export(
                net,
                (spec, wave),
                a.onnx,
                input_names=["spec", "wave"],
                output_names=["spec_out", "wave_out"],
                opset_version=17,
                dynamo=False,
            )
            print("wrote", a.onnx)

    import coremltools as ct

    mlmodel = ct.convert(
        traced,
        inputs=[
            ct.TensorType(name="spec", shape=spec.shape, dtype=np.float32),
            ct.TensorType(name="wave", shape=wave.shape, dtype=np.float32),
        ],
        outputs=[
            ct.TensorType(name="spec_out", dtype=np.float32),
            ct.TensorType(name="wave_out", dtype=np.float32),
        ],
        convert_to="mlprogram",
        compute_precision=ct.precision.FLOAT32 if a.fp32 else precision(a.fp32_ops),
        minimum_deployment_target=ct.target.macOS13,
    )
    mlmodel.short_description = "HT-Demucs (htdemucs) network, normalised spectrogram and wave in and out"
    mlmodel.save(a.out)
    print("wrote", a.out)


if __name__ == "__main__":
    main()
