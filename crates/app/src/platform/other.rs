//! Fallbacks for systems without native support yet.

use std::io;
use std::path::Path;
use std::process::Command;

/// Open a web address, or a file or folder, with its default app.
pub fn open(target: &str) -> io::Result<()> {
    Command::new("xdg-open").arg(target).spawn().map(drop)
}

pub fn open_folder(folder: &Path) -> io::Result<()> {
    open(&folder.to_string_lossy())
}

pub fn attach_console() {}

pub fn autostart_enabled() -> bool {
    false
}

pub fn set_autostart(_enabled: bool) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "starting with the system is not supported here yet"))
}

#[derive(Debug)]
pub struct SingleInstance;

impl SingleInstance {
    /// Always the only instance.
    pub fn acquire(_show_existing: bool) -> io::Result<Option<Self>> {
        Ok(Some(Self))
    }

    pub fn on_show_request(&mut self, _show: impl Fn() + Send + 'static) {}
}
