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
    /// Run on an accelerator where this build can: CoreML on macOS (the Neural Engine, not the
    /// GPU: CoreML's GPU work holds up the app's own drawing, which runs on the same GPU, until
    /// the window hangs), DirectML on Windows. A model (or a system) it can't use runs on the CPU
    /// (see [`commit`]).
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

/// Where a model runs, for messages: an accelerator ("Neural Engine (CoreML)", "GPU
/// (DirectML)"), the CPU, or the CPU and why not the accelerator asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum Device {
    Accelerator(&'static str),
    Cpu(Option<String>),
}

impl std::fmt::Display for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Device::Accelerator(name) => write!(f, "{name}"),
            Device::Cpu(None) => write!(f, "CPU"),
            Device::Cpu(Some(why)) => write!(f, "CPU (no acceleration: {why})"),
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
        // On macOS the Neural Engine needs the network in 16 bits (see `fp16`): CoreML runs a
        // 32-bit model on the CPU, slower than ONNX Runtime's own CPU code.
        let accelerated = || {
            let (mut b, ep) = gpu_builder(options)?;
            let model = if cfg!(target_os = "macos") {
                half_model(options, path)?
            } else {
                path.to_path_buf()
            };
            let session = b
                .commit_from_file(&model)
                .map_err(|e| ModelError::new(format!("loading {}: {e}", model.display())))?;
            Ok::<_, ModelError>((session, ep))
        };
        match accelerated() {
            Ok((session, ep)) => return Ok((session, Device::Accelerator(ep))),
            Err(e) => return Ok((load(builder(options)?)?, Device::Cpu(Some(e.to_string())))),
        }
    }
    Ok((load(builder(options)?)?, Device::Cpu(None)))
}

/// `path`'s model with its network in 16 bits (see [`crate::fp16`]), made once (a few seconds)
/// into `options.gpu_cache` (else next to it) and reused while the model file stays the same.
pub fn half_model(
    options: &OnnxOptions,
    path: &std::path::Path,
) -> Result<std::path::PathBuf, ModelError> {
    let io = |e: std::io::Error| ModelError::new(format!("16-bit model: {e}"));
    let dir = match &options.gpu_cache {
        Some(d) => d.clone(),
        None => path.parent().map(|p| p.to_path_buf()).unwrap_or_default(),
    };
    std::fs::create_dir_all(&dir).map_err(io)?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let len = std::fs::metadata(path).map_err(io)?.len();
    let dst = dir.join(format!("{stem}.{len}.fp16.onnx"));
    if dst.exists() {
        return Ok(dst);
    }
    // ONNX Runtime's basic optimisations first: shapes worked out and constants folded, so
    // what's left is the network's own operators, every tensor's type plain to see.
    let folded = dir.join(format!("{stem}.{len}.folded.tmp"));
    let session = builder(options)?
        .with_optimization_level(GraphOptimizationLevel::Level1)
        .map_err(err)?
        .with_optimized_model_path(&folded)
        .map_err(err)?
        .commit_from_file(path)
        .map_err(|e| ModelError::new(format!("loading {}: {e}", path.display())));
    drop(session?);
    let bytes = std::fs::read(&folded).map_err(io);
    let _ = std::fs::remove_file(&folded);
    let (half, _) = crate::fp16::half_precision(&bytes?)?;
    let tmp = dst.with_extension("tmp");
    std::fs::write(&tmp, half).map_err(io)?;
    std::fs::rename(&tmp, &dst).map_err(io)?;
    Ok(dst)
}

/// A session builder with this system's GPU provider, failing (rather than quietly running on
/// the CPU) when it can't be used.
#[cfg(target_os = "macos")]
fn gpu_builder(options: &OnnxOptions) -> Result<(SessionBuilder, &'static str), ModelError> {
    use ort::ep::coreml::{ComputeUnits, ModelFormat, SpecializationStrategy};
    let mut ep = ort::ep::CoreML::default()
        .with_model_format(ModelFormat::MLProgram)
        .with_specialization_strategy(SpecializationStrategy::FastPrediction)
        // (Not the GPU: see `OnnxOptions::gpu`. Ops the Neural Engine can't run go to the CPU.)
        .with_compute_units(ComputeUnits::CPUAndNeuralEngine);
    if let Some(dir) = &options.gpu_cache {
        let _ = std::fs::create_dir_all(dir);
        ep = ep.with_model_cache_dir(dir.display());
    }
    let b = builder(options)?
        .with_execution_providers([ep.build().error_on_failure()])
        .map_err(err)?;
    Ok((b, "Neural Engine (CoreML)"))
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
    Ok((b, "GPU (DirectML)"))
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
