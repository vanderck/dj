//! Real-time factor of the engine: `cargo run --release -p stems-core --example bench`
use std::time::Instant;

use stems_core::{FocusMode, Settings, StemEngine};

fn main() {
    let sr = 44_100.0;
    let seconds = 60;
    let block = 256;
    let mut engine = StemEngine::new(sr);
    engine.set_analysis_enabled(true);

    let mut settings = Settings::default();
    settings.stems[0].isolation = 0.5; // exercise the powf path
    settings.focus.mode = FocusMode::Isolate; // exercise the aux path

    let mut seed = 1u32;
    let mut noise = || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
    };
    let (mut l, mut r) = (vec![0.0f32; block], vec![0.0f32; block]);
    let (mut al, mut ar) = (vec![0.0f32; block], vec![0.0f32; block]);

    let blocks = seconds * sr as usize / block;
    let start = Instant::now();
    for _ in 0..blocks {
        l.iter_mut().for_each(|x| *x = noise());
        r.iter_mut().for_each(|x| *x = noise());
        engine.process(
            &mut [&mut l, &mut r],
            Some(&mut [&mut al, &mut ar]),
            &settings,
        );
    }
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "processed {seconds}s of stereo audio in {elapsed:.3}s -> {:.0}x real time ({:.2}% of one core), latency {} samples",
        seconds as f64 / elapsed,
        100.0 * elapsed / seconds as f64,
        engine.latency_samples()
    );
}
