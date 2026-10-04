//! Plays clicks into EchoBridge's virtual microphone and records them back from it, as a
//! call app would, to check that the virtual microphone works and to estimate its delay.
//!
//! `cargo run --release -p echobridge-audio --example virtual_mic`
//!
//! Linux only: other systems use a virtual cable that EchoBridge does not create.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use echobridge_audio::{Direction, Latency, RATE, Source, system_backend};

const NAME: &str = "EchoBridge Microphone";
/// Samples between clicks: half a second.
const PERIOD: usize = RATE as usize / 2;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let backend = system_backend()?;
    let output = backend
        .devices(Direction::Output)?
        .into_iter()
        .find(|device| device.name == NAME)
        .ok_or("this system has no virtual microphone")?;

    let sent = Arc::new(Mutex::new(Vec::<Instant>::new()));
    let sent_writer = sent.clone();
    let mut position = 0usize;
    let render = backend.render(
        &output.id,
        Latency::Low,
        Box::new(move |out| {
            let now = Instant::now();
            for (index, sample) in out.iter_mut().enumerate() {
                let phase = (position + index) % PERIOD;
                *sample = if phase < 48 { 0.5 } else { 0.0 };
                if phase == 0 {
                    // The click leaves the callback after the samples before it in this buffer.
                    sent_writer.lock().unwrap().push(now + Duration::from_secs_f64(index as f64 / f64::from(RATE)));
                }
            }
            position += out.len();
        }),
    )?;

    // The microphone appears among the inputs once something is rendering into it.
    std::thread::sleep(Duration::from_millis(500));
    let input = backend
        .devices(Direction::Input)?
        .into_iter()
        .find(|device| device.name == NAME)
        .ok_or("the virtual microphone is not listed as a recording device")?;
    let heard = Arc::new(Mutex::new(Vec::<Instant>::new()));
    let (heard_writer, mut quiet) = (heard.clone(), true);
    let (capture, info) = backend.capture(
        &input.id,
        Source::Microphone,
        480,
        Box::new(move |block| {
            let now = Instant::now();
            let loud = block.samples.iter().any(|sample| sample.abs() > 0.1);
            if loud && quiet {
                heard_writer.lock().unwrap().push(now);
            }
            quiet = !loud;
        }),
    )?;
    println!("recording {} channel(s) from {:?}", info.channels, input.name);
    std::thread::sleep(Duration::from_secs(5));
    println!("render failure: {:?}, capture failure: {:?}", render.failure(), capture.failure());

    let (sent, heard) = (sent.lock().unwrap().clone(), heard.lock().unwrap().clone());
    let delays: Vec<f64> = heard
        .iter()
        .filter_map(|&heard| sent.iter().rev().find(|&&sent| sent <= heard).map(|&sent| (heard - sent).as_secs_f64()))
        .collect();
    println!("{} clicks sent, {} heard", sent.len(), heard.len());
    if delays.is_empty() {
        return Err("nothing came through the virtual microphone".into());
    }
    let mean = delays.iter().sum::<f64>() / delays.len() as f64;
    println!("delay through the virtual microphone: {:.1} ms on average (callback to callback)", mean * 1000.0);
    Ok(())
}
