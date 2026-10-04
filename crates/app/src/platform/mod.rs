//! What differs between operating systems: starting with the user's session, keeping a
//! single running copy, and console output for command-line use.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::{SingleInstance, attach_console, autostart_enabled, open, open_folder, set_autostart};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use self::linux::{SingleInstance, attach_console, autostart_enabled, open, open_folder, set_autostart};

#[cfg(not(any(windows, target_os = "linux")))]
mod other;
#[cfg(not(any(windows, target_os = "linux")))]
pub use self::other::{SingleInstance, attach_console, autostart_enabled, open, open_folder, set_autostart};

/// The arguments EchoBridge starts with at sign-in.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub const AUTOSTART_ARGUMENTS: &str = "--background --autostart";

/// The name of the startup setting.
#[cfg(windows)]
pub const AUTOSTART_TITLE: &str = "Start with Windows";
#[cfg(not(windows))]
pub const AUTOSTART_TITLE: &str = "Start when you sign in";

/// Whether EchoBridge can register itself to start at sign-in on this system.
pub const AUTOSTART_SUPPORTED: bool = cfg!(any(windows, target_os = "linux"));
