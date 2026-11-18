//! ONNX Runtime backend for [`MaskModel`] (features `onnx` or `onnx-dynamic`).
//!
//! The model's STFT parameters are read from ONNX metadata (see `docs/MODEL_CONTRACT.md`), so a
//! new model file can change FFT size, context and lookahead without code changes.

use std::path::Path;
use std::sync::Mutex;

use ort::environment::Environment;
use ort::session::builder::{GraphOptimizationLevel, SessionBuilder};
use ort::session::Session;
use ort::value::TensorRef;

use crate::model::{MaskModel, ModelError, ModelSpec, INPUT_PLANES};

/// Metadata keys every model must define.
pub const META_CONTRACT: &str = "stems.contract";
pub const META_SAMPLE_RATE: &str = "stems.sample_rate";
pub const META_FFT_SIZE: &str = "stems.fft_size";
pub const META_CONTEXT_FRAMES: &str = "stems.context_frames";
pub const META_LOOKAHEAD_FRAMES: &str = "stems.lookahead_frames";
/// Optional display name.
pub const META_NAME: &str = "stems.name";
/// Contract version this code implements.
pub const CONTRACT_VERSION: &str = "1";

#[derive(Debug, Clone)]
pub struct OnnxOptions {
    /// Threads ONNX Runtime may use inside one inference.
    pub intra_threads: usize,
}

impl Default for OnnxOptions {
    fn default() -> Self {
        Self { intra_threads: 2 }
    }
}

pub struct OnnxModel {
    session: Session,
    spec: ModelSpec,
    name: String,
    input_name: String,
}

impl OnnxModel {
    pub fn from_file(path: impl AsRef<Path>, options: &OnnxOptions) -> Result<Self, ModelError> {
        let path = path.as_ref();
        let session = builder(options)?
            .commit_from_file(path)
            .map_err(|e| ModelError::new(format!("loading {}: {e}", path.display())))?;
        Self::from_session(session)
    }

    pub fn from_memory(bytes: &[u8], options: &OnnxOptions) -> Result<Self, ModelError> {
        let session = builder(options)?
            .commit_from_memory(bytes)
            .map_err(|e| ModelError::new(format!("loading embedded model: {e}")))?;
        Self::from_session(session)
    }

    fn from_session(session: Session) -> Result<Self, ModelError> {
        let meta = session.metadata().map_err(err)?;
        let get = |key: &str| {
            meta.custom(key)
                .ok_or_else(|| ModelError::new(format!("model metadata is missing `{key}`")))
        };
        let contract = get(META_CONTRACT)?;
        if contract != CONTRACT_VERSION {
            return Err(ModelError::new(format!(
                "model uses contract v{contract}, this build supports v{CONTRACT_VERSION}"
            )));
        }
        let num = |key: &str| -> Result<f64, ModelError> {
            get(key)?
                .trim()
                .parse()
                .map_err(|_| ModelError::new(format!("metadata `{key}` is not a number")))
        };
        let spec = ModelSpec {
            sample_rate: num(META_SAMPLE_RATE)? as f32,
            fft_size: num(META_FFT_SIZE)? as usize,
            context_frames: num(META_CONTEXT_FRAMES)? as usize,
            lookahead_frames: num(META_LOOKAHEAD_FRAMES)? as usize,
        };
        spec.validate()?;
        let name = meta
            .custom(META_NAME)
            .unwrap_or_else(|| "onnx model".into());
        drop(meta);

        let input_name = session
            .inputs()
            .first()
            .ok_or_else(|| ModelError::new("model has no inputs"))?
            .name()
            .to_string();

        let mut model = Self {
            session,
            spec,
            name,
            input_name,
        };

        // Dry run: catches shape mismatches at load time instead of on the worker thread.
        let context = vec![0.0; spec.input_len()];
        let mut masks = vec![0.0; spec.output_len()];
        model.infer(&context, &mut masks)?;
        Ok(model)
    }
}

impl MaskModel for OnnxModel {
    fn spec(&self) -> ModelSpec {
        self.spec
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn infer(&mut self, context: &[f32], masks: &mut [f32]) -> Result<(), ModelError> {
        let s = self.spec;
        let shape = [1, s.context_frames, INPUT_PLANES, s.num_bins()];
        let input = TensorRef::from_array_view((shape, context)).map_err(err)?;
        let outputs = self
            .session
            .run(ort::inputs![self.input_name.as_str() => input])
            .map_err(err)?;
        let (_, data) = outputs[0].try_extract_tensor::<f32>().map_err(err)?;
        if data.len() != masks.len() {
            return Err(ModelError::new(format!(
                "model output has {} values, expected {} (= 4 stems x {} bins)",
                data.len(),
                masks.len(),
                s.num_bins()
            )));
        }
        masks.copy_from_slice(data);
        Ok(())
    }
}

pub(crate) fn err(e: impl std::fmt::Display) -> ModelError {
    ModelError::new(e.to_string())
}

pub(crate) fn builder(options: &OnnxOptions) -> Result<SessionBuilder, ModelError> {
    let env = environment()?;
    Session::builder(&env)
        .map_err(err)?
        .with_optimization_level(GraphOptimizationLevel::All)
        .map_err(err)?
        .with_intra_threads(options.intra_threads.max(1))
        .map_err(err)
}

/// The process-wide ONNX Runtime environment, created on first use.
fn environment() -> Result<Environment, ModelError> {
    static ENV: Mutex<Option<Environment>> = Mutex::new(None);
    let mut env = ENV.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(env) = env.as_ref() {
        return Ok(env.clone());
    }
    let built = init_builder()?.with_name("stems").build().map_err(err)?;
    *env = Some(built.clone());
    Ok(built)
}

#[cfg(feature = "onnx-dynamic")]
fn init_builder() -> Result<ort::environment::EnvironmentBuilder, ModelError> {
    // Resolve the library ourselves so a missing runtime is an error, not a panic.
    let path = std::env::var("ORT_DYLIB_PATH")
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| {
            format!(
                "{}onnxruntime{}",
                std::env::consts::DLL_PREFIX,
                std::env::consts::DLL_SUFFIX
            )
        });
    ort::init_from(&path)
        .map_err(|e| ModelError::new(format!("cannot load ONNX Runtime from `{path}`: {e}")))
}

#[cfg(not(feature = "onnx-dynamic"))]
fn init_builder() -> Result<ort::environment::EnvironmentBuilder, ModelError> {
    Ok(ort::init())
}
