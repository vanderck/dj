import torch

from .model import compress_planes


def apply_masks(masks: torch.Tensor, mix_planes: torch.Tensor) -> torch.Tensor:
    """masks [B, S, F, n], mix_planes [B, 4, F, n] -> stem planes [B, S, 4, F, n]."""
    return masks[:, :, None] * mix_planes[:, None]


def compressed_loss(est: torch.Tensor, target: torch.Tensor, power: float = 0.3) -> torch.Tensor:
    """L1 between magnitude-compressed complex spectra (re/im) plus compressed magnitudes.
    Emphasises quiet detail. est/target: [B, S, 4, F, n]."""
    ce, ct = compress_planes(est, power), compress_planes(target, power)
    loss = (ce - ct).abs().mean()
    me = ce.reshape(*ce.shape[:2], 2, 2, *ce.shape[-2:]).pow(2).sum(3).add(1e-8).sqrt()
    mt = ct.reshape(*ct.shape[:2], 2, 2, *ct.shape[-2:]).pow(2).sum(3).add(1e-8).sqrt()
    return loss + (me - mt).abs().mean()


def sdr_loss(est: torch.Tensor, target: torch.Tensor, mix: torch.Tensor, floor: float = 1e-3) -> torch.Tensor:
    """Negative SDR (dB) per item and stem, averaged. Emphasises the loud, audible content.

    The epsilon is `floor` times the item's mixture energy, so a silent target stem (e.g. a verse
    without vocals) yields a bounded loss that asks for "quiet enough" instead of exploding.
    est/target: [B, S, 4, F, n], mix: [B, 4, F, n]."""
    eps = floor * mix.pow(2).sum(dim=(1, 2, 3))[:, None] + 1e-8  # [B, 1]
    num = target.pow(2).sum(dim=(2, 3, 4)) + eps
    den = (target - est).pow(2).sum(dim=(2, 3, 4)) + eps
    return -(10 * torch.log10(num / den)).mean()


def separation_loss(est: torch.Tensor, target: torch.Tensor, mix: torch.Tensor, sdr_weight: float = 0.05) -> torch.Tensor:
    """Training loss: compressed-spectrum L1 + sdr_weight * negative SDR.

    The compressed term alone optimises quiet bins and correlates poorly with SDR (on a
    single-batch overfit test it plateaued at +5 dB while the oracle was +12.5 dB); adding the SDR
    term reached +13.5 dB in the same steps."""
    loss = compressed_loss(est, target)
    if sdr_weight > 0:
        loss = loss + sdr_weight * sdr_loss(est, target, mix)
    return loss


@torch.no_grad()
def spectral_sdr(est: torch.Tensor, target: torch.Tensor) -> torch.Tensor:
    """Per-stem SDR in dB computed on spectra (≈ waveform SDR by Parseval). -> [S]."""
    num = target.pow(2).sum(dim=(0, 2, 3, 4))
    den = (target - est).pow(2).sum(dim=(0, 2, 3, 4))
    return 10 * torch.log10((num + 1e-8) / (den + 1e-8))
