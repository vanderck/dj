"""STFT exactly matching the plugin engine (docs/MODEL_CONTRACT.md).

Everything here is real-valued at the interface ("planes" = re/im split) so models and losses can
run on devices without full complex support (Apple MPS).
"""
import torch


def sqrt_hann(fft_size: int, device=None) -> torch.Tensor:
    n = torch.arange(fft_size, dtype=torch.float64, device=device)
    return torch.sqrt(0.5 - 0.5 * torch.cos(2 * torch.pi * n / fft_size)).float()


def stft_planes(audio: torch.Tensor, fft_size: int) -> torch.Tensor:
    """audio [..., 2, samples] -> planes [..., 4, F, frames] = (L.re, L.im, R.re, R.im).

    Frame t covers samples [(t+1)*hop - fft_size, (t+1)*hop) with zeros before the start, i.e.
    exactly the frame the engine gives the model at its t-th hop.
    """
    hop = fft_size // 4
    lead = audio.shape[:-2]
    x = audio.reshape(-1, audio.shape[-1])
    x = torch.nn.functional.pad(x, (fft_size - hop, 0))
    spec = torch.stft(
        x,
        n_fft=fft_size,
        hop_length=hop,
        window=sqrt_hann(fft_size, device=x.device),
        center=False,
        onesided=True,
        return_complex=True,
    )  # [B*2, F, frames]
    spec = spec.reshape(*lead, 2, spec.shape[-2], spec.shape[-1])
    planes = torch.stack([spec.real, spec.imag], dim=-3)  # [..., 2ch, 2, F, frames]
    return planes.reshape(*lead, 4, spec.shape[-2], spec.shape[-1])
