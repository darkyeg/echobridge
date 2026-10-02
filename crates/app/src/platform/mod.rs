//! What differs between operating systems: starting with the user's session, keeping a
//! single running copy, and console output for command-line use.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::{SingleInstance, attach_console, autostart_enabled, open, open_folder, set_autostart};

#[cfg(not(windows))]
mod other;
#[cfg(not(windows))]
pub use self::other::{SingleInstance, attach_console, autostart_enabled, open, open_folder, set_autostart};

/// The arguments EchoBridge starts with at sign-in.
pub const AUTOSTART_ARGUMENTS: &str = "--background --autostart";
