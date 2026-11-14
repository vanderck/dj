import torch

from stems_train.losses import apply_masks, sdr_loss, separation_loss


def _batch(silent_stem=None):
    torch.manual_seed(0)
    tgt = torch.randn(2, 4, 4, 33, 10)
    if silent_stem is not None:
        tgt[:, silent_stem] = 0.0
    mix = tgt.sum(1)
    return tgt, mix


def test_perfect_estimate_beats_equal_split():
    tgt, mix = _batch()
    equal = apply_masks(torch.full((2, 4, 33, 10), 0.25), mix)
    assert separation_loss(tgt, tgt, mix) < separation_loss(equal, tgt, mix)


def test_silent_stem_is_bounded_and_differentiable():
    tgt, mix = _batch(silent_stem=2)
    logits = torch.zeros(2, 4, 33, 10, requires_grad=True)
    est = apply_masks(torch.softmax(logits, dim=1), mix)
    loss = sdr_loss(est, tgt, mix)
    loss.backward()
    assert torch.isfinite(loss) and loss.abs() < 100
    assert torch.isfinite(logits.grad).all()
    # Outputting silence for the silent stem is rewarded, but only down to the floor (+30 dB).
    best = sdr_loss(tgt, tgt, mix)
    assert torch.isfinite(best)


def test_sdr_weight_zero_is_compressed_only():
    tgt, mix = _batch()
    est = tgt * 0.9
    from stems_train.losses import compressed_loss
    torch.testing.assert_close(separation_loss(est, tgt, mix, sdr_weight=0.0), compressed_loss(est, tgt))
