import torch

from .model import compress_planes


def apply_masks(masks: torch.Tensor, mix_planes: torch.Tensor) -> torch.Tensor:
    """masks [B, S, F, n], mix_planes [B, 4, F, n] -> stem planes [B, S, 4, F, n]."""
    return masks[:, :, None] * mix_planes[:, None]


def separation_loss(est: torch.Tensor, target: torch.Tensor, power: float = 0.3) -> torch.Tensor:
    """L1 between magnitude-compressed complex spectra (re/im) plus compressed magnitudes.
    est/target: [B, S, 4, F, n]."""
    ce, ct = compress_planes(est, power), compress_planes(target, power)
    loss = (ce - ct).abs().mean()
    me = ce.reshape(*ce.shape[:2], 2, 2, *ce.shape[-2:]).pow(2).sum(3).add(1e-8).sqrt()
    mt = ct.reshape(*ct.shape[:2], 2, 2, *ct.shape[-2:]).pow(2).sum(3).add(1e-8).sqrt()
    return loss + (me - mt).abs().mean()


@torch.no_grad()
def spectral_sdr(est: torch.Tensor, target: torch.Tensor) -> torch.Tensor:
    """Per-stem SDR in dB computed on spectra (≈ waveform SDR by Parseval). -> [S]."""
    num = target.pow(2).sum(dim=(0, 2, 3, 4))
    den = (target - est).pow(2).sum(dim=(0, 2, 3, 4))
    return 10 * torch.log10((num + 1e-8) / (den + 1e-8))
