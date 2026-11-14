//! Plugin glue: parameters, host IO, latency reporting and the editor.
//! All signal processing lives in `stems-core`.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use nih_plug::prelude::*;
use stems_core::{SoundfieldFrame, StemEngine};

mod editor;
mod params;
mod separator_choice;

use separator_choice::SeparatorInfo;

pub use params::{FocusModeParam, StemsParams};

/// Shared with the editor. The audio thread only touches the lock-free triple-buffer input.
pub(crate) struct UiShared {
    pub soundfield: Mutex<triple_buffer::Output<SoundfieldFrame>>,
    pub latency_samples: AtomicU32,
    pub sample_rate: AtomicU32,
    /// Only written from `initialize`, never from the audio thread.
    pub separator: Mutex<SeparatorInfo>,
}

pub struct Stems {
    params: Arc<StemsParams>,
    engine: Option<StemEngine>,
    soundfield_in: triple_buffer::Input<SoundfieldFrame>,
    last_published_frame: u64,
    ui: Arc<UiShared>,
}

impl Default for Stems {
    fn default() -> Self {
        let (soundfield_in, soundfield_out) =
            triple_buffer::TripleBuffer::new(&SoundfieldFrame::default()).split();
        Self {
            params: Arc::new(StemsParams::default()),
            engine: None,
            soundfield_in,
            last_published_frame: 0,
            ui: Arc::new(UiShared {
                soundfield: Mutex::new(soundfield_out),
                latency_samples: AtomicU32::new(0),
                sample_rate: AtomicU32::new(44_100),
                separator: Mutex::new(SeparatorInfo::default()),
            }),
        }
    }
}

impl Plugin for Stems {
    const NAME: &'static str = "Stems";
    const VENDOR: &'static str = "vanderck";
    const URL: &'static str = "https://github.com/vanderck/stems";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            aux_output_ports: &[new_nonzero_u32(2)],
            names: PortNames {
                layout: Some("Stereo"),
                main_input: Some("Input"),
                main_output: Some("Output"),
                aux_inputs: &[],
                aux_outputs: &["Focus"],
            },
            ..AudioIOLayout::const_default()
        },
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(1),
            main_output_channels: NonZeroU32::new(1),
            aux_output_ports: &[new_nonzero_u32(1)],
            names: PortNames {
                layout: Some("Mono"),
                main_input: Some("Input"),
                main_output: Some("Output"),
                aux_inputs: &[],
                aux_outputs: &["Focus"],
            },
            ..AudioIOLayout::const_default()
        },
    ];

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        editor::create(self.params.clone(), self.ui.clone())
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        context: &mut impl InitContext<Self>,
    ) -> bool {
        // Allocation (and model loading) is fine here; `process` must never allocate.
        let offline = buffer_config.process_mode == ProcessMode::Offline;
        let (engine, info) = separator_choice::build_engine(buffer_config.sample_rate, offline);
        if let Some(note) = &info.note {
            nih_log!("separator: {} ({note})", info.name);
        } else {
            nih_log!("separator: {}", info.name);
        }
        *self.ui.separator.lock().unwrap() = info;
        // Drop the previous engine (and join its worker thread) here, not on the audio thread.
        self.engine = None;
        let latency = engine.latency_samples() as u32;
        context.set_latency_samples(latency);
        self.ui.latency_samples.store(latency, Ordering::Relaxed);
        self.ui
            .sample_rate
            .store(buffer_config.sample_rate as u32, Ordering::Relaxed);
        self.engine = Some(engine);
        true
    }

    fn reset(&mut self) {
        if let Some(engine) = &mut self.engine {
            engine.reset();
        }
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let Some(engine) = &mut self.engine else {
            return ProcessStatus::Normal;
        };

        let settings = self.params.to_settings();
        engine.set_analysis_enabled(self.params.editor_state.is_open());

        let focus_out = aux.outputs.get_mut(0).map(|b| b.as_slice());
        engine.process(buffer.as_slice(), focus_out, &settings);

        if engine.frames_processed() != self.last_published_frame {
            self.last_published_frame = engine.frames_processed();
            *self.soundfield_in.input_buffer_mut() = *engine.soundfield();
            self.soundfield_in.publish();
        }

        ProcessStatus::Normal
    }
}

impl ClapPlugin for Stems {
    const CLAP_ID: &'static str = "com.github.vanderck.stems";
    const CLAP_DESCRIPTION: Option<&'static str> = Some("Real-time stem separation");
    const CLAP_MANUAL_URL: Option<&'static str> = Some(Self::URL);
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::AudioEffect,
        ClapFeature::Stereo,
        ClapFeature::Mono,
        ClapFeature::Utility,
    ];
}

nih_export_clap!(Stems);

#[cfg(feature = "vst3")]
impl Vst3Plugin for Stems {
    const VST3_CLASS_ID: [u8; 16] = *b"VanderckStemsV01";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Fx, Vst3SubCategory::Tools];
}

#[cfg(feature = "vst3")]
nih_export_vst3!(Stems);
