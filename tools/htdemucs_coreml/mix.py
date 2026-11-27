"""A 20 s test mix for checking a Demucs model against another: kick, hats, a bass line, chords
and a sung-like line, as interleaved 32-bit float stereo at 44.1 kHz on stdout."""

import sys

import numpy as np

SR = 44100
t = np.arange(20 * SR) / SR
beat = 60 / 124
rng = np.random.default_rng(7)
x = np.zeros_like(t)
for k in range(int(20 / beat)):
    s = int(k * beat * SR)
    n = min(len(t) - s, 6000)
    x[s : s + n] += 0.6 * np.sin(2 * np.pi * 50 * np.arange(n) / SR) * np.exp(-np.arange(n) / 1500)
    h = s + int(beat / 2 * SR)
    m = max(0, min(len(t) - h, 1500))
    x[h : h + m] += 0.1 * rng.standard_normal(m) * np.exp(-np.arange(m) / 400)
notes = [55.0, 65.4, 49.0, 73.4]
bar = (t // (4 * beat)).astype(int) % 4
x += 0.25 * np.sin(2 * np.pi * np.take(notes, bar) * t)
x += 0.08 * sum(np.sin(2 * np.pi * f * np.take([1, 1.2, 0.9, 1.33], bar) * t) for f in (220, 277, 330))
x += 0.12 * np.sin(2 * np.pi * (440 + 8 * np.sin(2 * np.pi * 5 * t)) * t) * (np.sin(2 * np.pi * t / 4) > 0)
stereo = np.stack([x, np.roll(x, 300) * 0.9], axis=1).astype(np.float32)
sys.stdout.buffer.write(stereo.tobytes())
