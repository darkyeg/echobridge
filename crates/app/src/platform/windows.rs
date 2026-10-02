//! Windows: the per-user Run key, a named mutex for a single instance, and a named event
//! another launch uses to bring the window back.

use std::io;
use std::path::Path;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, GetLastError, HANDLE, WAIT_OBJECT_0, WIN32_ERROR,
};
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

use super::AUTOSTART_ARGUMENTS;

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("EchoBridge");
/// Shared with EchoBridge 0.x, so the old and new app never run together.
const INSTANCE_MUTEX: PCWSTR = w!(r"Local\EchoBridge.App");
const SHOW_EVENT: PCWSTR = w!(r"Local\EchoBridge.Show");

/// Print to the console that started EchoBridge, if any; the app has no console of its own.
pub fn attach_console() {
    // SAFETY: no pointers; failure only means there is no parent console.
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS).ok() };
}

/// Open a web address, or a file or folder, with its default app.
pub fn open(target: &str) -> io::Result<()> {
    let target = HSTRING::from(target);
    // SAFETY: `target` is a NUL-terminated string alive for the call; no window is passed.
    let result = unsafe { ShellExecuteW(None, w!("open"), &target, None, None, SW_SHOWNORMAL) };
    // ShellExecute reports success with a value above 32.
    if result.0 as usize > 32 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Open a folder in the file manager.
pub fn open_folder(folder: &Path) -> io::Result<()> {
    open(&folder.to_string_lossy())
}

pub fn autostart_enabled() -> bool {
    // SAFETY: a size query with no buffer; the strings are static.
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, None) }.is_ok()
}

pub fn set_autostart(enabled: bool) -> io::Result<()> {
    if !enabled {
        // SAFETY: static strings.
        let result = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE) };
        return if result == ERROR_FILE_NOT_FOUND { Ok(()) } else { check(result) };
    }
    let command = format!("\"{}\" {AUTOSTART_ARGUMENTS}", std::env::current_exe()?.display());
    let data: Vec<u16> = command.encode_utf16().chain([0]).collect();
    let bytes = u32::try_from(data.len() * 2).map_err(io::Error::other)?;
    // SAFETY: `data` is a NUL-terminated UTF-16 string of `bytes` bytes, alive for the call.
    check(unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, REG_SZ.0, Some(data.as_ptr().cast()), bytes)
    })
}

fn check(result: WIN32_ERROR) -> io::Result<()> {
    result.ok().map_err(|error| io::Error::from_raw_os_error(error.code().0 & 0xFFFF))
}

/// Ownership of the single running EchoBridge, released when dropped.
#[derive(Debug)]
pub struct SingleInstance {
    _mutex: OwnedHandle,
    /// Moves to the watcher thread in [`Self::on_show_request`].
    show: Option<OwnedHandle>,
}

impl SingleInstance {
    /// `None` when EchoBridge already runs; `show_existing` then brings its window up.
    pub fn acquire(show_existing: bool) -> io::Result<Option<Self>> {
        // SAFETY: static name; the handle is owned below.
        let mutex = OwnedHandle(unsafe { CreateMutexW(None, false, INSTANCE_MUTEX) }?);
        // SAFETY: reads the calling thread's last error, set by CreateMutexW.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            if show_existing {
                request_show();
            }
            return Ok(None);
        }
        // SAFETY: static name; auto-reset, so each request wakes the window once.
        let show = OwnedHandle(unsafe { CreateEventW(None, false, false, SHOW_EVENT) }?);
        Ok(Some(Self { _mutex: mutex, show: Some(show) }))
    }

    /// Call `show` whenever another launch asks for the window. The watcher owns the
    /// event and lives until the process exits.
    pub fn on_show_request(&mut self, show: impl Fn() + Send + 'static) {
        let Some(event) = self.show.take() else { return };
        let watcher = move || {
            let event = event; // move the whole handle, not only its raw pointer field
            // SAFETY: `event` is owned by this thread, so it stays open while waited on.
            while unsafe { WaitForSingleObject(event.0, INFINITE) } == WAIT_OBJECT_0 {
                show();
            }
        };
        std::thread::Builder::new().name("EchoBridge show requests".into()).spawn(watcher).ok();
    }
}

fn request_show() {
    // SAFETY: static name; the handle is closed after use.
    if let Ok(event) = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, SHOW_EVENT) } {
        let event = OwnedHandle(event);
        // SAFETY: a valid event handle.
        unsafe { SetEvent(event.0) }.ok();
    }
}

#[derive(Debug)]
struct OwnedHandle(HANDLE);

// SAFETY: kernel handles are valid on every thread of the process.
unsafe impl Send for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by this process and is closed once.
        unsafe { CloseHandle(self.0) }.ok();
    }
}
