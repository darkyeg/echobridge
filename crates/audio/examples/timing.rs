//! Checks the timestamps the backend puts on captured blocks: opens the default microphone
//! and the default playback loopback together and reports, for each, how steady its block
//! times are against the clock of the callbacks, and any gaps.
//!
//! `cargo run --release -p echobridge-audio --example timing -- [seconds]`
//!
//! Healthy streams show a spread of a few milliseconds or less, no discontinuities, and a
//! drift near zero. Blocks arrive in bursts of one graph period, so the spread of callback
//! times, not of block times, is what the spread measures.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use echobridge_audio::{Direction, Source, system_backend};

#[derive(Debug, Default)]
struct Series {
    first: Option<(Instant, f64)>,
    /// How much later than the first block each block's callback came, minus how much
    /// later its audio was, in seconds.
    lateness: Vec<f64>,
    blocks: u64,
    discontinuities: u64,
    out_of_order: u64,
    last_time: f64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seconds: u64 = std::env::args().nth(1).map_or(Ok(5), |s| s.parse())?;
    let backend = system_backend()?;
    let mut streams = Vec::new();
    for (direction, source) in [(Direction::Input, Source::Microphone), (Direction::Output, Source::Loopback)] {
        let Some(device) = backend.default_device(direction)? else {
            println!("no default {direction:?} device");
            continue;
        };
        let series = Arc::new(Mutex::new(Series::default()));
        let writer = series.clone();
        let (stream, info) = backend.capture(
            &device,
            source,
            480,
            Box::new(move |block| {
                let now = Instant::now();
                let mut series = writer.lock().unwrap();
                series.blocks += 1;
                series.discontinuities += u64::from(block.discontinuity);
                series.out_of_order += u64::from(series.blocks > 1 && block.time <= series.last_time);
                series.last_time = block.time;
                let (start, first_time) = *series.first.get_or_insert((now, block.time));
                let late = (now - start).as_secs_f64() - (block.time - first_time);
                series.lateness.push(late);
            }),
        )?;
        println!("{source:?}: {} channel(s)", info.channels);
        streams.push((source, series, stream));
    }
    std::thread::sleep(Duration::from_secs(seconds));
    for (source, series, stream) in &streams {
        let series = series.lock().unwrap();
        let min = series.lateness.iter().copied().fold(f64::INFINITY, f64::min);
        let max = series.lateness.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let half = series.lateness.len() / 2;
        let mean = |values: &[f64]| values.iter().sum::<f64>() / values.len().max(1) as f64;
        let drift = (mean(&series.lateness[half..]) - mean(&series.lateness[..half])) / (seconds as f64 / 2.0);
        println!(
            "{source:?}: {} blocks ({:.1}/s), spread {:.2} ms, drift {:+.3} ms/s, {} discontinuities, {} out of order, failure {:?}",
            series.blocks,
            series.blocks as f64 / seconds as f64,
            (max - min) * 1000.0,
            drift * 1000.0,
            series.discontinuities,
            series.out_of_order,
            stream.failure(),
        );
    }
    Ok(())
}
