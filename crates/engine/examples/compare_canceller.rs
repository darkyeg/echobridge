//! Record the microphone and the stereo headphone playback on one sample grid, run the
//! Clean voice canceller on it offline, and write the recording as raw f32 files so another
//! implementation can process the same audio.
//!
//! cargo run --release -p echobridge-engine --example compare_canceller -- <folder> [seconds]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use echobridge_audio::{AudioBackend, CaptureBlock, Device, Direction, Source, system_backend};
use echobridge_dsp::leak::{Packet, align};
use echobridge_dsp::level::dbfs;
use echobridge_dsp::linear::{LinearCanceller, LinearConfig};

type Packets = Arc<Mutex<Vec<Packet>>>;

fn find(devices: &[Device], text: &str) -> Result<Device, String> {
    devices.iter().find(|d| d.name.contains(text)).cloned().ok_or_else(|| format!("no device named like {text:?}"))
}

/// Records channel `channel` (or the mean of all channels) scaled by the block gain.
fn recorder(packets: Packets, channel: Option<usize>) -> Box<dyn FnMut(CaptureBlock<'_>) + Send> {
    Box::new(move |block: CaptureBlock<'_>| {
        let channels = block.channels.max(1);
        let samples = block
            .samples
            .chunks_exact(channels)
            .map(|frame| match channel {
                Some(c) => frame[c.min(channels - 1)] * block.gain,
                None => frame.iter().sum::<f32>() / channels as f32,
            })
            .collect();
        packets.lock().unwrap().push(Packet { timestamp: block.time, samples });
    })
}

fn write(path: &std::path::Path, samples: &[f32]) -> std::io::Result<()> {
    std::fs::write(path, samples.iter().flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>())
}

/// Removal in dB per second: microphone level minus output level.
fn removal(near: &[f32], out: &[f32]) -> Vec<f32> {
    near.chunks(48_000).zip(out.chunks(48_000)).map(|(n, o)| dbfs(n) - dbfs(o)).collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let folder = std::path::PathBuf::from(arguments.first().ok_or("give an output folder")?);
    let seconds: u64 = arguments.get(1).map_or(Ok(30), |s| s.parse())?;
    std::fs::create_dir_all(&folder)?;
    let backend: Arc<dyn AudioBackend> = system_backend()?;
    let microphone = find(&backend.devices(Direction::Input)?, "Microphone (High Definition")?;
    let playback = backend.default_device(Direction::Output)?.ok_or("no playback device")?;

    println!("waiting for music on the headphones...");
    loop {
        let probe = Packets::default();
        {
            let (_probe, _) = backend.capture(&playback, Source::Loopback, 480, recorder(probe.clone(), Some(0)))?;
            std::thread::sleep(Duration::from_millis(500));
        }
        let samples: Vec<f32> = probe.lock().unwrap().iter().flat_map(|p| p.samples.clone()).collect();
        if dbfs(&samples) > -50.0 {
            break;
        }
    }
    let (mic, left, right) = (Packets::default(), Packets::default(), Packets::default());
    let (mic0, mic1) = (Packets::default(), Packets::default());
    {
        let (_l, _) = backend.capture(&playback, Source::Loopback, 480, recorder(left.clone(), Some(0)))?;
        let (_r, _) = backend.capture(&playback, Source::Loopback, 480, recorder(right.clone(), Some(1)))?;
        let (_m, _) = backend.capture(&microphone.id, Source::Microphone, 480, recorder(mic.clone(), None))?;
        let (_m0, _) = backend.capture(&microphone.id, Source::Microphone, 480, recorder(mic0.clone(), Some(0)))?;
        let (_m1, _) = backend.capture(&microphone.id, Source::Microphone, 480, recorder(mic1.clone(), Some(1)))?;
        std::thread::sleep(Duration::from_secs(seconds));
    }
    let mic = mic.lock().unwrap();
    let l = align(&mic, &left.lock().unwrap());
    let r = align(&mic, &right.lock().unwrap());
    let frames = (l.microphone.len().min(r.reference.len()) / 240) * 240;
    let near = &l.microphone[..frames];
    let far: Vec<f32> = l.reference[..frames].iter().zip(&r.reference[..frames]).flat_map(|(&a, &b)| [a, b]).collect();
    println!("recorded {:.1} s, coverage {:.3}/{:.3}", frames as f64 / 48_000.0, l.coverage, r.coverage);
    println!("microphone {:.1} dBFS, playback {:.1} dBFS", dbfs(near), dbfs(&l.reference[..frames]));

    let mut canceller = LinearCanceller::new(LinearConfig::default());
    let mut out = vec![0.0; frames];
    canceller.process(near, &far, &mut out)?;
    let per_second = removal(near, &out);
    println!("rust removal per second: {}", per_second.iter().map(|r| format!("{r:.1}")).collect::<Vec<_>>().join(" "));

    write(&folder.join("near.f32"), near)?;
    write(&folder.join("far.f32"), &far)?;
    write(&folder.join("rust_out.f32"), &out)?;
    for (name, packets) in [("mic0.f32", &mic0), ("mic1.f32", &mic1)] {
        let aligned = align(&mic, &packets.lock().unwrap());
        write(&folder.join(name), &aligned.reference[..frames.min(aligned.reference.len())])?;
    }
    Ok(())
}
