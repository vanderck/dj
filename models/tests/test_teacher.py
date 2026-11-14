import numpy as np
import soundfile as sf

import make_teacher_stems as teacher
from stems_train.data import StemFolder


def fake_separator(audio, sr):
    """Stand-in for HT-Demucs: splits the mix evenly."""
    assert audio.shape[0] == 2
    return {s: audio * 0.25 for s in teacher.STEMS}


def test_writes_training_layout_and_resumes(tmp_path):
    sr = 44100
    songs = tmp_path / "songs" / "album"
    songs.mkdir(parents=True)
    for i in range(6):
        x = 0.3 * np.sin(2 * np.pi * 220 * (i + 1) * np.arange(sr * 3) / sr)
        sf.write(songs / f"song {i}.flac", x if i % 2 else np.stack([x, x], 1), sr)
    (songs / "notes.txt").write_text("not audio")
    out = tmp_path / "data"

    counts = teacher.process(tmp_path / "songs", out, fake_separator, valid_fraction=0.3,
                             excerpt_seconds=2.0, log=lambda *_: None)
    assert counts == {"done": 6, "skipped": 0, "failed": 0}
    tracks = sorted(out.glob("*/*"))
    assert len(tracks) == 6
    assert {t.parent.name for t in tracks} <= {"train", "valid"}
    y, ysr = sf.read(tracks[0] / "bass.flac")
    assert ysr == sr and y.shape == (2 * sr, 2)

    again = teacher.process(tmp_path / "songs", out, fake_separator, valid_fraction=0.3,
                            excerpt_seconds=2.0, log=lambda *_: None)
    assert again["skipped"] == 6

    split = "train" if any(out.glob("train/*")) else "valid"
    ds = StemFolder(str(out), split, sr, 1.0, length=2)
    assert ds[0].shape == (4, 2, sr)


def test_split_is_deterministic():
    names = [f"song-{i}" for i in range(2000)]
    valid = [n for n in names if teacher.split_for(n, 0.05) == "valid"]
    assert 50 < len(valid) < 150
    assert valid == [n for n in names if teacher.split_for(n, 0.05) == "valid"]
