"""Train the separation model.

    python -m stems_train.train --data synthetic --out runs/synth --steps 2000
    python -m stems_train.train --data /data/multitracks --out runs/v1 --steps 200000 --batch-size 16

Runs on CUDA, Apple Silicon (MPS) or CPU (`--device auto` picks the best available). STFTs are
computed on CPU in the data-loader workers, so the model never needs complex tensors on device.
"""
import argparse
import json
import math
import time
from pathlib import Path

import torch
from torch.utils.data import DataLoader

from .config import STEMS, ModelConfig, TrainConfig
from .data import make_dataset
from .losses import apply_masks, sdr_energies, sdr_from_energies, separation_loss
from .model import StemsNet, count_parameters
from .stft import stft_planes


def pick_device(name: str) -> torch.device:
    if name != "auto":
        return torch.device(name)
    if torch.cuda.is_available():
        return torch.device("cuda")
    if torch.backends.mps.is_available():
        return torch.device("mps")
    return torch.device("cpu")


class Collate:
    """Stacks stems and computes STFT planes (runs inside loader workers)."""

    def __init__(self, fft_size: int):
        self.fft_size = fft_size

    def __call__(self, batch):
        stems = torch.stack(batch)  # [B, S, 2, n]
        with torch.no_grad():
            mix = stft_planes(stems.sum(1), self.fft_size)  # [B, 4, F, N]
            tgt = stft_planes(stems, self.fft_size)  # [B, S, 4, F, N]
        return mix, tgt


def make_loader(ds, cfg: TrainConfig, shuffle_workers: bool) -> DataLoader:
    workers = cfg.num_workers if shuffle_workers else 0
    return DataLoader(
        ds,
        batch_size=cfg.batch_size,
        num_workers=workers,
        collate_fn=Collate(cfg.model.fft_size),
        drop_last=shuffle_workers,
        persistent_workers=workers > 0,
    )


def step_loss(model: StemsNet, mix, tgt, sdr_weight: float = 0.05):
    masks = model(mix)
    frames = model.target_frames(mix.shape[-1])
    mix, tgt = mix[..., frames], tgt[..., frames]
    est = apply_masks(masks, mix)
    return separation_loss(est, tgt, mix, sdr_weight), est, tgt, mix


@torch.no_grad()
def evaluate(model, loader, device):
    """-> per-stem SDR of the model, the equal-split floor and the oracle ceiling over the whole
    loader. Energies are summed batch by batch, so the evaluation set can be large."""
    model.eval()
    totals = None
    for mix, tgt in loader:
        _, est, t, m = step_loss(model, mix.to(device), tgt.to(device))
        parts = (
            sdr_energies(est, t),
            # Floor: every stem gets a quarter of the mix (what an untrained model outputs).
            sdr_energies(0.25 * m[:, None].expand_as(t), t),
            # Ceiling: oracle ratio masks computed from the true stems.
            sdr_energies(apply_masks(oracle_masks(t), m), t),
        )
        parts = [(a.cpu().double(), b.cpu().double()) for a, b in parts]  # MPS has no float64
        totals = parts if totals is None else [(a + c, b + d) for (a, b), (c, d) in zip(totals, parts)]
    model.train()
    return tuple(sdr_from_energies(a, b).float() for a, b in totals)


def oracle_masks(tgt: torch.Tensor) -> torch.Tensor:
    """Ideal ratio masks |S_s| / sum_s |S_s| from target planes [B, S, 4, F, n] -> [B, S, F, n]."""
    mag = tgt.reshape(*tgt.shape[:2], 2, 2, *tgt.shape[-2:]).pow(2).sum(3).sum(2).sqrt()
    return mag / (mag.sum(1, keepdim=True) + 1e-8)


def save(path: Path, model, opt, sched, step, cfg: TrainConfig, metrics=None):
    torch.save(
        {
            "model_config": cfg.model.to_dict(),
            "state_dict": model.state_dict(),
            "optimizer": opt.state_dict(),
            "scheduler": sched.state_dict(),
            "step": step,
            "metrics": metrics or {},
        },
        path,
    )


def parse_args(argv=None) -> TrainConfig:
    d, m = TrainConfig(), ModelConfig()
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--data", default=d.data)
    p.add_argument("--out", default=d.out)
    p.add_argument("--steps", type=int, default=d.steps)
    p.add_argument("--batch-size", type=int, default=d.batch_size)
    p.add_argument("--segment-seconds", type=float, default=d.segment_seconds)
    p.add_argument("--lr", type=float, default=d.lr)
    p.add_argument("--warmup-steps", type=int, default=d.warmup_steps)
    p.add_argument("--num-workers", type=int, default=d.num_workers)
    p.add_argument("--device", default=d.device)
    p.add_argument("--seed", type=int, default=d.seed)
    p.add_argument("--log-every", type=int, default=d.log_every)
    p.add_argument("--eval-every", type=int, default=d.eval_every)
    p.add_argument("--eval-at", type=float, default=d.eval_at,
                   help="seconds into each validation song where its evaluation clip starts")
    p.add_argument("--eval-clips", type=int, default=d.eval_clips,
                   help="evaluate on at most this many validation songs (0 = all)")
    p.add_argument("--ckpt-every", type=int, default=d.ckpt_every)
    p.add_argument("--remix-prob", type=float, default=d.remix_prob)
    p.add_argument("--sdr-weight", type=float, default=d.sdr_weight,
                   help="weight of the negative-SDR loss term (0 = compressed L1 only)")
    p.add_argument("--resume", default=None, help="checkpoint to continue from")
    p.add_argument("--sample-rate", type=int, default=m.sample_rate)
    p.add_argument("--fft-size", type=int, default=m.fft_size)
    p.add_argument("--lookahead-frames", type=int, default=m.lookahead_frames)
    p.add_argument("--band-width", type=int, default=m.band_width)
    p.add_argument("--bands-per-group", type=int, default=m.bands_per_group)
    p.add_argument("--dim", type=int, default=m.dim)
    p.add_argument("--dilations", default=",".join(map(str, m.dilations)))
    a = p.parse_args(argv)
    model = ModelConfig(
        sample_rate=a.sample_rate,
        fft_size=a.fft_size,
        lookahead_frames=a.lookahead_frames,
        band_width=a.band_width,
        bands_per_group=a.bands_per_group,
        dim=a.dim,
        dilations=tuple(int(x) for x in a.dilations.split(",")),
    )
    cfg = TrainConfig(
        data=a.data, out=a.out, steps=a.steps, batch_size=a.batch_size,
        segment_seconds=a.segment_seconds, lr=a.lr, warmup_steps=a.warmup_steps,
        num_workers=a.num_workers, device=a.device, seed=a.seed, log_every=a.log_every,
        eval_every=a.eval_every, eval_at=a.eval_at, eval_clips=a.eval_clips, ckpt_every=a.ckpt_every, remix_prob=a.remix_prob,
        sdr_weight=a.sdr_weight, model=model,
    )
    cfg.resume = a.resume
    return cfg


def train(cfg: TrainConfig) -> dict:
    torch.manual_seed(cfg.seed)
    out = Path(cfg.out)
    out.mkdir(parents=True, exist_ok=True)
    device = pick_device(cfg.device)
    m = cfg.model

    start_step = 0
    resume = getattr(cfg, "resume", None)
    ckpt = torch.load(resume, map_location="cpu") if resume else None
    if ckpt:
        cfg.model = m = ModelConfig.from_dict(ckpt["model_config"])
        start_step = ckpt["step"]

    model = StemsNet(m).to(device)
    opt = torch.optim.AdamW(model.parameters(), lr=cfg.lr, weight_decay=cfg.weight_decay)

    def lr_lambda(step):
        if step < cfg.warmup_steps:
            return (step + 1) / cfg.warmup_steps
        progress = (step - cfg.warmup_steps) / max(1, cfg.steps - cfg.warmup_steps)
        return 0.02 + 0.98 * 0.5 * (1 + math.cos(math.pi * min(1.0, progress)))

    sched = torch.optim.lr_scheduler.LambdaLR(opt, lr_lambda)
    if ckpt:
        model.load_state_dict(ckpt["state_dict"])
        opt.load_state_dict(ckpt["optimizer"])
        sched.load_state_dict(ckpt["scheduler"])

    remaining = cfg.steps - start_step
    train_ds = make_dataset(cfg.data, "train", m.sample_rate, cfg.segment_seconds,
                            length=max(1, remaining) * cfg.batch_size, remix_prob=cfg.remix_prob, seed=cfg.seed)
    val_ds = make_dataset(cfg.data, "valid", m.sample_rate, cfg.segment_seconds, length=2 * cfg.batch_size,
                          seed=cfg.seed, snapshot_seconds=cfg.eval_at, max_snapshots=cfg.eval_clips)
    train_loader = make_loader(train_ds, cfg, shuffle_workers=True)
    val_loader = make_loader(val_ds, cfg, shuffle_workers=False)

    print(
        f"device {device} · {count_parameters(model) / 1e6:.2f}M params · context {m.context_frames} frames · "
        f"lookahead {m.lookahead_frames} · plugin latency {m.latency_samples()} samples "
        f"({1000 * m.latency_samples() / m.sample_rate:.0f} ms) · eval on {len(val_ds)} clips",
        flush=True,
    )
    (out / "config.json").write_text(json.dumps({"model": m.to_dict(), "train": {
        k: v for k, v in vars(cfg).items() if k != "model"}}, indent=2))

    log = (out / "log.jsonl").open("a")
    best = -float("inf")
    metrics = {}
    step = start_step
    t0, running = time.time(), 0.0
    model.train()
    for mix, tgt in train_loader:
        if step >= cfg.steps:
            break
        mix, tgt = mix.to(device, non_blocking=True), tgt.to(device, non_blocking=True)
        loss, *_ = step_loss(model, mix, tgt, cfg.sdr_weight)
        opt.zero_grad(set_to_none=True)
        loss.backward()
        torch.nn.utils.clip_grad_norm_(model.parameters(), cfg.grad_clip)
        opt.step()
        sched.step()
        step += 1
        running += loss.item()

        if step % cfg.log_every == 0:
            rate = cfg.log_every / (time.time() - t0)
            rec = {"step": step, "loss": running / cfg.log_every, "lr": sched.get_last_lr()[0], "steps_per_s": rate}
            print(f"step {step:>7} loss {rec['loss']:.4f} lr {rec['lr']:.2e} {rate:.2f} it/s", flush=True)
            log.write(json.dumps(rec) + "\n")
            log.flush()
            t0, running = time.time(), 0.0

        if step % cfg.eval_every == 0 or step == cfg.steps:
            sdr, base, oracle = evaluate(model, val_loader, device)
            metrics = {"step": step, "sdr": dict(zip(STEMS, sdr.tolist())), "mean_sdr": sdr.mean().item(),
                       "baseline_sdr": dict(zip(STEMS, base.tolist())), "baseline_mean_sdr": base.mean().item(),
                       "oracle_sdr": dict(zip(STEMS, oracle.tolist())), "oracle_mean_sdr": oracle.mean().item()}
            print("  eval " + " ".join(f"{s} {v:+.2f}dB" for s, v in metrics["sdr"].items())
                  + f" · mean {metrics['mean_sdr']:+.2f}dB"
                  + f" (equal-split floor {base.mean():+.2f}dB, oracle ceiling {oracle.mean():+.2f}dB)", flush=True)
            log.write(json.dumps({"eval": metrics}) + "\n")
            log.flush()
            if metrics["mean_sdr"] > best:
                best = metrics["mean_sdr"]
                save(out / "best.pt", model, opt, sched, step, cfg, metrics)

        if step % cfg.ckpt_every == 0 or step == cfg.steps:
            save(out / "last.pt", model, opt, sched, step, cfg, metrics)

    log.close()
    return metrics


def main(argv=None):
    train(parse_args(argv))


if __name__ == "__main__":
    main()
