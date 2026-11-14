"""Synthetic multitrack generator: toy drums, bass, vocals and other with realistic-ish
frequency ranges and stereo placement. Lets the full pipeline train and be verified without any
licensed data. Models trained on it will NOT separate real music.
"""
import numpy as np


def _env(n, attack, decay, sr):
    t = np.arange(n) / sr
    a = np.clip(t / max(attack, 1e-4), 0, 1)
    return a * np.exp(-np.maximum(t - attack, 0) / decay)


def _pan(mono, pan):
    """Constant-power pan, pan in [-1, 1] -> [2, n]."""
    th = (pan + 1) * np.pi / 4
    return np.stack([mono * np.cos(th), mono * np.sin(th)])


def make_stems(sr: int, seconds: float, rng: np.random.Generator) -> np.ndarray:
    """-> [4 stems, 2 channels, samples] float32 in contract stem order."""
    n = int(sr * seconds)
    t = np.arange(n) / sr
    bpm = rng.uniform(85, 140)
    beat = 60.0 / bpm
    stems = np.zeros((4, 2, n), dtype=np.float64)

    # Drums: kick on beats, snare on 2 & 4, hats on eighths.
    drums = np.zeros(n)
    for i, start in enumerate(np.arange(rng.uniform(0, beat), seconds, beat / 2)):
        s = int(start * sr)
        m = min(n - s, int(0.4 * sr))
        if m <= 0:
            continue
        tt = np.arange(m) / sr
        if i % 2 == 0:
            if (i // 2) % 2 == 0 or rng.random() < 0.2:  # kick
                f = 50 + 70 * np.exp(-tt / 0.03)
                drums[s : s + m] += 0.9 * np.sin(2 * np.pi * np.cumsum(f) / sr) * _env(m, 0.002, 0.12, sr)
            else:  # snare
                drums[s : s + m] += 0.5 * rng.standard_normal(m) * _env(m, 0.001, 0.08, sr)
                drums[s : s + m] += 0.3 * np.sin(2 * np.pi * 190 * tt) * _env(m, 0.001, 0.05, sr)
        hat = rng.standard_normal(m)
        hat = np.diff(np.diff(hat, prepend=0), prepend=0)  # crude high-pass
        drums[s : s + m] += 0.12 * hat * _env(m, 0.0005, 0.02, sr)
    stems[0] = _pan(drums, rng.uniform(-0.15, 0.15))

    # Bass: one note per beat, 40-110 Hz, a few harmonics.
    bass = np.zeros(n)
    for start in np.arange(0, seconds, beat):
        s, m = int(start * sr), min(n - int(start * sr), int(beat * sr))
        if m <= 0:
            continue
        f0 = rng.uniform(40, 110)
        tt = np.arange(m) / sr
        tone = sum(np.sin(2 * np.pi * f0 * k * tt) / k**1.5 for k in range(1, 5))
        bass[s : s + m] += 0.5 * tone * _env(m, 0.01, beat * 0.8, sr)
    stems[1] = _pan(bass, 0.0)

    # Vocals: center, vibrato, formant-shaped harmonics, phrases with gaps.
    vox = np.zeros(n)
    pos = rng.uniform(0, 0.5)
    while pos < seconds:
        dur = rng.uniform(0.4, 1.6)
        s, m = int(pos * sr), min(n - int(pos * sr), int(dur * sr))
        if m > 0:
            tt = np.arange(m) / sr
            f0 = rng.uniform(150, 450) * (1 + 0.012 * np.sin(2 * np.pi * rng.uniform(4.5, 6.5) * tt))
            phase = 2 * np.pi * np.cumsum(f0) / sr
            formant = rng.uniform(600, 2500)
            tone = sum(
                np.sin(k * phase) * np.exp(-(((k * f0.mean()) - formant) ** 2) / (2 * 900.0**2)) / k**0.5
                for k in range(1, 20)
            )
            vox[s : s + m] += 0.4 * tone * _env(m, 0.05, dur, sr) * np.minimum(1, (m - np.arange(m)) / (0.05 * sr))
        pos += dur + rng.uniform(0.1, 0.8)
    stems[2] = _pan(vox, rng.uniform(-0.05, 0.05))

    # Other: two panned chord pads, 200-2000 Hz.
    for side in (-1, 1):
        pad = np.zeros(n)
        for start in np.arange(0, seconds, 4 * beat):
            s, m = int(start * sr), min(n - int(start * sr), int(4 * beat * sr))
            if m <= 0:
                continue
            tt = np.arange(m) / sr
            root = rng.uniform(200, 500)
            chord = sum(
                np.sin(2 * np.pi * root * r * tt + rng.uniform(0, 6)) + 0.3 * np.sin(4 * np.pi * root * r * tt)
                for r in (1.0, 1.26, 1.5)
            )
            pad[s : s + m] += 0.12 * chord * _env(m, 0.3, 4 * beat, sr)
        stems[3] += _pan(pad, side * rng.uniform(0.4, 0.9))

    peak = np.abs(stems.sum(0)).max() + 1e-9
    return (stems * (0.8 / peak)).astype(np.float32)
