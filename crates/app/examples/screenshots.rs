//! Render every page of the window to PNG files, offscreen, with sample data. No audio
//! device is opened. Used to review the design and catch layout regressions.
//!
//! cargo run -p echobridge --example screenshots -- <folder>

use std::path::PathBuf;
use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{ComponentHandle, ModelRc, PhysicalSize, PlatformError, SharedString, VecModel};

/// The code Slint generates from `ui/`.
#[allow(missing_debug_implementations, clippy::all)]
mod ui {
    slint::include_modules!();
}

use ui::{AppWindow, LeakTest, Live, Page, Setup, Tone};

const WIDTH: u32 = 1000;
const HEIGHT: u32 = 760;

struct Offscreen(Rc<MinimalSoftwareWindow>);

impl Platform for Offscreen {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

fn strings(items: &[&str]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(items.iter().map(|&s| SharedString::from(s)).collect::<Vec<_>>()))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let folder = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "screenshots".into()));
    std::fs::create_dir_all(&folder)?;
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Offscreen(window.clone()))).map_err(|e| format!("{e:?}"))?;
    let ui = AppWindow::new()?;
    window.set_size(PhysicalSize::new(WIDTH, HEIGHT));
    ui.show()?;

    let live = ui.global::<Live>();
    live.set_tone(Tone::Good);
    live.set_headline("Protecting".into());
    live.set_detail(
        "Leaked playback is removed. In your call app, choose CABLE Output (VB-Audio Virtual Cable).".into(),
    );
    live.set_running(true);
    live.set_protecting(true);
    live.set_can_start(true);
    live.set_microphone("Microphone (High Definition Audio Device)".into());
    live.set_playback("Headphones (Realtek(R) Audio)".into());
    live.set_output("CABLE Input (VB-Audio Virtual Cable)".into());
    live.set_microphone_level(-31.0);
    live.set_playback_level(-18.0);
    live.set_output_level(-62.0);

    let setup = ui.global::<Setup>();
    setup.set_microphones(strings(&["Microphone (High Definition Audio Device)"]));
    setup.set_playback_devices(strings(&["Headphones (Realtek(R) Audio)", "Speakers (Steam Streaming Speakers)"]));
    setup.set_outputs(strings(&["CABLE Input (VB-Audio Virtual Cable)", "Nothing (meters only)"]));
    setup.set_call_app_microphone("CABLE Output (VB-Audio Virtual Cable)".into());
    setup.set_version("1.0.0".into());
    setup.set_autostart(true);

    let leak = ui.global::<LeakTest>();
    leak.set_microphone("Microphone (High Definition Audio Device)".into());
    leak.set_playback("Headphones (Realtek(R) Audio)".into());
    leak.set_worn_done(true);
    leak.set_worn_result("Leak -63.2 dBFS, -5.1 dB relative to playback, 98% of the microphone signal. Spread across frequencies: 1.8 dB.".into());
    leak.set_covered_result("Leak -63.6 dBFS, -5.4 dB relative to playback, 97% of the microphone signal.".into());
    leak.set_has_verdict(true);
    leak.set_verdict_tone(Tone::Warning);
    leak.set_verdict_title("The leak is electrical, not acoustic".into());
    leak.set_verdict_detail("Covering the earcups did not reduce the leak and it is flat across frequencies. Choose Clean voice, which removes such a leak without altering your voice.\n\nCovering the earcups changed the leak by 0.3 dB.".into());

    let shots = [(Page::Live, "live"), (Page::Setup, "setup"), (Page::LeakTest, "leak-test")];
    for (page, name) in shots {
        ui.set_page(page);
        // Let animations finish before rendering.
        for _ in 0..30 {
            slint::platform::update_timers_and_animations();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        window.request_redraw();
        let mut pixels = vec![PremultipliedRgbaColor::default(); (WIDTH * HEIGHT) as usize];
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, WIDTH as usize);
        });
        let bytes: Vec<u8> = pixels.iter().flat_map(|p| [p.red, p.green, p.blue]).collect();
        let path = folder.join(format!("{name}.png"));
        image::save_buffer(&path, &bytes, WIDTH, HEIGHT, image::ColorType::Rgb8)?;
        println!("{}", path.display());
    }
    Ok(())
}
