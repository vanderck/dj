//! HT-Demucs (v4) as ONNX: high-quality offline separation of whole tracks.
//!
//! Uses the self-contained ONNX export from the Mixxx project
//! (<https://github.com/mixxxdj/demucs>, `htdemucs.onnx`): waveform in, four stems out, for one
//! fixed-length segment. Around it this module does what Demucs' own `apply_model` does, so the
//! result matches running Demucs in Python (without random shifts):
//!
//! - normalise the track by the mean and standard deviation of its mono mix;
//! - cut it into segments with 25 % overlap; a short last segment is padded with the audio
//!   *before* it (centred, as `TensorChunk.padded` does) and zeros after the end;
//! - blend overlapping outputs with triangular weights and divide by the summed weights;
//! - undo the normalisation.
//!
//! A "bag" of models (HT-Demucs FT: one fine-tuned model per stem) is several files, each with
//! metadata `demucs.stem` naming the one stem taken from it.
//!
//! Licence note: the Demucs code is MIT, but the pretrained weights were trained partly on MUSDB18
//! (non-commercial). Applications should let users supply the model files rather than ship them.

use std::path::Path;

use ort::session::Session;
use ort::value::TensorRef;

use crate::offline::{Frame, ModelError, OfflineError, Progress, StemBuffers};
use crate::onnx::{commit, err, Device, OnnxOptions};
use crate::resample::resample;
use crate::Stem;

/// Demucs' source order, unless a model's `demucs.sources` metadata says otherwise.
pub const DEFAULT_SOURCES: [&str; 4] = ["drums", "bass", "other", "vocals"];
/// Metadata: comma-separated source names, in output order.
pub const META_SOURCES: &str = "demucs.sources";
/// Metadata: the one source to take from this model (bag of models).
pub const META_STEM: &str = "demucs.stem";
/// Metadata: display name.
pub const META_NAME: &str = "demucs.name";
/// Demucs v4 runs at 44.1 kHz.
pub const SAMPLE_RATE: u32 = 44_100;
/// Fraction of each segment shared with the next (Demucs' default).
pub const OVERLAP: f32 = 0.25;

/// A network that runs without HT-Demucs's signal processing (see [`crate::spec`]): the
/// normalised spectrogram and wave of a segment in, the sources' (still normalised) out.
pub(crate) trait Network: Send {
    /// `spec`: [4, BINS, frames], `wave`: [2, len] → ([S, 4, BINS, frames], [S, 2, len]).
    fn run(&mut self, spec: &[f32], wave: &[f32]) -> Result<(Vec<f32>, Vec<f32>), String>;
}

enum Runner {
    /// The whole of HT-Demucs in one ONNX model (Mixxx's export): wave in, sources out.
    Whole {
        session: Session,
        input_name: String,
    },
    /// Just the network; the spectrogram and its inverse here.
    Network(Box<dyn Network>),
}

/// The network part of HT-Demucs as ONNX (inputs `spec` and `wave`), as `tools/htdemucs_coreml`
/// writes it with `--onnx`: the same network as the Core ML model, for systems without it.
struct OnnxNetwork(Session);

impl Network for OnnxNetwork {
    fn run(&mut self, spec: &[f32], wave: &[f32]) -> Result<(Vec<f32>, Vec<f32>), String> {
        let frames = spec.len() / (4 * crate::spec::BINS);
        let len = wave.len() / 2;
        let s = TensorRef::from_array_view(([1usize, 4, crate::spec::BINS, frames], spec))
            .map_err(|e| e.to_string())?;
        let w = TensorRef::from_array_view(([1usize, 2, len], wave)).map_err(|e| e.to_string())?;
        let out = self
            .0
            .run(ort::inputs!["spec" => s, "wave" => w])
            .map_err(|e| e.to_string())?;
        let spec_out = out["spec_out"]
            .try_extract_tensor::<f32>()
            .map_err(|e| e.to_string())?
            .1
            .to_vec();
        let wave_out = out["wave_out"]
            .try_extract_tensor::<f32>()
            .map_err(|e| e.to_string())?
            .1
            .to_vec();
        Ok((spec_out, wave_out))
    }
}

struct Part {
    runner: Runner,
    /// Output source index -> our [`Stem`], for the sources taken from this model.
    take: Vec<(usize, Stem)>,
}

impl Part {
    /// The sources for one segment (`buf`: [2, seg]): [S, 2, seg].
    fn run(&mut self, buf: &[f32], seg: usize) -> Result<Vec<f32>, OfflineError> {
        let model = |e: String| OfflineError::Model(e);
        match &mut self.runner {
            Runner::Whole {
                session,
                input_name,
            } => {
                let tensor = TensorRef::from_array_view(([1usize, 2, seg], buf))
                    .map_err(|e| model(e.to_string()))?;
                let outputs = session
                    .run(ort::inputs![input_name.as_str() => tensor])
                    .map_err(|e| model(e.to_string()))?;
                let (shape, data) = outputs[0]
                    .try_extract_tensor::<f32>()
                    .map_err(|e| model(e.to_string()))?;
                if shape.len() != 4 || shape[2] != 2 || shape[3] as usize != seg {
                    return Err(model(format!(
                        "output shape {shape:?}, expected [1, S, 2, {seg}]"
                    )));
                }
                Ok(data.to_vec())
            }
            Runner::Network(net) => {
                let inputs = crate::spec::inputs(buf, seg);
                let (spec_out, wave_out) = net.run(&inputs.spec, &inputs.wave).map_err(model)?;
                let sources = wave_out.len() / (2 * seg);
                if sources == 0
                    || wave_out.len() != sources * 2 * seg
                    || spec_out.len() != sources * inputs.spec.len()
                {
                    return Err(model(format!(
                        "network outputs of {} and {} values, for {seg} samples",
                        spec_out.len(),
                        wave_out.len()
                    )));
                }
                Ok(crate::spec::sources(&inputs, &spec_out, &wave_out, seg))
            }
        }
    }
}

pub struct DemucsModel {
    parts: Vec<Part>,
    segment: usize,
    name: String,
    device: Device,
}

fn stem_from_name(name: &str) -> Option<Stem> {
    match name.trim().to_ascii_lowercase().as_str() {
        "drums" => Some(Stem::Drums),
        "bass" => Some(Stem::Bass),
        "vocals" => Some(Stem::Vocals),
        "other" => Some(Stem::Other),
        _ => None,
    }
}

impl DemucsModel {
    /// One file (HT-Demucs) or one file per stem (HT-Demucs FT, each with `demucs.stem`).
    pub fn from_files<P: AsRef<Path>>(
        paths: &[P],
        options: &OnnxOptions,
    ) -> Result<Self, ModelError> {
        if paths.is_empty() {
            return Err(ModelError::new("no Demucs model files given"));
        }
        let mut parts = Vec::new();
        let mut segment = None;
        let mut name = None;
        let mut device = None;
        for path in paths {
            let path = path.as_ref();
            if crate::coreml::is_coreml(path) {
                // HT-Demucs's network for Core ML (the Neural Engine): one model, all stems.
                let (net, n, on) = crate::coreml::load(path)?;
                device = Some(on);
                segment = Some(n);
                parts.push(Part {
                    runner: Runner::Network(net),
                    take: DEFAULT_SOURCES
                        .iter()
                        .enumerate()
                        .filter_map(|(i, s)| Some((i, stem_from_name(s)?)))
                        .collect(),
                });
                continue;
            }
            let (session, on) = commit(options, path)?;
            // (All parts on the GPU, or the first reason one isn't.)
            if device
                .as_ref()
                .is_none_or(|d| matches!(d, Device::Accelerator(_)))
            {
                device = Some(on);
            }
            let (n, part, meta_name) = Self::part(session, path)?;
            match segment {
                None => segment = Some(n),
                Some(s) if s != n => {
                    return Err(ModelError::new(format!(
                        "{}: segment of {n} samples, the other model files use {s}",
                        path.display()
                    )))
                }
                _ => {}
            }
            name = name.or(meta_name);
            parts.push(part);
        }
        // Every stem must come from exactly one model.
        for stem in Stem::ALL {
            let n = parts
                .iter()
                .flat_map(|p| &p.take)
                .filter(|(_, s)| *s == stem)
                .count();
            if n != 1 {
                return Err(ModelError::new(format!(
                    "{} model files provide the {} stem {n} times (expected once)",
                    paths.len(),
                    stem.name()
                )));
            }
        }
        let name = name.unwrap_or_else(|| {
            if parts.len() > 1 {
                "HT-Demucs FT".into()
            } else {
                "HT-Demucs".into()
            }
        });
        Ok(Self {
            parts,
            segment: segment.unwrap(),
            name,
            device: device.unwrap_or(Device::Cpu(None)),
        })
    }

    fn part(session: Session, path: &Path) -> Result<(usize, Part, Option<String>), ModelError> {
        let bad = |what: String| ModelError::new(format!("{}: {what}", path.display()));
        // The whole model (one input: the wave) or just its network (`spec` and `wave`).
        let network = session.inputs().iter().any(|i| i.name() == "spec");
        let input = session
            .inputs()
            .iter()
            .find(|i| !network || i.name() == "wave")
            .ok_or_else(|| bad("no inputs".into()))?;
        let input_name = input.name().to_string();
        let dims: Vec<i64> = input
            .dtype()
            .tensor_shape()
            .map(|s| s.to_vec())
            .unwrap_or_default();
        let segment = match dims.as_slice() {
            [1, 2, n] if *n > 0 => *n as usize,
            _ => {
                return Err(bad(format!(
                    "input shape {dims:?}, expected [1, 2, samples]"
                )))
            }
        };
        let meta = session.metadata().map_err(err)?;
        let sources: Vec<String> = meta
            .custom(META_SOURCES)
            .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
            .unwrap_or_else(|| DEFAULT_SOURCES.iter().map(|s| s.to_string()).collect());
        let only = meta.custom(META_STEM);
        let name = meta.custom(META_NAME);
        drop(meta);
        let take: Vec<(usize, Stem)> = sources
            .iter()
            .enumerate()
            .filter_map(|(i, s)| Some((i, stem_from_name(s)?)))
            .filter(|(_, stem)| {
                only.as_deref()
                    .is_none_or(|o| stem_from_name(o) == Some(*stem))
            })
            .collect();
        if take.is_empty() {
            return Err(bad(format!(
                "no usable stems (sources {sources:?}, stem {only:?})"
            )));
        }
        let runner = if network {
            Runner::Network(Box::new(OnnxNetwork(session)))
        } else {
            Runner::Whole {
                session,
                input_name,
            }
        };
        Ok((segment, Part { runner, take }, name))
    }

    /// Where it runs (see [`OnnxOptions::gpu`]).
    pub fn device(&self) -> &Device {
        &self.device
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Samples per segment (343 980 for HT-Demucs: 7.8 s).
    pub fn segment(&self) -> usize {
        self.segment
    }

    /// Separate a whole track at any sample rate. Each stem has `input.len()` frames.
    pub fn separate(
        &mut self,
        input: &[Frame],
        input_rate: u32,
        progress: Progress,
    ) -> Result<StemBuffers, OfflineError> {
        let mut out: StemBuffers = Default::default();
        if input.is_empty() {
            return Ok(out);
        }
        let audio = resample(input, input_rate, SAMPLE_RATE);
        let n = audio.len();

        // Normalise by the mono mix (unbiased std, as torch.std).
        let mono = |f: &Frame| 0.5 * (f[0] as f64 + f[1] as f64);
        let mean = audio.iter().map(mono).sum::<f64>() / n as f64;
        let var =
            audio.iter().map(|f| (mono(f) - mean).powi(2)).sum::<f64>() / (n.max(2) - 1) as f64;
        let scale = var.sqrt() + 1e-8;
        let (mean32, scale32) = (mean as f32, scale as f32);
        let norm: [Vec<f32>; 2] =
            std::array::from_fn(|c| audio.iter().map(|f| (f[c] - mean32) / scale32).collect());
        drop(audio);

        let seg = self.segment;
        let stride = ((1.0 - OVERLAP) * seg as f32) as usize;
        // Triangle 1..=seg/2, then seg-seg/2 down to 1, normalised to a peak of 1.
        let half = seg / 2;
        let peak = half.max(seg - half) as f32;
        let weight: Vec<f32> = (1..=half)
            .chain((1..=seg - half).rev())
            .map(|w| w as f32 / peak)
            .collect();
        let offsets: Vec<usize> = (0..n).step_by(stride.max(1)).collect();

        let mut acc: [Vec<Frame>; 4] = std::array::from_fn(|_| Vec::new());
        for part in &self.parts {
            for (_, stem) in &part.take {
                acc[*stem as usize] = vec![[0.0; 2]; n];
            }
        }
        let mut sum_weight = vec![0.0f32; n];
        for &offset in &offsets {
            let len = seg.min(n - offset);
            for (i, w) in weight[..len].iter().enumerate() {
                sum_weight[offset + i] += w;
            }
        }

        let total = (offsets.len() * self.parts.len()) as f32;
        let mut done = 0.0f32;
        let mut buf = vec![0.0f32; 2 * seg];
        for part in &mut self.parts {
            for &offset in &offsets {
                if !progress(0.98 * done / total) {
                    return Err(OfflineError::Cancelled);
                }
                let len = seg.min(n - offset);
                // Centred padding with real context where it exists (TensorChunk.padded).
                let delta = seg - len;
                let start = offset as isize - (delta / 2) as isize;
                for c in 0..2 {
                    for (i, v) in buf[c * seg..(c + 1) * seg].iter_mut().enumerate() {
                        let k = start + i as isize;
                        *v = if k >= 0 && (k as usize) < n {
                            norm[c][k as usize]
                        } else {
                            0.0
                        };
                    }
                }
                let data = part.run(&buf, seg)?;
                let sources = data.len() / (2 * seg);
                // Centre trim back to the chunk (center_trim), then weight and accumulate.
                let left = delta / 2;
                for &(src, stem) in &part.take {
                    if src >= sources {
                        return Err(OfflineError::Model(format!(
                            "model has {sources} sources, wanted index {src}"
                        )));
                    }
                    let dst = &mut acc[stem as usize][offset..offset + len];
                    for c in 0..2 {
                        let ch =
                            &data[(src * 2 + c) * seg + left..(src * 2 + c) * seg + left + len];
                        for ((d, &x), &w) in dst.iter_mut().zip(ch).zip(&weight[..len]) {
                            d[c] += w * x;
                        }
                    }
                }
                done += 1.0;
            }
        }

        for (stem, buf) in out.iter_mut().zip(acc) {
            let mut s: Vec<Frame> = buf
                .into_iter()
                .zip(&sum_weight)
                .map(|(f, &w)| [f[0] / w * scale32 + mean32, f[1] / w * scale32 + mean32])
                .collect();
            if input_rate != SAMPLE_RATE {
                s = resample(&s, SAMPLE_RATE, input_rate);
            }
            s.resize(input.len(), [0.0; 2]);
            *stem = s;
        }
        progress(1.0);
        Ok(out)
    }
}
