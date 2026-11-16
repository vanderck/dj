//! Audio output: drives the engine from the device callback and maps the
//! master, cue and direct-output buses onto the device's stereo pairs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use collective_engine::engine::MAX_PAIRS;
use collective_engine::{create, Controller, Engine, EngineConfig};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};

use crate::settings::AudioSettings;

pub struct AudioHost {
    _stream: Option<cpal::Stream>,
    stop: Arc<AtomicBool>,
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: usize,
    pub buffer: Option<u32>,
}

impl Drop for AudioHost {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub fn device_names() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|it| it.filter_map(|d| d.description().ok().map(|x| x.name().to_string())).collect())
        .unwrap_or_default()
}

pub const MAX_BLOCK: usize = 1024;

struct Router {
    engine: Engine,
    channels: usize,
    master_pair: usize,
    cue_pair: Option<usize>,
    scratch: Vec<f32>,
}

impl Router {
    fn new(engine: Engine, channels: usize, master_pair: usize, cue_pair: Option<usize>) -> Self {
        Self { engine, channels, master_pair, cue_pair, scratch: vec![0.0; MAX_BLOCK * channels.max(2)] }
    }

    fn render<T: SizedSample + FromSample<f32>>(&mut self, data: &mut [T]) {
        let ch = self.channels.max(1);
        let frames = data.len() / ch;
        let pairs = ch / 2;
        let mut off = 0;
        while off < frames {
            let n = (frames - off).min(MAX_BLOCK);
            self.engine.process(n);
            let sc = &mut self.scratch[..n * ch];
            sc.fill(0.0);
            // Buses may share a pair (e.g. cue on the master pair): they sum.
            let mut add = |pair: usize, buf: &[[f32; 2]]| {
                if ch == 1 {
                    for (i, f) in buf.iter().enumerate() {
                        sc[i] += (f[0] + f[1]) * 0.5;
                    }
                } else if pair < pairs {
                    for (i, f) in buf.iter().enumerate() {
                        sc[i * ch + pair * 2] += f[0];
                        sc[i * ch + pair * 2 + 1] += f[1];
                    }
                }
            };
            add(self.master_pair, self.engine.master(n));
            if let Some(c) = self.cue_pair {
                add(c, self.engine.cue(n));
            }
            for p in 0..MAX_PAIRS {
                if let Some(b) = self.engine.pair(p, n) {
                    add(p, b);
                }
            }
            for (o, s) in data[off * ch..(off + n) * ch].iter_mut().zip(sc.iter()) {
                *o = T::from_sample(*s);
            }
            off += n;
        }
    }
}

/// Start audio. Falls back to a timer-driven "null" device when no output
/// device exists (headless servers, CI), so OSC/MIDI control still works.
pub fn start(settings: &AudioSettings) -> Result<(AudioHost, Controller, f32)> {
    match start_device(settings) {
        Ok(x) => Ok(x),
        Err(e) => {
            eprintln!("audio: {e}; running without an audio device");
            start_null(settings.sample_rate.unwrap_or(48000))
        }
    }
}

fn start_device(settings: &AudioSettings) -> Result<(AudioHost, Controller, f32)> {
    let host = cpal::default_host();
    let device = match &settings.device {
        Some(name) => host
            .output_devices()?
            .find(|d| d.description().map(|x| x.name() == name).unwrap_or(false))
            .or_else(|| host.default_output_device()),
        None => host.default_output_device(),
    }
    .ok_or_else(|| anyhow!("no output device"))?;
    let name = device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "Output".into());
    let default = device.default_output_config()?;
    let mut sr = default.sample_rate();
    if let Some(want) = settings.sample_rate {
        if device
            .supported_output_configs()?
            .any(|c| c.min_sample_rate() <= want && c.max_sample_rate() >= want && c.channels() == default.channels())
        {
            sr = want;
        }
    }
    // Use every output the device offers (e.g. both stereo pairs of a Xone:K2 or any multi-output
    // interface), not only the default layout, so decks can go to separate outputs.
    let supported = |f: cpal::SampleFormat| {
        matches!(f, cpal::SampleFormat::F32 | cpal::SampleFormat::I16 | cpal::SampleFormat::I32 | cpal::SampleFormat::U16)
    };
    let chosen = device
        .supported_output_configs()?
        .filter(|c| c.min_sample_rate() <= sr && c.max_sample_rate() >= sr && supported(c.sample_format()))
        .max_by_key(|c| (c.channels(), c.sample_format() == default.sample_format(), c.sample_format() == cpal::SampleFormat::F32))
        .map(|c| c.with_sample_rate(sr))
        .filter(|c| c.channels() > default.channels())
        .unwrap_or(default);
    let channels = chosen.channels() as usize;
    let mut config = chosen.config();
    config.sample_rate = sr;
    if let Some(b) = settings.buffer_frames {
        config.buffer_size = cpal::BufferSize::Fixed(b);
    }
    let (engine, ctl) = create(EngineConfig { sample_rate: sr as f32, max_block: MAX_BLOCK, max_latency: 16384 });
    let pairs = (channels / 2).max(1);
    let master_pair = (settings.master_pair as usize).min(pairs - 1);
    let cue_pair = settings.cue_enabled.then_some((settings.cue_pair as usize).min(pairs - 1));
    let router = Router::new(engine, channels, master_pair, cue_pair);
    let stream = match chosen.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(&device, config, router),
        cpal::SampleFormat::I16 => build::<i16>(&device, config, router),
        cpal::SampleFormat::I32 => build::<i32>(&device, config, router),
        cpal::SampleFormat::U16 => build::<u16>(&device, config, router),
        f => return Err(anyhow!("unsupported sample format {f:?}")),
    };
    let stream = match stream {
        Ok(s) => s,
        Err(e) if settings.buffer_frames.is_some() => {
            // Retry with the driver's default buffer size.
            let (engine, ctl2) = create(EngineConfig { sample_rate: sr as f32, max_block: MAX_BLOCK, max_latency: 16384 });
            let router = Router::new(engine, channels, master_pair, cue_pair);
            let mut c = config;
            c.buffer_size = cpal::BufferSize::Default;
            eprintln!("audio: fixed buffer rejected ({e}); using the default");
            let s = build::<f32>(&device, c, router)?;
            s.play()?;
            return Ok((
                AudioHost { _stream: Some(s), stop: Arc::new(AtomicBool::new(false)), device_name: name, sample_rate: sr, channels, buffer: None },
                ctl2,
                sr as f32,
            ));
        }
        Err(e) => return Err(e),
    };
    stream.play()?;
    Ok((
        AudioHost {
            _stream: Some(stream),
            stop: Arc::new(AtomicBool::new(false)),
            device_name: name,
            sample_rate: sr,
            channels,
            buffer: settings.buffer_frames,
        },
        ctl,
        sr as f32,
    ))
}

fn build<T: SizedSample + FromSample<f32> + Send + 'static>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut router: Router,
) -> Result<cpal::Stream> {
    let s = device.build_output_stream(
        config,
        move |data: &mut [T], _| router.render(data),
        |e| eprintln!("audio stream error: {e}"),
        None,
    )?;
    Ok(s)
}

fn start_null(sr: u32) -> Result<(AudioHost, Controller, f32)> {
    let (mut engine, ctl) = create(EngineConfig { sample_rate: sr as f32, max_block: MAX_BLOCK, max_latency: 16384 });
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    std::thread::Builder::new().name("null-audio".into()).spawn(move || {
        let block = 256usize;
        let period = Duration::from_secs_f64(block as f64 / sr as f64);
        let mut next = Instant::now();
        while !stop2.load(Ordering::Relaxed) {
            engine.process(block);
            next += period;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else if now - next > Duration::from_millis(200) {
                next = now;
            }
        }
    })?;
    Ok((
        AudioHost { _stream: None, stop, device_name: "No audio device".into(), sample_rate: sr, channels: 2, buffer: None },
        ctl,
        sr as f32,
    ))
}
