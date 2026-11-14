import torch

from stems_train.config import ModelConfig
from stems_train.model import ExportWrapper, StemsNet, band_groups

SMALL = ModelConfig(fft_size=512, bands_per_group=3, dim=16, dilations=(2, 1), lookahead_frames=2)


def make(cfg=SMALL):
    torch.manual_seed(0)
    return StemsNet(cfg).eval()


def test_band_groups_tile_all_bins():
    for bins in (2049, 1025, 513, 257):
        pos = 0
        for start, width, count in band_groups(bins, 2, 8):
            assert start == pos and width >= 1 and count >= 1
            pos += width * count
        assert pos == bins
    assert sum(c for _, _, c in band_groups(2049, 2, 8)) == 57


def test_output_shape_and_partition_of_unity():
    net = make()
    T = SMALL.context_frames
    m = net(torch.randn(2, 4, SMALL.num_bins, T + 9))
    assert m.shape == (2, 4, SMALL.num_bins, 10)
    torch.testing.assert_close(m.sum(1), torch.ones(2, SMALL.num_bins, 10))


def test_sequence_equals_sliding_window():
    """Training on sequences computes exactly what the plugin computes window by window."""
    net, T = make(), SMALL.context_frames
    seq = torch.randn(1, 4, SMALL.num_bins, T + 6) * 5
    with torch.no_grad():
        full = net(seq)
        wrapper = ExportWrapper(net)
        for j in range(full.shape[-1]):
            window = seq[..., j : j + T].permute(0, 3, 1, 2)  # [1, T, 4, F]
            torch.testing.assert_close(wrapper(window), full[..., j], atol=1e-5, rtol=1e-4)


def test_respects_lookahead():
    """Frames after target + lookahead must not influence the masks (they are outside the
    window by construction); frames inside the window must."""
    net, T, L = make(), SMALL.context_frames, SMALL.lookahead_frames
    x = torch.randn(1, 4, SMALL.num_bins, T)
    target = T - 1 - L
    with torch.no_grad():
        base = net(x)
        for frame, should_change in [(T - 1, True), (target, True), (0, True)]:
            y = x.clone()
            y[..., frame] += 3.0
            changed = (net(y) - base).abs().max() > 1e-6
            assert bool(changed) == should_change, frame
    assert net.target_frames(T + 5) == slice(target, target + 6)


def test_default_config_budget():
    cfg = ModelConfig()
    assert cfg.context_frames == 19  # 1 + 2 * (4 + 2 + 2 + 1)
    assert cfg.latency_samples() == 4096 + 5 * 1024  # ≈ 209 ms at 44.1 kHz
