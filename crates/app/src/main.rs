//! EchoBridge: removes headphone playback that leaks into the microphone, and sends the
//! clean microphone to call apps through a virtual cable.

// A window app: no console window opens. Command-line use attaches to the caller's console.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod app;
mod cli;
mod devices;
mod diagnostics;
mod leak_test;
mod logging;
mod platform;
mod service;
mod settings;
mod status;

use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let command = match cli::parse(&arguments) {
        Ok(command) => command,
        Err(message) => {
            platform::attach_console();
            eprintln!("{message}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    match command {
        cli::Command::Window(launch) => {
            logging::init(&settings::data_dir());
            let result = app::run(launch);
            log::logger().flush();
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    platform::attach_console();
                    eprintln!("EchoBridge could not start: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        command => {
            platform::attach_console();
            cli::run(command)
        }
    }
}
