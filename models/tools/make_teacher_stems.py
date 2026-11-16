"""Distillation data: separate a music library with a large offline model (the "teacher",
default HT-Demucs FT) and write the stems in the training layout:

    out/{train,valid}/<song>/{drums,bass,vocals,other}.flac

    pip install demucs
    python models/tools/make_teacher_stems.py ~/Music/library data/teacher --device mps

The real-time model is then trained on these pseudo-stems:

    python -m stems_train.train --data data/teacher --out runs/distill

Only use music you have the rights to, and check the teacher's licence and training data before
using a model distilled from it commercially (see models/README.md).
"""
import argparse
import hashlib
import json
import random
import re
import sys
from pathlib import Path

import numpy as np
import soundfile as sf

STEMS = ("drums", "bass", "vocals", "other")
AUDIO_SUFFIXES = {".wav", ".flac", ".mp3", ".ogg", ".aif", ".aiff"}


def find_audio(root: Path) -> list:
    """Audio files under `root`, skipping hidden files and folders. That includes macOS "._*"
    AppleDouble files, which carry an audio extension on non-Mac volumes but are not audio."""
    return sorted(
        p
        for p in root.rglob("*")
        if p.suffix.lower() in AUDIO_SUFFIXES
        and not any(part.startswith(".") for part in p.relative_to(root).parts)
        and p.is_file()
    )


def song_id(path: Path) -> str:
    """From the file name only, so the same file reached through different folders (a folder of
    shortcuts, the library itself) gets the same id, the same train/valid split, and is
    separated once."""
    name = path.stem
    slug = re.sub(r"[^A-Za-z0-9._-]+", "_", name).strip("_")[:80]
    return f"{slug}-{hashlib.sha1(name.encode()).hexdigest()[:8]}"


def split_for(name: str, valid_fraction: float) -> str:
    h = int(hashlib.sha1(name.encode()).hexdigest()[:8], 16) / 0xFFFFFFFF
    return "valid" if h < valid_fraction else "train"


def read_stereo(path: Path, excerpt_seconds: float = 0.0, excerpt_start: float = None):
    """Whole file, or `excerpt_seconds` from `excerpt_start` seconds in (from the middle when
    that is not given or the song is too short for it)."""
    info = sf.info(str(path))
    start, frames = 0, -1
    if excerpt_seconds > 0 and info.frames > excerpt_seconds * info.samplerate:
        frames = int(excerpt_seconds * info.samplerate)
        start = (info.frames - frames) // 2  # middle of the song
        if excerpt_start is not None and (excerpt_start + excerpt_seconds) * info.samplerate <= info.frames:
            start = int(excerpt_start * info.samplerate)
    x, sr = sf.read(str(path), start=start, frames=frames, dtype="float32", always_2d=True)
    x = x.T
    if x.shape[0] == 1:
        x = np.repeat(x, 2, axis=0)
    return np.ascontiguousarray(x[:2]), sr


def process(songs_root, out, separate, sample_rate=44100, valid_fraction=0.05,
            excerpt_seconds=0.0, overwrite=False, teacher="unknown", log=print,
            excerpt_start=None, shuffle=False, limit=0, seed=0) -> dict:
    """`shuffle` visits songs in a random (seeded, so repeatable) order, so stopping early still
    gives a sample of the whole library; `limit` stops after that many newly separated songs."""
    songs_root, out = Path(songs_root), Path(out)
    counts = {"done": 0, "skipped": 0, "failed": 0}
    files = find_audio(songs_root)
    if shuffle:
        random.Random(seed).shuffle(files)
    for i, path in enumerate(files):
        if limit and counts["done"] >= limit:
            break
        name = song_id(path)
        dest = out / split_for(name, valid_fraction) / name
        if not overwrite and all((dest / f"{s}.flac").exists() for s in STEMS):
            counts["skipped"] += 1
            continue
        try:
            audio, sr = read_stereo(path, excerpt_seconds, excerpt_start)
            stems = separate(audio, sr)
            dest.mkdir(parents=True, exist_ok=True)
            for s in STEMS:
                y = np.clip(stems[s], -1.0, 1.0)
                sf.write(str(dest / f"{s}.flac"), y.T, sample_rate, subtype="PCM_16")
            (dest / "source.json").write_text(json.dumps({"source": str(path), "teacher": teacher}))
            counts["done"] += 1
            log(f"[{i + 1}/{len(files)}] {name}")
        except Exception as e:  # keep going on unreadable files
            counts["failed"] += 1
            log(f"[{i + 1}/{len(files)}] FAILED {path}: {e}")
    return counts


def demucs_separator(model_name: str, device: str, shifts: int, overlap: float, sample_rate: int):
    import torch
    from demucs.apply import apply_model
    from demucs.pretrained import get_model
    from julius import resample_frac

    if device == "auto":
        device = "cuda" if torch.cuda.is_available() else "mps" if torch.backends.mps.is_available() else "cpu"
    model = get_model(model_name).eval()
    missing = set(STEMS) - set(model.sources)
    if missing:
        raise SystemExit(f"{model_name} has no {missing} source(s); pick a 4-stem model")

    @torch.no_grad()
    def separate(audio: np.ndarray, sr: int) -> dict:
        wav = torch.from_numpy(audio)
        if sr != model.samplerate:
            wav = resample_frac(wav, sr, model.samplerate)
        ref = wav.mean(0)
        mean, std = ref.mean(), ref.std() + 1e-8
        out = apply_model(model, ((wav - mean) / std)[None], device=device, shifts=shifts,
                          split=True, overlap=overlap, progress=False)[0]
        out = out * std + mean
        if model.samplerate != sample_rate:
            out = resample_frac(out, model.samplerate, sample_rate)
        return {s: out[model.sources.index(s)].cpu().numpy() for s in STEMS}

    return separate


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("songs", help="folder of songs (searched recursively)")
    p.add_argument("out", help="output dataset folder")
    p.add_argument("--model", default="htdemucs_ft", help="demucs model name (default: htdemucs_ft)")
    p.add_argument("--device", default="auto", help="auto, mps, cuda or cpu")
    p.add_argument("--shifts", type=int, default=1, help="demucs test-time shifts (quality vs speed)")
    p.add_argument("--overlap", type=float, default=0.25)
    p.add_argument("--sample-rate", type=int, default=44100)
    p.add_argument("--valid-fraction", type=float, default=0.05)
    p.add_argument("--excerpt-seconds", type=float, default=0.0,
                   help="only separate this many seconds of each song (0 = all)")
    p.add_argument("--excerpt-start", type=float, default=None,
                   help="where the excerpt starts, in seconds (default: centred in the song)")
    p.add_argument("--shuffle", action="store_true",
                   help="visit songs in random order, so stopping early still samples the whole library")
    p.add_argument("--limit", type=int, default=0, help="stop after this many newly separated songs")
    p.add_argument("--overwrite", action="store_true")
    a = p.parse_args(argv)
    sep = demucs_separator(a.model, a.device, a.shifts, a.overlap, a.sample_rate)
    counts = process(a.songs, a.out, sep, a.sample_rate, a.valid_fraction, a.excerpt_seconds,
                     a.overwrite, teacher=a.model, excerpt_start=a.excerpt_start,
                     shuffle=a.shuffle, limit=a.limit)
    print(counts)
    return 0 if counts["failed"] == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
