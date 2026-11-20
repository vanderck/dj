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
}

impl Default for OnnxOptions {
    fn default() -> Self {
        Self { intra_threads: 2 }
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
