//! Preferences, stored as JSON in the user's local app data and replaced atomically.
//!
//! Version 2 stores devices by id and name. Version 1 (EchoBridge 0.x, by name only) is
//! read and upgraded, so an update keeps the user's setup.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use echobridge_engine::{EchoMode, NoiseRemoval, Options};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const VERSION: u64 = 2;

/// A remembered device: matched by id, or by name when the id changed (after a driver
/// reinstall, for example).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedDevice {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub microphone: Option<SavedDevice>,
    pub playback: Option<SavedDevice>,
    /// Where the clean microphone goes; `None` only shows meters.
    pub output: Option<SavedDevice>,
    pub echo: EchoMode,
    pub noise: NoiseRemoval,
    pub delay_ms: u32,
    /// Turn protection on when EchoBridge starts; otherwise the microphone passes through.
    pub protect_on_start: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            microphone: None,
            playback: None,
            output: None,
            echo: EchoMode::Adaptive,
            noise: NoiseRemoval::Off,
            delay_ms: 0,
            protect_on_start: true,
        }
    }
}

impl Settings {
    pub fn options(&self) -> Options {
        Options { echo: self.echo, noise: self.noise, delay_ms: self.delay_ms }
    }
}

/// `%LOCALAPPDATA%\EchoBridge` (or the platform's equivalent).
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_DATA_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("EchoBridge")
}

pub fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

/// Load settings; anything unreadable gives the defaults.
pub fn load(path: &Path) -> Settings {
    let Ok(text) = fs::read_to_string(path) else { return Settings::default() };
    let Ok(value) = serde_json::from_str::<Value>(&text) else { return Settings::default() };
    match value.get("schema_version").and_then(Value::as_u64) {
        Some(VERSION) => serde_json::from_value(value).unwrap_or_default(),
        Some(1) => from_version_1(&value),
        _ => Settings::default(),
    }
}

/// Write settings next to their final path, then replace it, so a crash cannot leave a
/// half-written file.
pub fn save(path: &Path, settings: &Settings) -> io::Result<()> {
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder)?;
    }
    let mut value = serde_json::to_value(settings).map_err(io::Error::other)?;
    value["schema_version"] = VERSION.into();
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, serde_json::to_string_pretty(&value).map_err(io::Error::other)?)?;
    fs::rename(&temporary, path)
}

fn from_version_1(value: &Value) -> Settings {
    let defaults = Settings::default();
    // Version 1 knew devices by name only; the id is found again when devices are listed.
    let device = |key: &str| {
        value.get(key).and_then(Value::as_str).map(|name| SavedDevice { id: String::new(), name: name.into() })
    };
    let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default();
    Settings {
        microphone: device("microphone"),
        playback: device("playback"),
        output: device("destination"),
        echo: match text("strength") {
            "clean" => EchoMode::CleanVoice,
            "strong" => EchoMode::Strong,
            // "balanced" protected a quiet voice; Adaptive does that and more.
            _ => EchoMode::Adaptive,
        },
        noise: match text("noise_suppression") {
            "ai" => NoiseRemoval::Ai,
            "low" | "medium" | "high" => NoiseRemoval::Standard,
            _ => NoiseRemoval::Off,
        },
        delay_ms: value
            .get("delay_ms")
            .and_then(Value::as_u64)
            .filter(|&d| d <= 250)
            .map_or(defaults.delay_ms, |d| d as u32),
        protect_on_start: value.get("resume_processing").and_then(Value::as_bool).unwrap_or(defaults.protect_on_start),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("echobridge-settings-{}-{name}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        folder.join("settings.json")
    }

    #[test]
    fn round_trips() {
        let path = temporary("round-trip");
        let settings = Settings {
            microphone: Some(SavedDevice { id: "{1}".into(), name: "Mic".into() }),
            echo: EchoMode::CleanVoice,
            noise: NoiseRemoval::Ai,
            ..Settings::default()
        };
        save(&path, &settings).unwrap();
        assert_eq!(load(&path), settings);
    }

    #[test]
    fn upgrades_version_1() {
        let path = temporary("upgrade");
        fs::write(
            &path,
            r#"{"schema_version": 1, "microphone": "Microphone (High Definition Audio Device)",
                "playback": "Headphones", "destination": "CABLE Input (VB-Audio Virtual Cable)",
                "delay_ms": 0, "resume_processing": true, "strength": "clean", "noise_suppression": "ai"}"#,
        )
        .unwrap();
        let settings = load(&path);
        assert_eq!(settings.microphone.unwrap().name, "Microphone (High Definition Audio Device)");
        assert_eq!(settings.output.unwrap().name, "CABLE Input (VB-Audio Virtual Cable)");
        assert_eq!(settings.echo, EchoMode::CleanVoice);
        assert_eq!(settings.noise, NoiseRemoval::Ai);
        assert!(settings.protect_on_start);
    }

    #[test]
    fn unreadable_or_unknown_files_give_defaults() {
        let path = temporary("broken");
        fs::write(&path, "{not json").unwrap();
        assert_eq!(load(&path), Settings::default());
        fs::write(&path, r#"{"schema_version": 99}"#).unwrap();
        assert_eq!(load(&path), Settings::default());
        assert_eq!(load(&path.with_file_name("missing.json")), Settings::default());
    }
}
