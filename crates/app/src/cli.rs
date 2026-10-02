//! Command-line use: diagnostics that print JSON and never save audio.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use echobridge_audio::{AudioBackend, Direction, system_backend};
use echobridge_dsp::leak::analyze_leak;
use echobridge_engine::record_leak;
use serde_json::json;

use crate::devices::{DeviceLists, pick};
use crate::settings;

pub const USAGE: &str = "Usage: EchoBridge [--background] [--autostart]
       EchoBridge --list-devices
       EchoBridge --leak-test SECONDS [--report FILE]";

/// How the window starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Launch {
    /// Start in the notification area without showing the window.
    pub background: bool,
    /// Started at sign-in.
    pub autostart: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Window(Launch),
    ListDevices,
    LeakTest { seconds: f64, report: Option<PathBuf> },
}

pub fn parse(arguments: &[String]) -> Result<Command, String> {
    let mut launch = Launch::default();
    let mut leak_seconds = None;
    let mut report = None;
    let mut list = false;
    let mut iter = arguments.iter();
    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "--background" => launch.background = true,
            "--autostart" => launch.autostart = true,
            "--list-devices" => list = true,
            "--leak-test" => {
                let value = iter.next().ok_or("--leak-test needs a duration in seconds")?;
                let seconds: f64 = value.parse().map_err(|_| format!("not a number of seconds: {value}"))?;
                if !(2.0..=60.0).contains(&seconds) {
                    return Err("the leak test lasts between 2 and 60 seconds".into());
                }
                leak_seconds = Some(seconds);
            }
            "--report" => report = Some(PathBuf::from(iter.next().ok_or("--report needs a file name")?)),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(match (list, leak_seconds) {
        (true, None) => Command::ListDevices,
        (false, Some(seconds)) => Command::LeakTest { seconds, report },
        (false, None) if report.is_none() => Command::Window(launch),
        _ => return Err("choose one command".into()),
    })
}

pub fn run(command: Command) -> ExitCode {
    let backend = match system_backend() {
        Ok(backend) => backend,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let result = match command {
        Command::ListDevices => list_devices(backend.as_ref()),
        Command::LeakTest { seconds, report } => leak_test(backend.as_ref(), seconds, report),
        Command::Window(_) => unreachable!("the window is not a command-line command"),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

type CliResult = Result<(), Box<dyn std::error::Error>>;

fn list_devices(backend: &dyn AudioBackend) -> CliResult {
    let list = |direction| -> Result<_, echobridge_audio::Error> {
        let default = backend.default_device(direction)?;
        Ok(backend
            .devices(direction)?
            .into_iter()
            .map(|d| json!({ "id": d.id, "name": d.name, "default": Some(&d.id) == default.as_ref() }))
            .collect::<Vec<_>>())
    };
    let devices = json!({ "inputs": list(Direction::Input)?, "outputs": list(Direction::Output)? });
    println!("{}", serde_json::to_string_pretty(&devices)?);
    Ok(())
}

/// Measure the leak on the saved microphone and headphones.
fn leak_test(backend: &dyn AudioBackend, seconds: f64, report: Option<PathBuf>) -> CliResult {
    let settings = settings::load(&settings::settings_path());
    let lists = DeviceLists::load(backend)?;
    let microphone = pick(&lists.microphones, settings.microphone.as_ref(), lists.default_microphone.as_ref())
        .map(|i| &lists.microphones[i])
        .ok_or("no microphone is connected")?;
    let playback = pick(&lists.playback, settings.playback.as_ref(), lists.default_playback.as_ref())
        .map(|i| &lists.playback[i])
        .ok_or("no playback device is connected")?;
    eprintln!("Recording {seconds} s from {} while {} plays. Stay silent.", microphone.name, playback.name);
    let recording = record_leak(
        backend,
        &microphone.id,
        &playback.id,
        Duration::from_secs_f64(seconds),
        &AtomicBool::new(false),
        &mut |_| {},
    )?;
    let measurement = analyze_leak(&recording.microphone, &recording.reference, recording.coverage)?;
    let text = serde_json::to_string_pretty(&json!({
        "kind": "leak-measurement",
        "devices": { "microphone": microphone.name, "playback": playback.name },
        "measurement": measurement,
        "detected": measurement.detected(),
        "warnings": measurement.warnings().iter().map(|w| w.message()).collect::<Vec<_>>(),
        "audio_saved": false,
    }))?;
    println!("{text}");
    if let Some(path) = report {
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder)?;
        }
        std::fs::write(path, text)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_words(words: &str) -> Result<Command, String> {
        parse(&words.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    #[test]
    fn parses_launch_flags_and_commands() {
        assert_eq!(parse_words(""), Ok(Command::Window(Launch::default())));
        assert_eq!(
            parse_words("--background --autostart"),
            Ok(Command::Window(Launch { background: true, autostart: true }))
        );
        assert_eq!(parse_words("--list-devices"), Ok(Command::ListDevices));
        assert_eq!(
            parse_words("--leak-test 8 --report out.json"),
            Ok(Command::LeakTest { seconds: 8.0, report: Some("out.json".into()) })
        );
    }

    #[test]
    fn rejects_bad_arguments() {
        assert!(parse_words("--leak-test").is_err());
        assert!(parse_words("--leak-test 1").is_err());
        assert!(parse_words("--leak-test soon").is_err());
        assert!(parse_words("--list-devices --leak-test 5").is_err());
        assert!(parse_words("--report x.json").is_err());
        assert!(parse_words("--loud").is_err());
    }
}
