//! HT-Demucs's network on Core ML (macOS): a model made by `tools/htdemucs_coreml` (32-bit:
//! 16 bits changes the stems audibly; inputs `spec` and `wave`, outputs `spec_out` and
//! `wave_out`), with the spectrogram and its inverse done here ([`crate::spec`]). Several times
//! faster than ONNX Runtime on the CPU.
//!
//! A compiled model (`.mlmodelc`) loads in a second; a package (`.mlpackage`) is compiled first.

use std::path::Path;

use crate::demucs::Network;
use crate::offline::ModelError;
use crate::onnx::Device;

/// Whether `path` is a Core ML model.
pub(crate) fn is_coreml(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("mlmodelc") || e.eq_ignore_ascii_case("mlpackage"))
}

/// The model at `path`, on Core ML: the network, its segment length.
#[cfg(not(all(target_os = "macos", feature = "coreml")))]
pub(crate) fn load(path: &Path) -> Result<(Box<dyn Network>, usize, Device), ModelError> {
    Err(ModelError::new(format!(
        "{}: Core ML models run on macOS only",
        path.display()
    )))
}

#[cfg(all(target_os = "macos", feature = "coreml"))]
pub(crate) use mac::load;

#[cfg(all(target_os = "macos", feature = "coreml"))]
mod mac {
    use std::path::Path;
    use std::ptr::NonNull;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, ProtocolObject};
    use objc2::AnyThread;
    use objc2_core_ml::{
        MLComputeUnits, MLDictionaryFeatureProvider, MLFeatureProvider, MLFeatureValue, MLModel,
        MLModelConfiguration, MLMultiArray, MLMultiArrayDataType,
    };
    use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSString, NSURL};

    use super::{Device, ModelError, Network};

    fn ns_err(what: &str, e: Retained<NSError>) -> ModelError {
        ModelError::new(format!("{what}: {}", e.localizedDescription()))
    }

    struct CoreMl {
        model: Retained<MLModel>,
    }

    // SAFETY: an MLModel may be used from any thread; this one is used from one at a time (the
    // separation thread that owns it).
    unsafe impl Send for CoreMl {}

    pub(crate) fn load(path: &Path) -> Result<(Box<dyn Network>, usize, Device), ModelError> {
        let url = NSURL::from_file_path(path)
            .ok_or_else(|| ModelError::new(format!("{}: not a file path", path.display())))?;
        let compiled = if path.extension().is_some_and(|e| e == "mlpackage") {
            // SAFETY: plain Core ML calls on a valid URL.
            #[allow(deprecated)]
            unsafe { MLModel::compileModelAtURL_error(&url) }
                .map_err(|e| ns_err("compiling the Core ML model", e))?
        } else {
            url
        };
        // SAFETY: plain Core ML calls with valid arguments.
        let model = unsafe {
            let config = MLModelConfiguration::new();
            // Not the GPU: the app draws on it, and Core ML's work there holds up the window. (A
            // 32-bit model runs on the CPU; the Neural Engine takes only 16-bit work.)
            config.setComputeUnits(MLComputeUnits::CPUAndNeuralEngine);
            MLModel::modelWithContentsOfURL_configuration_error(&compiled, &config)
        }
        .map_err(|e| ns_err(&format!("loading {}", path.display()), e))?;
        // The segment: the `wave` input's last dimension.
        // SAFETY: reading the model's description.
        let segment = unsafe {
            model
                .modelDescription()
                .inputDescriptionsByName()
                .objectForKey(&NSString::from_str("wave"))
                .and_then(|d| d.multiArrayConstraint())
                .map(|c| c.shape())
                .and_then(|s| s.lastObject())
                .map(|n| n.integerValue() as usize)
        }
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            ModelError::new(format!(
                "{}: no `wave` input of [1, 2, samples]",
                path.display()
            ))
        })?;
        Ok((
            Box::new(CoreMl { model }),
            segment,
            Device::Accelerator("Core ML"),
        ))
    }

    /// `data` (contiguous) as a Float32 multi-array of `shape`, without copying: the array
    /// borrows `data`, so it must not outlive it.
    unsafe fn borrow_array(
        data: &[f32],
        shape: &[usize],
    ) -> Result<Retained<MLMultiArray>, ModelError> {
        let mut strides = vec![1usize; shape.len()];
        for i in (0..shape.len().saturating_sub(1)).rev() {
            strides[i] = strides[i + 1] * shape[i + 1];
        }
        let nums = |v: &[usize]| {
            NSArray::from_retained_slice(
                &v.iter()
                    .map(|&n| NSNumber::numberWithInteger(n as isize))
                    .collect::<Vec<_>>(),
            )
        };
        MLMultiArray::initWithDataPointer_shape_dataType_strides_deallocator_error(
            MLMultiArray::alloc(),
            NonNull::new(data.as_ptr() as *mut std::ffi::c_void).expect("a slice's pointer"),
            &nums(shape),
            MLMultiArrayDataType::Float32,
            &nums(&strides),
            None,
        )
        .map_err(|e| ns_err("an input array", e))
    }

    /// A Float32 (or Float16) multi-array's values in order, whatever its strides.
    unsafe fn read_array(a: &MLMultiArray) -> Result<Vec<f32>, ModelError> {
        let shape: Vec<usize> = a
            .shape()
            .iter()
            .map(|n| n.integerValue() as usize)
            .collect();
        let strides: Vec<usize> = a
            .strides()
            .iter()
            .map(|n| n.integerValue() as usize)
            .collect();
        let count: usize = shape.iter().product();
        let span = shape
            .iter()
            .zip(&strides)
            .map(|(n, s)| n.saturating_sub(1) * s)
            .sum::<usize>()
            + 1;
        #[allow(deprecated)]
        let ptr = a.dataPointer();
        let dtype = a.dataType();
        let value: Box<dyn Fn(usize) -> f32> = if dtype == MLMultiArrayDataType::Float32 {
            let p = std::slice::from_raw_parts(ptr.as_ptr() as *const f32, span);
            Box::new(move |i| p[i])
        } else if dtype == MLMultiArrayDataType::Float16 {
            let p = std::slice::from_raw_parts(ptr.as_ptr() as *const u16, span);
            Box::new(move |i| f16_to_f32(p[i]))
        } else {
            return Err(ModelError::new(format!(
                "an output of data type {:?}, not float",
                dtype
            )));
        };
        let mut out = Vec::with_capacity(count);
        let mut idx = vec![0usize; shape.len()];
        for _ in 0..count {
            out.push(value(idx.iter().zip(&strides).map(|(i, s)| i * s).sum()));
            for d in (0..shape.len()).rev() {
                idx[d] += 1;
                if idx[d] < shape[d] {
                    break;
                }
                idx[d] = 0;
            }
        }
        Ok(out)
    }

    fn f16_to_f32(h: u16) -> f32 {
        let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
        let exp = ((h >> 10) & 0x1f) as i32;
        let man = (h & 0x3ff) as f32;
        sign * match exp {
            0 => man * (2.0f32).powi(-24),
            0x1f if man == 0.0 => f32::INFINITY,
            0x1f => f32::NAN,
            e => (1.0 + man / 1024.0) * (2.0f32).powi(e - 15),
        }
    }

    impl Network for CoreMl {
        fn run(&mut self, spec: &[f32], wave: &[f32]) -> Result<(Vec<f32>, Vec<f32>), String> {
            let bins = crate::spec::BINS;
            let frames = spec.len() / (4 * bins);
            let len = wave.len() / 2;
            // SAFETY: the arrays borrow `spec` and `wave`, which outlive the prediction (it's
            // synchronous and the arrays are dropped at the end of this block).
            unsafe {
                let s = borrow_array(spec, &[1, 4, bins, frames]).map_err(|e| e.to_string())?;
                let w = borrow_array(wave, &[1, 2, len]).map_err(|e| e.to_string())?;
                let (sv, wv) = (
                    MLFeatureValue::featureValueWithMultiArray(&s),
                    MLFeatureValue::featureValueWithMultiArray(&w),
                );
                let keys = [NSString::from_str("spec"), NSString::from_str("wave")];
                let values: [&AnyObject; 2] = [&sv, &wv];
                let dict: Retained<NSDictionary<NSString, AnyObject>> =
                    NSDictionary::from_slices(&[&*keys[0], &*keys[1]], &values);
                let input = MLDictionaryFeatureProvider::initWithDictionary_error(
                    MLDictionaryFeatureProvider::alloc(),
                    &dict,
                )
                .map_err(|e| ns_err("the inputs", e).to_string())?;
                let out = self
                    .model
                    .predictionFromFeatures_error(ProtocolObject::from_ref(&*input))
                    .map_err(|e| ns_err("Core ML", e).to_string())?;
                let get = |name: &str| -> Result<Vec<f32>, String> {
                    let v = out
                        .featureValueForName(&NSString::from_str(name))
                        .and_then(|v| v.multiArrayValue())
                        .ok_or_else(|| format!("no output `{name}`"))?;
                    read_array(&v).map_err(|e| e.to_string())
                };
                let spec_out = get("spec_out")?;
                let wave_out = get("wave_out")?;
                drop((input, dict, sv, wv, s, w));
                Ok((spec_out, wave_out))
            }
        }
    }
}
