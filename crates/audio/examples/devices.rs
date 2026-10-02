//! Lists audio devices and checks capture from the default microphone and playback.
//!
//! `cargo run -p echobridge-audio --example devices`

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use echobridge_audio::{Direction, Source, system_backend};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let backend = system_backend()?;
    for direction in [Direction::Input, Direction::Output] {
        let default = backend.default_device(direction)?;
        println!("{direction:?} devices:");
        for device in backend.devices(direction)? {
            let mark = if Some(&device.id) == default.as_ref() { "*" } else { " " };
            println!(" {mark} {}", device.name);
        }
    }
    let mut streams = Vec::new();
    for (direction, source) in [(Direction::Input, Source::Microphone), (Direction::Output, Source::Loopback)] {
        let Some(device) = backend.default_device(direction)? else { continue };
        let blocks = Arc::new(AtomicUsize::new(0));
        let gain = Arc::new(std::sync::Mutex::new(1.0f32));
        let (counter, last_gain) = (blocks.clone(), gain.clone());
        let (stream, info) = backend.capture(
            &device,
            source,
            480,
            Box::new(move |block| {
                counter.fetch_add(1, Ordering::Relaxed);
                *last_gain.lock().unwrap() = block.gain;
            }),
        )?;
        streams.push((source, info, blocks, gain, stream));
    }
    std::thread::sleep(Duration::from_secs(2));
    for (source, info, blocks, gain, stream) in &streams {
        println!(
            "{source:?}: {} blocks in 2 s, {} channel(s), raw {}, gain {:.4}, failure {:?}",
            blocks.load(Ordering::Relaxed),
            info.channels,
            info.raw,
            gain.lock().unwrap(),
            stream.failure()
        );
    }
    Ok(())
}
