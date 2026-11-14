import numpy as np
import torch

import stft as reference  # models/tools/stft.py, the numpy reference of the engine's STFT
from stems_train.stft import stft_planes


def test_torch_stft_matches_reference():
    rng = np.random.default_rng(0)
    x = rng.standard_normal((2, 20000)).astype(np.float32)
    ref = reference.stft_frames(x, 1024)  # [N, 4, F]
    got = stft_planes(torch.from_numpy(x), 1024).numpy()  # [4, F, N]
    assert got.shape == (4, 513, ref.shape[0])
    np.testing.assert_allclose(got.transpose(2, 0, 1), ref, atol=2e-3, rtol=1e-4)


def test_batched_shapes():
    x = torch.randn(3, 4, 2, 8192)
    assert stft_planes(x, 2048).shape == (3, 4, 4, 1025, 16)
