"""Demucs add-on for Collective's "External separator" setting.

Separates one audio file with HT-Demucs into four stem files, following Collective's external
separator protocol: it is called as `demucs_stems.py INPUT OUTPUT_DIR`, writes
OUTPUT_DIR/{drums,bass,vocals,other}.wav (32-bit float, at the input's sample rate) and prints
`name ...` and `progress 0.42` lines.

    pip install demucs
    python models/tools/demucs_stems.py song.wav stems/

In Collective (Settings → Stems → External separator), with the paths of your checkout:

    /Users/you/stems/models/.venv/bin/python /Users/you/stems/models/tools/demucs_stems.py

Licence: the Demucs code is MIT, but the pretrained weights were trained partly on MUSDB18
(non-commercial research licence). Fine for your own use; do not ship them in a product
without legal advice. That is why this is an add-on and not part of Collective.
"""
import argparse
import sys
from pathlib import Path

import numpy as np
import soundfile as sf

from make_teacher_stems import STEMS, demucs_separator

NAMES = {"htdemucs_ft": "HT-Demucs FT", "htdemucs": "HT-Demucs", "htdemucs_6s": "HT-Demucs 6s"}


def separate_file(audio: np.ndarray, sr: int, separate, chunk_seconds: float = 60.0,
                  overlap_seconds: float = 4.0, progress=lambda p: None) -> dict:
    """Separate [2, n] audio in chunks (bounded memory, progress reports), crossfading the
    overlaps linearly so the chunks join seamlessly. -> {stem: [2, n]}."""
    n = audio.shape[1]
    chunk, overlap = int(chunk_seconds * sr), int(overlap_seconds * sr)
    out = {s: np.zeros_like(audio) for s in STEMS}
    pos = 0
    while True:
        end = min(n, pos + chunk)
        stems = separate(np.ascontiguousarray(audio[:, pos:end]), sr)
        w = np.ones(end - pos, dtype=audio.dtype)
        if pos > 0:
            w[:overlap] = np.linspace(0.0, 1.0, overlap, endpoint=False)
        if end < n:  # complements the next chunk's fade-in sample for sample
            w[-overlap:] = 1.0 - np.linspace(0.0, 1.0, overlap, endpoint=False)
        for s in STEMS:
            out[s][:, pos:end] += stems[s][:, : end - pos] * w
        progress(end / n)
        if end >= n:
            return out
        pos = end - overlap


def read_audio(path: Path):
    x, sr = sf.read(str(path), dtype="float32", always_2d=True)
    x = x.T
    if x.shape[0] == 1:
        x = np.repeat(x, 2, axis=0)
    return np.ascontiguousarray(x[:2]), sr


def main(argv=None) -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("input")
    p.add_argument("output_dir")
    p.add_argument("--model", default="htdemucs_ft", help="demucs model (default: htdemucs_ft)")
    p.add_argument("--device", default="auto", help="auto, mps, cuda or cpu")
    p.add_argument("--shifts", type=int, default=1, help="test-time shifts: better but slower")
    a = p.parse_args(argv)

    say = lambda line: print(line, flush=True)  # noqa: E731
    say(f"name {NAMES.get(a.model, a.model)} (external)")
    audio, sr = read_audio(Path(a.input))
    separate = demucs_separator(a.model, a.device, a.shifts, 0.25, sr)
    stems = separate_file(audio, sr, separate, progress=lambda f: say(f"progress {f:.3f}"))
    out = Path(a.output_dir)
    out.mkdir(parents=True, exist_ok=True)
    for s in STEMS:
        sf.write(str(out / f"{s}.wav"), stems[s].T, sr, subtype="FLOAT")
    return 0


if __name__ == "__main__":
    sys.exit(main())
