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


def test_skips_macos_metadata_and_hidden_files(tmp_path):
    (tmp_path / "album").mkdir()
    (tmp_path / ".hidden").mkdir()
    for name in ["album/song.mp3", "album/._song.mp3", ".hidden/x.flac", "._top.wav", "top.wav", "notes.txt"]:
        (tmp_path / name).write_bytes(b"x")
    found = [p.relative_to(tmp_path).as_posix() for p in teacher.find_audio(tmp_path)]
    assert found == ["album/song.mp3", "top.wav"]


def _ramp_song(path, sr, seconds):
    t = np.arange(int(sr * seconds)) / sr
    sf.write(path, np.stack([t, t], 1) * 0.001, sr, subtype="FLOAT")  # value = time in ms


def test_excerpt_start_and_fallback(tmp_path):
    sr = 8000
    _ramp_song(tmp_path / "long.wav", sr, 100)
    _ramp_song(tmp_path / "short.wav", sr, 40)
    x, _ = teacher.read_stereo(tmp_path / "long.wav", 5.0, excerpt_start=60.0)
    assert x.shape == (2, 5 * sr) and np.isclose(x[0, 0], 0.060)
    x, _ = teacher.read_stereo(tmp_path / "short.wav", 5.0, excerpt_start=60.0)  # too short: middle
    assert np.isclose(x[0, 0], 0.0175)


def test_same_file_via_two_folders_is_separated_once(tmp_path):
    sr = 8000
    (tmp_path / "library" / "artist").mkdir(parents=True)
    (tmp_path / "sample").mkdir()
    for i in range(4):
        _ramp_song(tmp_path / "library" / "artist" / f"song {i}.wav", sr, 3)
    (tmp_path / "sample" / "song 1.wav").symlink_to(tmp_path / "library" / "artist" / "song 1.wav")
    out = tmp_path / "data"
    quiet = dict(excerpt_seconds=1.0, log=lambda *_: None)
    assert teacher.process(tmp_path / "sample", out, fake_separator, **quiet)["done"] == 1
    again = teacher.process(tmp_path / "library", out, fake_separator, **quiet)
    assert again == {"done": 3, "skipped": 1, "failed": 0}
    assert len(list(out.glob("*/*"))) == 4


def test_shuffle_and_limit(tmp_path):
    sr = 8000
    (tmp_path / "lib").mkdir()
    for i in range(20):
        _ramp_song(tmp_path / "lib" / f"{i:02d}.wav", sr, 2)
    out = tmp_path / "data"
    counts = teacher.process(tmp_path / "lib", out, fake_separator, excerpt_seconds=1.0, shuffle=True,
                             limit=5, seed=3, log=lambda *_: None)
    assert counts["done"] == 5
    names = sorted(p.name.split("-")[0] for p in out.glob("*/*"))
    assert names != [f"{i:02d}" for i in range(5)]  # not just the first five alphabetically
    more = teacher.process(tmp_path / "lib", out, fake_separator, excerpt_seconds=1.0, shuffle=True,
                           limit=5, seed=3, log=lambda *_: None)
    assert more == {"done": 5, "skipped": 5, "failed": 0}  # same order: resumes after the first five


def test_eval_snapshots_can_be_capped(tmp_path):
    sr = 8000
    for i in range(10):
        d = tmp_path / "valid" / f"t{i}"
        d.mkdir(parents=True)
        for s in teacher.STEMS:
            _ramp_song(d / f"{s}.wav", sr, 2)
    ds = StemFolder(str(tmp_path), "valid", sr, 1.0, length=1, snapshot_seconds=0.0, max_snapshots=4)
    assert len(ds) == 4 and ds.snapshot_tracks == [0, 2, 5, 7]
    assert len(StemFolder(str(tmp_path), "valid", sr, 1.0, length=1, snapshot_seconds=0.0)) == 10


def test_demucs_addon_chunks_join_seamlessly():
    import demucs_stems

    sr = 1000
    rng = np.random.default_rng(0)
    audio = rng.standard_normal((2, 10 * sr + 123)).astype(np.float32)
    calls, seen = [], []

    def fake(chunk, rate):  # a "separator" that depends on the chunk, like a real one would
        calls.append(chunk.shape[1])
        return {s: chunk * w for s, w in zip(teacher.STEMS, (0.1, 0.2, 0.3, 0.4))}

    out = demucs_stems.separate_file(audio, sr, fake, chunk_seconds=3.0, overlap_seconds=0.5,
                                     progress=seen.append)
    for s, w in zip(teacher.STEMS, (0.1, 0.2, 0.3, 0.4)):
        np.testing.assert_allclose(out[s], audio * w, atol=1e-6)
    assert len(calls) == 4 and seen[-1] == 1.0 and seen == sorted(seen)


def test_demucs_addon_writes_the_protocol_files(tmp_path, monkeypatch, capsys):
    import demucs_stems

    sr = 8000
    x = np.random.default_rng(1).standard_normal((sr * 2, 2)).astype(np.float32) * 0.1
    sf.write(tmp_path / "in.wav", x, sr, subtype="FLOAT")
    monkeypatch.setattr(demucs_stems, "demucs_separator", lambda *a: fake_separator)
    assert demucs_stems.main([str(tmp_path / "in.wav"), str(tmp_path / "out")]) == 0
    total = sum(sf.read(tmp_path / "out" / f"{s}.wav")[0] for s in teacher.STEMS)
    np.testing.assert_allclose(total, x, atol=1e-6)
    lines = capsys.readouterr().out.splitlines()
    assert lines[0] == "name HT-Demucs FT (external)" and lines[-1] == "progress 1.000"
