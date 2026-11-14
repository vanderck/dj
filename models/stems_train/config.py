from dataclasses import asdict, dataclass, field

STEMS = ("drums", "bass", "vocals", "other")


@dataclass
class ModelConfig:
    sample_rate: int = 44100
    fft_size: int = 4096
    # Future frames the model may see beyond the frame it predicts (latency!).
    lookahead_frames: int = 3
    # Band split: groups of `bands_per_group` bands, widths band_width, 2x, 4x, ... bins.
    band_width: int = 2
    bands_per_group: int = 8
    dim: int = 64
    kernel: int = 3
    # Temporal receptive field = 1 + sum((kernel - 1) * d) frames = context_frames.
    # Large dilations first: the window shrinks fastest, so later layers see fewer frames.
    dilations: tuple = (4, 2, 2, 1)
    compress: float = 0.3  # magnitude compression exponent for input features

    @property
    def num_bins(self) -> int:
        return self.fft_size // 2 + 1

    @property
    def hop(self) -> int:
        return self.fft_size // 4

    @property
    def context_frames(self) -> int:
        return 1 + sum((self.kernel - 1) * d for d in self.dilations)

    def validate(self) -> None:
        assert self.fft_size & (self.fft_size - 1) == 0, "fft_size must be a power of two"
        assert 0 <= self.lookahead_frames < self.context_frames, "lookahead must be < context"
        assert self.band_width >= 1 and self.bands_per_group >= 1

    def to_dict(self) -> dict:
        d = asdict(self)
        d["dilations"] = list(self.dilations)
        return d

    @classmethod
    def from_dict(cls, d: dict) -> "ModelConfig":
        d = dict(d)
        d["dilations"] = tuple(d["dilations"])
        return cls(**d)

    def latency_samples(self, pipeline_frames: int = 2) -> int:
        return self.fft_size + (self.lookahead_frames + pipeline_frames) * self.hop


@dataclass
class TrainConfig:
    data: str = "synthetic"  # "synthetic" or a folder of <track>/{drums,bass,vocals,other}.wav
    out: str = "runs/default"
    steps: int = 20000
    batch_size: int = 8
    segment_seconds: float = 4.0
    lr: float = 1e-3
    weight_decay: float = 1e-2
    grad_clip: float = 5.0
    warmup_steps: int = 500
    num_workers: int = 2
    device: str = "auto"
    seed: int = 0
    log_every: int = 50
    eval_every: int = 1000
    ckpt_every: int = 1000
    remix_prob: float = 0.5  # probability of mixing stems from different tracks
    sdr_weight: float = 0.05  # weight of the negative-SDR term in the loss
    model: ModelConfig = field(default_factory=ModelConfig)
