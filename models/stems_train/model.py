"""Band-split temporal-convolution mask estimator.

Key property: every temporal layer is an unpadded ("valid") dilated convolution, so the network
maps a window of `context_frames` frames to exactly one output frame. Run on a long sequence it
produces the same numbers the plugin gets from sliding that window one hop at a time — training
and deployment are the same computation (see tests/test_model.py).

Shapes: planes [B, 4, F, N] -> masks [B, S, F, N - context_frames + 1], where output frame j holds
the masks for input frame j + context_frames - 1 - lookahead_frames.

Bands come in groups of equal width (2, 4, 8, ... bins), so each group's projection is a single
batched matmul. This keeps the exported ONNX graph small: per-op overhead, not arithmetic,
dominates a model this size on CPU.
"""
import torch
from torch import nn

from .config import STEMS, ModelConfig


def band_groups(num_bins: int, base_width: int, per_group: int) -> list:
    """-> [(start_bin, width, count)], widths doubling per group, covering all bins exactly."""
    groups, start, width = [], 0, base_width
    while start < num_bins:
        count = min(per_group, (num_bins - start) // width)
        if count == 0:
            groups.append((start, num_bins - start, 1))
            break
        groups.append((start, width, count))
        start += width * count
        width *= 2
    return groups


def compress_planes(planes: torch.Tensor, power: float, eps: float = 1e-8) -> torch.Tensor:
    """Compress magnitude to |X|^power, keeping phase. planes [..., 4, F, N] (re/im pairs)."""
    shape = planes.shape
    p = planes.reshape(*shape[:-3], 2, 2, *shape[-2:])
    mag = torch.sqrt(p[..., 0:1, :, :].pow(2) + p[..., 1:2, :, :].pow(2) + eps)
    return (p * mag.pow(power - 1.0)).reshape(shape)


class ChannelNorm(nn.Module):
    """LayerNorm over dim 1 of [B, D, K, N] (per band and frame; never across time)."""

    def __init__(self, dim: int, eps: float = 1e-5):
        super().__init__()
        self.weight = nn.Parameter(torch.ones(1, dim, 1, 1))
        self.bias = nn.Parameter(torch.zeros(1, dim, 1, 1))
        self.eps = eps

    def forward(self, x):
        mean = x.mean(1, keepdim=True)
        var = (x - mean).pow(2).mean(1, keepdim=True)
        return (x - mean) * torch.rsqrt(var + self.eps) * self.weight + self.bias


class TemporalBlock(nn.Module):
    """Valid dilated conv over time (shared by all bands), residual cropped to the output."""

    def __init__(self, dim: int, kernel: int, dilation: int, residual_offset: int):
        super().__init__()
        self.norm = ChannelNorm(dim)
        self.conv = nn.Conv2d(dim, 2 * dim, (1, kernel), dilation=(1, dilation))
        self.proj = nn.Conv2d(2 * dim, dim, 1)
        self.offset = residual_offset

    def forward(self, x):  # [B, D, K, N]
        y = self.proj(nn.functional.gelu(self.conv(self.norm(x))))
        return x[..., self.offset : self.offset + y.shape[-1]] + y


class BandMixer(nn.Module):
    """MLP across bands, per feature and frame."""

    def __init__(self, bands: int, dim: int):
        super().__init__()
        self.norm = ChannelNorm(dim)
        self.fc1 = nn.Linear(bands, 2 * bands)
        self.fc2 = nn.Linear(2 * bands, bands)

    def forward(self, x):  # [B, D, K, N]
        y = self.norm(x).transpose(2, 3)  # [B, D, N, K]
        y = self.fc2(nn.functional.gelu(self.fc1(y))).transpose(2, 3)
        return x + y


class GroupLinear(nn.Module):
    """Independent linear layer per band within a group: [B, N, c, i] -> [B, N, c, o]."""

    def __init__(self, count: int, d_in: int, d_out: int):
        super().__init__()
        self.weight = nn.Parameter(torch.randn(count, d_in, d_out) * d_in**-0.5)
        self.bias = nn.Parameter(torch.zeros(count, d_out))

    def forward(self, x):
        # One batched matmul per band: [c, B*N, i] @ [c, i, o]. (Broadcasting x[..., None, :] against
        # the weight instead materialised a copy of the weight for every frame: gigabytes at
        # training batch sizes, and out-of-memory on Apple GPUs.)
        b, n, c, i = x.shape
        y = torch.bmm(x.reshape(b * n, c, i).transpose(0, 1), self.weight)  # [c, B*N, o]
        return y.transpose(0, 1).reshape(b, n, c, -1) + self.bias


class StemsNet(nn.Module):
    def __init__(self, cfg: ModelConfig):
        super().__init__()
        cfg.validate()
        self.cfg = cfg
        self.groups = band_groups(cfg.num_bins, cfg.band_width, cfg.bands_per_group)
        self.num_bands = sum(c for _, _, c in self.groups)
        S, D = len(STEMS), cfg.dim

        self.band_in = nn.ModuleList(GroupLinear(c, 4 * w, D) for _, w, c in self.groups)
        self.in_norm = nn.LayerNorm(D)

        # Residual crops route the target frame (index T-1-L of the window) straight through.
        remaining = cfg.context_frames - 1 - cfg.lookahead_frames
        temporal, mixers = [], []
        for d in cfg.dilations:
            off = min((cfg.kernel - 1) * d, remaining)
            remaining -= off
            temporal.append(TemporalBlock(D, cfg.kernel, d, off))
            mixers.append(BandMixer(self.num_bands, D))
        assert remaining == 0
        self.temporal = nn.ModuleList(temporal)
        self.mixers = nn.ModuleList(mixers)

        self.out_norm = nn.LayerNorm(D)
        self.out_hidden = nn.Linear(D, 2 * D)
        self.band_out = nn.ModuleList(GroupLinear(c, 2 * D, S * w) for _, w, c in self.groups)

    def forward(self, planes: torch.Tensor) -> torch.Tensor:
        """planes [B, 4, F, N] -> masks [B, S, F, N - T + 1] (softmax over stems)."""
        cfg = self.cfg
        B, _, _, N = planes.shape
        x = compress_planes(planes, cfg.compress)

        feats = []
        for (start, w, c), proj in zip(self.groups, self.band_in):
            g = x[:, :, start : start + w * c, :].reshape(B, 4, c, w, N)
            g = g.permute(0, 4, 2, 1, 3).reshape(B, N, c, 4 * w)
            feats.append(proj(g))  # [B, N, c, D]
        h = self.in_norm(torch.cat(feats, dim=2)).permute(0, 3, 2, 1)  # [B, D, K, N]

        for block, mixer in zip(self.temporal, self.mixers):
            h = mixer(block(h))

        n = h.shape[-1]
        h = nn.functional.gelu(self.out_hidden(self.out_norm(h.permute(0, 3, 2, 1))))  # [B, n, K, 2D]
        logits, k = [], 0
        for (_, w, c), head in zip(self.groups, self.band_out):
            o = head(h[:, :, k : k + c])  # [B, n, c, S*w]
            k += c
            logits.append(o.reshape(B, n, c, len(STEMS), w).permute(0, 3, 2, 4, 1).reshape(B, len(STEMS), c * w, n))
        return torch.softmax(torch.cat(logits, dim=2), dim=1)  # [B, S, F, n]

    def target_frames(self, n_frames: int) -> slice:
        """Which input frames the output masks belong to."""
        t0 = self.cfg.context_frames - 1 - self.cfg.lookahead_frames
        return slice(t0, t0 + n_frames - self.cfg.context_frames + 1)


class ExportWrapper(nn.Module):
    """Contract v1 interface: spec [1, T, 4, F] -> masks [1, 4, F]."""

    def __init__(self, net: StemsNet):
        super().__init__()
        self.net = net

    def forward(self, spec: torch.Tensor) -> torch.Tensor:
        planes = spec.permute(0, 2, 3, 1)  # [1, 4, F, T]
        return self.net(planes)[..., 0]  # [1, S, F]


def count_parameters(model: nn.Module) -> int:
    return sum(p.numel() for p in model.parameters())
