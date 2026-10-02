//! A small log file, `echobridge.log` in the data folder, so audio problems in a call can
//! be traced afterwards. It holds messages and numbers, never audio.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use crate::leak_test::Timestamp;

/// A log larger than this is moved to `echobridge.old.log` at start.
const MAX_BYTES: u64 = 1 << 20;

struct FileLog(Mutex<File>);

impl log::Log for FileLog {
    /// EchoBridge's own messages only: libraries report per-frame details (DeepFilterNet
    /// warns on every loud hop) that would bury what matters.
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        // The app's own target is the binary name, `EchoBridge`; the libraries are `echobridge_*`.
        let ours = metadata.target().get(..10).is_some_and(|prefix| prefix.eq_ignore_ascii_case("echobridge"));
        metadata.level() <= log::Level::Info && ours
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            let line = format!("{} {:<5} {}\n", Timestamp::now().iso(), record.level(), record.args());
            self.0.lock().unwrap().write_all(line.as_bytes()).ok();
        }
    }

    fn flush(&self) {
        self.0.lock().unwrap().flush().ok();
    }
}

/// Send `log` messages to `<folder>/echobridge.log`. Failing to open it only loses the log.
pub fn init(folder: &Path) {
    let path = folder.join("echobridge.log");
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        fs::rename(&path, folder.join("echobridge.old.log")).ok();
    }
    let file = fs::create_dir_all(folder).and_then(|()| OpenOptions::new().create(true).append(true).open(&path));
    if let Ok(file) = file
        && log::set_boxed_logger(Box::new(FileLog(Mutex::new(file)))).is_ok()
    {
        log::set_max_level(log::LevelFilter::Info);
        log::info!("EchoBridge {} started", env!("CARGO_PKG_VERSION"));
    }
}
