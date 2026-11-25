//! ONNX Runtime setup for the Demucs models (features `onnx` or `onnx-dynamic`).

use std::sync::Mutex;

use ort::environment::Environment;
use ort::session::builder::{GraphOptimizationLevel, SessionBuilder};
use ort::session::Session;

use crate::offline::ModelError;

#[derive(Debug, Clone)]
pub struct OnnxOptions {
    /// Threads ONNX Runtime may use inside one inference.
    pub intra_threads: usize,
    /// Run on the GPU where this build can: CoreML on macOS (GPU and Neural Engine), DirectML
    /// on Windows. A model (or a system) it can't use runs on the CPU (see [`commit`]).
    pub gpu: bool,
    /// Where CoreML keeps the models it compiled (compiling takes minutes the first time).
    pub gpu_cache: Option<std::path::PathBuf>,
}

impl Default for OnnxOptions {
    fn default() -> Self {
        Self {
            intra_threads: 2,
            gpu: false,
            gpu_cache: None,
        }
    }
}

/// Where a model runs: its device for messages ("GPU (CoreML)", "CPU", or "CPU (GPU: why
/// not)").
#[derive(Debug, Clone, PartialEq)]
pub enum Device {
    Gpu(&'static str),
    Cpu(Option<String>),
}

impl std::fmt::Display for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Device::Gpu(ep) => write!(f, "GPU ({ep})"),
            Device::Cpu(None) => write!(f, "CPU"),
            Device::Cpu(Some(why)) => write!(f, "CPU (GPU: {why})"),
        }
    }
}

/// A session for the model in `path`: on the GPU if `options.gpu` and it works there, else on
/// the CPU (and why not the GPU).
pub(crate) fn commit(
    options: &OnnxOptions,
    path: &std::path::Path,
) -> Result<(Session, Device), ModelError> {
    let load = |mut b: SessionBuilder| {
        b.commit_from_file(path)
            .map_err(|e| ModelError::new(format!("loading {}: {e}", path.display())))
    };
    if options.gpu {
        match gpu_builder(options).and_then(|(b, ep)| Ok((load(b)?, ep))) {
            Ok((session, ep)) => return Ok((session, Device::Gpu(ep))),
            Err(e) => return Ok((load(builder(options)?)?, Device::Cpu(Some(e.to_string())))),
        }
    }
    Ok((load(builder(options)?)?, Device::Cpu(None)))
}

/// A session builder with this system's GPU provider, failing (rather than quietly running on
/// the CPU) when it can't be used.
#[cfg(target_os = "macos")]
fn gpu_builder(options: &OnnxOptions) -> Result<(SessionBuilder, &'static str), ModelError> {
    use ort::ep::coreml::{ComputeUnits, ModelFormat};
    let mut ep = ort::ep::CoreML::default()
        .with_model_format(ModelFormat::MLProgram)
        .with_compute_units(ComputeUnits::All);
    if let Some(dir) = &options.gpu_cache {
        let _ = std::fs::create_dir_all(dir);
        ep = ep.with_model_cache_dir(dir.display());
    }
    let b = builder(options)?
        .with_execution_providers([ep.build().error_on_failure()])
        .map_err(err)?;
    Ok((b, "CoreML"))
}

#[cfg(windows)]
fn gpu_builder(options: &OnnxOptions) -> Result<(SessionBuilder, &'static str), ModelError> {
    // (DirectML runs one inference at a time per session, and wants no memory pattern.)
    let b = builder(options)?
        .with_memory_pattern(false)
        .map_err(err)?
        .with_parallel_execution(false)
        .map_err(err)?
        .with_execution_providers([ort::ep::DirectML::default().build().error_on_failure()])
        .map_err(err)?;
    Ok((b, "DirectML"))
}

#[cfg(not(any(target_os = "macos", windows)))]
fn gpu_builder(_: &OnnxOptions) -> Result<(SessionBuilder, &'static str), ModelError> {
    Err(ModelError::new("not supported on this system"))
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
