//! Measure how much of the song the running EchoBridge removes: wait until music plays,
//! then record the microphone, the headphone playback and CABLE Output (what the call app
//! hears) side by side and print their levels each second. Nothing is saved.
//!
//! cargo run --release -p echobridge-engine --example removal -- [seconds] [wait-seconds] [off|ai]
//!
//! With `off` or `ai`, the tool runs its own engine (Clean voice) into CABLE Input instead
//! of measuring a running EchoBridge, and prints the engine's state each second.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use echobridge_audio::{AudioBackend, CaptureBlock, Device, Direction, Source, system_backend};
use echobridge_dsp::level::dbfs;
use echobridge_engine::{EchoMode, Engine, EngineConfig, NoiseRemoval, Options};

type Samples = Arc<Mutex<Vec<f32>>>;

fn find(devices: &[Device], text: &str) -> Result<Device, String> {
    devices.iter().find(|d| d.name.contains(text)).cloned().ok_or_else(|| format!("no device named like {text:?}"))
}

fn recorder(samples: Samples) -> Box<dyn FnMut(CaptureBlock<'_>) + Send> {
    Box::new(move |block: CaptureBlock<'_>| {
        let channels = block.channels.max(1);
        let gain = if block.gain > 0.0 { block.gain } else { 1.0 };
        let mut samples = samples.lock().unwrap();
        samples.extend(block.samples.chunks_exact(channels).map(|frame| frame[0] * gain));
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let seconds: usize = arguments.first().map_or(Ok(10), |s| s.parse())?;
    let wait: u64 = arguments.get(1).map_or(Ok(600), |s| s.parse())?;
    let backend: Arc<dyn AudioBackend> = system_backend()?;
    let inputs = backend.devices(Direction::Input)?;
    let microphone = find(&inputs, "Microphone (High Definition")?;
    let cable = find(&inputs, "CABLE Output")?;
    let playback = backend.default_device(Direction::Output)?.ok_or("no playback device")?;

    let streams = [Samples::default(), Samples::default(), Samples::default()];
    let [mic, music, call] = streams.clone();
    let (_music, _) = backend.capture(&playback, Source::Loopback, 480, recorder(music))?;
    println!("waiting for music on the headphones...");
    let deadline = Instant::now() + Duration::from_secs(wait);
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let level = {
            let mut music = streams[1].lock().unwrap();
            let level = dbfs(&music);
            music.clear();
            level
        };
        if level > -50.0 {
            break;
        }
        if Instant::now() > deadline {
            return Err("no music played".into());
        }
    }
    let engine = match arguments.get(2).map(String::as_str) {
        Some(mode) => {
            let noise = if mode == "ai" { NoiseRemoval::Ai } else { NoiseRemoval::Off };
            let cable_in = find(&backend.devices(Direction::Output)?, "CABLE Input")?;
            let config = EngineConfig {
                microphone: microphone.id.clone(),
                playback: playback.clone(),
                output: Some(cable_in.id),
                options: Options { echo: EchoMode::CleanVoice, noise, delay_ms: 0 },
                processing: true,
            };
            let engine = Engine::start(backend.clone(), config)?;
            println!("own engine, noise {noise:?}; settling 15 s");
            std::thread::sleep(Duration::from_secs(15));
            Some(engine)
        }
        None => None,
    };
    let (_mic, _) = backend.capture(&microphone.id, Source::Microphone, 480, recorder(mic))?;
    let (_call, _) = backend.capture(&cable.id, Source::Microphone, 480, recorder(call))?;
    streams[1].lock().unwrap().clear();
    let mut states = Vec::new();
    for _ in 0..seconds {
        std::thread::sleep(Duration::from_secs(1));
        if let Some(engine) = &engine {
            let s = engine.stats();
            states.push(format!(
                "shift {:.1} ms coverage {:.2} ai_overloaded {} dropped {} underflows {} frames {} processing {:.2} ms",
                s.reference_shift_ms,
                s.reference_coverage,
                s.ai_overloaded,
                s.dropped_blocks,
                s.output_underflows,
                s.frames,
                s.processing_ms
            ));
        }
    }
    for (second, state) in states.iter().enumerate() {
        println!("engine {second:>2}: {state}");
    }

    let [mic, music, call] = streams.map(|s| s.lock().unwrap().clone());
    let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
    println!("second  playback  microphone (peak)  call app hears  removed");
    let mut removed = Vec::new();
    for second in 0..seconds {
        let range = |x: &[f32]| x[(second * 48_000).min(x.len())..((second + 1) * 48_000).min(x.len())].to_vec();
        let (m, p, c) = (range(&mic), range(&music), range(&call));
        let gone = dbfs(&m) - dbfs(&c);
        removed.push(gone);
        println!(
            "{second:>6}  {:>8.1}  {:>10.1} ({:>5.2})  {:>14.1}  {gone:>7.1}",
            dbfs(&p),
            dbfs(&m),
            peak(&m),
            dbfs(&c)
        );
    }
    removed.sort_by(f32::total_cmp);
    println!("median removed: {:.1} dB (higher is better; while you talk it is lower)", removed[removed.len() / 2]);
    Ok(())
}
