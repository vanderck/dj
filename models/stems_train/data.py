"""Datasets yielding (stems [4, 2, samples]) segments; the mixture is their sum.

Folder layout (MUSDB18-HQ style), all files at the model's sample rate:

    root/
      train/<track>/{drums,bass,vocals,other}.{wav,flac}
      valid/<track>/{drums,bass,vocals,other}.{wav,flac}

Stems may come from licensed multitracks or from a teacher model's separations (distillation,
see tools/make_teacher_stems.py).
"""
from pathlib import Path

import numpy as np
import soundfile as sf
import torch
from torch.utils.data import Dataset

from .config import STEMS
from .synth import make_stems


class SyntheticStems(Dataset):
    def __init__(self, sample_rate: int, segment_seconds: float, length: int, seed: int = 0):
        self.sr, self.seconds, self.length, self.seed = sample_rate, segment_seconds, length, seed

    def __len__(self):
        return self.length

    def __getitem__(self, i):
        rng = np.random.default_rng((self.seed, i))
        return torch.from_numpy(make_stems(self.sr, self.seconds, rng))


class StemFolder(Dataset):
    def __init__(
        self,
        root: str,
        split: str,
        sample_rate: int,
        segment_seconds: float,
        length: int,
        augment: bool = True,
        remix_prob: float = 0.5,
        seed: int = 0,
    ):
        self.tracks = sorted(p for p in (Path(root) / split).iterdir() if p.is_dir())
        if not self.tracks:
            raise FileNotFoundError(f"no track folders in {Path(root) / split}")
        self.sr, self.length, self.augment, self.remix_prob = sample_rate, length, augment, remix_prob
        self.segment = int(segment_seconds * sample_rate)
        self.seed = seed
        self.frames = []
        for track in self.tracks:
            lengths = []
            for stem in STEMS:
                path = self._path(track, stem)
                info = sf.info(str(path))
                if info.samplerate != sample_rate:
                    raise ValueError(f"{path} is {info.samplerate} Hz, expected {sample_rate}")
                lengths.append(info.frames)
            self.frames.append(min(lengths))

    def __len__(self):
        return self.length

    @staticmethod
    def _path(track: Path, stem: str) -> Path:
        for ext in ("wav", "flac"):
            p = track / f"{stem}.{ext}"
            if p.exists():
                return p
        raise FileNotFoundError(f"{track}/{stem}.wav or .flac not found")

    def _read(self, track_idx: int, stem: str, rng) -> np.ndarray:
        total = self.frames[track_idx]
        start = int(rng.integers(0, max(1, total - self.segment)))
        x, _ = sf.read(
            str(self._path(self.tracks[track_idx], stem)),
            start=start,
            frames=self.segment,
            dtype="float32",
            always_2d=True,
        )
        x = x.T[:2]
        if x.shape[0] == 1:
            x = np.repeat(x, 2, axis=0)
        if x.shape[1] < self.segment:
            x = np.pad(x, ((0, 0), (0, self.segment - x.shape[1])))
        return x

    def __getitem__(self, i):
        rng = np.random.default_rng((self.seed, i)) if not self.augment else np.random.default_rng()
        base = int(rng.integers(len(self.tracks)))
        remix = self.augment and rng.random() < self.remix_prob
        stems = []
        for stem in STEMS:
            track = int(rng.integers(len(self.tracks))) if remix else base
            x = self._read(track, stem, rng)
            if self.augment:
                x = x * 10 ** (rng.uniform(-6, 3) / 20)
                if rng.random() < 0.5:
                    x = x[::-1]
                if rng.random() < 0.5:
                    x = -x
            stems.append(x)
        stems = np.stack(stems).astype(np.float32)
        peak = np.abs(stems.sum(0)).max()
        if peak > 1.0:
            stems /= peak
        return torch.from_numpy(np.ascontiguousarray(stems))


def make_dataset(data: str, split: str, sample_rate: int, segment_seconds: float, length: int,
                 remix_prob: float = 0.5, seed: int = 0) -> Dataset:
    if data == "synthetic":
        return SyntheticStems(sample_rate, segment_seconds, length, seed=seed + (0 if split == "train" else 10**6))
    return StemFolder(data, split, sample_rate, segment_seconds, length,
                      augment=(split == "train"), remix_prob=remix_prob, seed=seed)
