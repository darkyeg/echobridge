//! Measure the real delay from the microphone to the call app: run the engine into a
//! virtual cable, record the microphone and the cable's recording side together on the
//! audio clock, and find the lag between them by cross-correlation. Close EchoBridge first;
//! two writers on one cable would mix.
//!
//! cargo run --release -p echobridge-engine --example latency -- [seconds] [off|standard|ai|external]
//!
//! `external` only measures, while another program (an older EchoBridge) feeds the cable.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use echobridge_audio::{AudioBackend, CaptureBlock, Device, Direction, Source, system_backend};
use echobridge_dsp::leak::{Packet, align};
use echobridge_engine::{EchoMode, Engine, EngineConfig, NoiseRemoval, Options};

fn find(devices: &[Device], text: &str) -> Result<Device, String> {
    devices.iter().find(|d| d.name.contains(text)).cloned().ok_or_else(|| format!("no device named like {text:?}"))
}

fn recorder(packets: Arc<Mutex<Vec<Packet>>>) -> Box<dyn FnMut(CaptureBlock<'_>) + Send> {
    Box::new(move |block: CaptureBlock<'_>| {
        let channels = block.channels.max(1);
        let samples = block.samples.chunks_exact(channels).map(|frame| frame[0]).collect();
        packets.lock().unwrap().push(Packet { timestamp: block.time, samples });
    })
}

/// The lag of `delayed` behind `source`, in samples, searched up to `max_lag`.
fn lag(source: &[f32], delayed: &[f32], max_lag: usize) -> (usize, f64) {
    let length = source.len().min(delayed.len()) - max_lag;
    let energy = |x: &[f32]| x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>().sqrt();
    let reference = &source[..length];
    let mut best = (0, 0.0);
    for shift in 0..max_lag {
        let window = &delayed[shift..shift + length];
        let dot: f64 = reference.iter().zip(window).map(|(&a, &b)| f64::from(a) * f64::from(b)).sum();
        let score = dot / (energy(reference) * energy(window) + 1e-12);
        if score > best.1 {
            best = (shift, score);
        }
    }
    best
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let seconds: f64 = arguments.first().map_or(Ok(8.0), |s| s.parse())?;
    let noise = match arguments.get(1).map(String::as_str) {
        Some("ai") => NoiseRemoval::Ai,
        Some("standard") => NoiseRemoval::Standard,
        _ => NoiseRemoval::Off,
    };
    let backend: Arc<dyn AudioBackend> = system_backend()?;
    let inputs = backend.devices(Direction::Input)?;
    let outputs = backend.devices(Direction::Output)?;
    let microphone = find(&inputs, "Microphone (High Definition")?;
    let cable_out = find(&inputs, "CABLE Output")?;
    let cable_in = find(&outputs, "CABLE Input")?;
    let playback = backend.default_device(Direction::Output)?.ok_or("no playback device")?;

    let config = EngineConfig {
        microphone: microphone.id.clone(),
        playback,
        output: Some(cable_in.id.clone()),
        options: Options { echo: EchoMode::CleanVoice, noise, delay_ms: 0 },
        processing: true,
    };
    let external = arguments.get(1).is_some_and(|a| a == "external");
    let engine = if external { None } else { Some(Engine::start(backend.clone(), config)?) };
    std::thread::sleep(Duration::from_secs(1)); // let buffers settle

    let (near, far) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())));
    let (_mic, _) = backend.capture(&microphone.id, Source::Microphone, 480, recorder(near.clone()))?;
    let (_cable, _) = backend.capture(&cable_out.id, Source::Microphone, 480, recorder(far.clone()))?;
    let mut fill = Vec::new();
    let started = std::time::Instant::now();
    while started.elapsed().as_secs_f64() < seconds {
        std::thread::sleep(Duration::from_millis(7));
        if let Some(engine) = &engine {
            fill.push(engine.stats().output_buffer_ms);
        }
    }
    let stats = engine.as_ref().map(Engine::stats).unwrap_or_default();
    fill.push(0.0);
    fill.sort_by(f32::total_cmp);
    let percentile = |p: f64| fill[((fill.len() - 1) as f64 * p) as usize];
    drop(engine);

    println!("noise removal: {noise:?}");
    let recording = align(&near.lock().unwrap(), &far.lock().unwrap());
    let (shift, score) = lag(&recording.microphone, &recording.reference, 48 * 250);
    println!("microphone -> call app: {:.1} ms (correlation {score:.2})", shift as f64 / 48.0);
    println!(
        "output buffer: min {:.1} ms, median {:.1} ms, max {:.1} ms",
        percentile(0.0),
        percentile(0.5),
        percentile(1.0)
    );
    println!(
        "processing {:.2} ms/frame, reference wait {:.2} ms, underflows {}, trims {}, dropped {}, coverage {:.2}",
        stats.processing_ms,
        stats.reference_wait_ms,
        stats.output_underflows,
        stats.output_trims,
        stats.dropped_blocks,
        stats.reference_coverage
    );
    Ok(())
}
