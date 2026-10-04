//! Linux: an XDG autostart entry, and a Unix socket in the user's runtime directory that
//! keeps a single copy running and lets another launch bring the window back.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::AUTOSTART_ARGUMENTS;

/// Printing needs nothing here: the terminal that started EchoBridge already has its output.
pub fn attach_console() {}

/// Open a web address, or a file or folder, with its default app.
pub fn open(target: &str) -> io::Result<()> {
    Command::new("xdg-open").arg(target).spawn().map(drop)
}

pub fn open_folder(folder: &Path) -> io::Result<()> {
    open(&folder.to_string_lossy())
}

fn autostart_file() -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(std::env::temp_dir);
    config.join("autostart").join("echobridge.desktop")
}

pub fn autostart_enabled() -> bool {
    fs::read_to_string(autostart_file()).is_ok_and(|entry| !entry.lines().any(|line| line.trim() == "Hidden=true"))
}

pub fn set_autostart(enabled: bool) -> io::Result<()> {
    let file = autostart_file();
    if !enabled {
        return match fs::remove_file(&file) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    // An AppImage runs from a temporary mount; the file it was started from is the program.
    let program = std::env::var_os("APPIMAGE").map(PathBuf::from).map_or_else(std::env::current_exe, Ok)?;
    if let Some(folder) = file.parent() {
        fs::create_dir_all(folder)?;
    }
    fs::write(file, desktop_entry(&program))
}

/// The autostart entry that runs `program` in the background at sign-in.
fn desktop_entry(program: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=EchoBridge\nComment=Removes headphone sound that leaks into your microphone\n\
         Exec={} {AUTOSTART_ARGUMENTS}\nIcon=echobridge\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        quote_exec(&program.to_string_lossy())
    )
}

/// An argument of the `Exec` key, quoted as the Desktop Entry specification requires.
fn quote_exec(argument: &str) -> String {
    let mut quoted = String::from("\"");
    for character in argument.chars() {
        // These stay special inside quotes, and the specification escapes them with a
        // backslash, which itself needs doubling in the key's string value.
        if matches!(character, '"' | '`' | '$' | '\\') {
            quoted.push_str("\\\\");
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

/// Where a running EchoBridge listens for other launches.
fn socket_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|path| !path.is_empty()).map(PathBuf::from);
    // Without a runtime directory, a per-user folder under /tmp keeps two users on one
    // machine from sharing a socket.
    // SAFETY: no arguments, cannot fail.
    let user = unsafe { libc::getuid() };
    runtime.unwrap_or_else(|| std::env::temp_dir().join(format!("echobridge-{user}"))).join("echobridge.sock")
}

/// Ownership of the single running EchoBridge, released when dropped.
#[derive(Debug)]
pub struct SingleInstance {
    path: PathBuf,
    /// Moves to the listening thread in [`Self::on_show_request`].
    listener: Option<UnixListener>,
}

impl SingleInstance {
    /// `None` when EchoBridge already runs; `show_existing` then brings its window up.
    pub fn acquire(show_existing: bool) -> io::Result<Option<Self>> {
        Self::acquire_at(socket_path(), show_existing)
    }

    fn acquire_at(path: PathBuf, show_existing: bool) -> io::Result<Option<Self>> {
        if let Some(folder) = path.parent() {
            fs::create_dir_all(folder)?;
        }
        for _ in 0..2 {
            // A socket that answers belongs to a running copy; one that refuses is left
            // over from a copy that crashed.
            match UnixStream::connect(&path) {
                Ok(mut running) => {
                    if show_existing {
                        running.write_all(b"show\n").ok();
                    }
                    return Ok(None);
                }
                Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => {}
                Err(error) => return Err(error),
            }
            fs::remove_file(&path).ok();
            match UnixListener::bind(&path) {
                Ok(listener) => return Ok(Some(Self { path, listener: Some(listener) })),
                // Another launch bound it in between: connect to that one.
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(io::ErrorKind::AddrInUse, "another EchoBridge is starting"))
    }

    /// Call `show` whenever another launch asks for the window. The listener lives until
    /// the process exits.
    pub fn on_show_request(&mut self, show: impl Fn() + Send + 'static) {
        let Some(listener) = self.listener.take() else { return };
        let serve = move || {
            for connection in listener.incoming().flatten() {
                connection.set_read_timeout(Some(Duration::from_millis(500))).ok();
                let mut line = String::new();
                if BufReader::new(connection).read_line(&mut line).is_ok() && line.trim() == "show" {
                    show();
                }
            }
        };
        std::thread::Builder::new().name("EchoBridge show requests".into()).spawn(serve).ok();
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        fs::remove_file(&self.path).ok();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;

    use super::*;

    fn temp_socket(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("echobridge-test-{}-{name}", std::process::id())).join("echobridge.sock")
    }

    #[test]
    fn a_second_launch_is_refused_and_asks_the_first_to_show() {
        let path = temp_socket("show");
        let mut first = SingleInstance::acquire_at(path.clone(), true).unwrap().expect("the first launch owns it");
        let (sender, receiver) = channel();
        first.on_show_request(move || sender.send(()).unwrap());
        assert!(SingleInstance::acquire_at(path.clone(), true).unwrap().is_none());
        receiver.recv_timeout(Duration::from_secs(2)).expect("the window is requested");
        // A background launch (autostart) does not ask for the window.
        assert!(SingleInstance::acquire_at(path.clone(), false).unwrap().is_none());
        assert!(receiver.recv_timeout(Duration::from_millis(300)).is_err());
        drop(first);
        assert!(SingleInstance::acquire_at(path, true).unwrap().is_some(), "released on drop");
    }

    #[test]
    fn a_socket_left_by_a_crash_does_not_block_the_next_launch() {
        let path = temp_socket("stale");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        drop(UnixListener::bind(&path).unwrap()); // closing the listener leaves the file behind
        assert!(path.exists());
        assert!(SingleInstance::acquire_at(path, true).unwrap().is_some());
    }

    #[test]
    fn the_autostart_entry_runs_the_program_in_the_background() {
        let entry = desktop_entry(Path::new("/home/me/My Apps/EchoBridge"));
        assert!(entry.contains("Exec=\"/home/me/My Apps/EchoBridge\" --background --autostart\n"), "{entry}");
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert_eq!(quote_exec("/tmp/a\"b$c"), "\"/tmp/a\\\\\"b\\\\$c\"");
    }
}
