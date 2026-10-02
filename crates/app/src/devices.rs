//! The device choices offered in Setup, and matching saved choices to them.

use echobridge_audio::{AudioBackend, Device, DeviceId, Direction, Error};

use crate::settings::SavedDevice;

/// Virtual cables that carry the clean microphone into call apps.
const CABLE_NAMES: [&str; 2] = ["cable input", "voicemeeter input"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceLists {
    pub microphones: Vec<Device>,
    /// Output devices whose playback can leak: everything but virtual cables.
    pub playback: Vec<Device>,
    /// Where the clean microphone can go.
    pub outputs: Vec<Device>,
    pub default_microphone: Option<DeviceId>,
    pub default_playback: Option<DeviceId>,
}

impl DeviceLists {
    pub fn load(backend: &dyn AudioBackend) -> Result<Self, Error> {
        let outputs = backend.devices(Direction::Output)?;
        let (outputs, playback): (Vec<_>, Vec<_>) = outputs.into_iter().partition(|d| is_cable(&d.name));
        Ok(Self {
            microphones: backend.devices(Direction::Input)?,
            playback,
            outputs,
            default_microphone: backend.default_device(Direction::Input)?,
            default_playback: backend.default_device(Direction::Output)?,
        })
    }
}

pub fn is_cable(name: &str) -> bool {
    let name = name.to_lowercase();
    CABLE_NAMES.iter().any(|cable| name.contains(cable))
}

/// The index in `devices` of the saved device: by id, or by a unique name when the id
/// changed. Without a match, the `default` device, else the first one.
pub fn pick(devices: &[Device], saved: Option<&SavedDevice>, default: Option<&DeviceId>) -> Option<usize> {
    if let Some(saved) = saved {
        if let Some(index) = devices.iter().position(|d| !saved.id.is_empty() && d.id == saved.id) {
            return Some(index);
        }
        let named: Vec<usize> = (0..devices.len()).filter(|&i| devices[i].name == saved.name).collect();
        if let [index] = named[..] {
            return Some(index);
        }
    }
    default.and_then(|id| devices.iter().position(|d| &d.id == id)).or((!devices.is_empty()).then_some(0))
}

/// Whether `saved` names a device that is connected now.
pub fn connected(devices: &[Device], saved: Option<&SavedDevice>) -> bool {
    saved.is_some_and(|saved| {
        devices.iter().any(|d| d.id == saved.id) || devices.iter().filter(|d| d.name == saved.name).count() == 1
    })
}

/// The recording device a call app should use for a cable's input: "CABLE Input (…)"
/// pairs with "CABLE Output (…)".
pub fn call_app_microphone(output: &str) -> String {
    match output.find("Input") {
        Some(index) => format!("{}Output{}", &output[..index], &output[index + "Input".len()..]),
        None => output.to_string(),
    }
}

pub fn saved(device: &Device) -> SavedDevice {
    SavedDevice { id: device.id.clone(), name: device.name.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, name: &str) -> Device {
        Device { id: id.into(), name: name.into() }
    }

    #[test]
    fn saved_devices_match_by_id_then_unique_name() {
        let devices = [device("a", "Mic"), device("b", "Headset Mic"), device("c", "Headset Mic")];
        let by_id = SavedDevice { id: "c".into(), name: "Old name".into() };
        assert_eq!(pick(&devices, Some(&by_id), None), Some(2));
        let by_name = SavedDevice { id: "gone".into(), name: "Mic".into() };
        assert_eq!(pick(&devices, Some(&by_name), None), Some(0));
        // Two devices share the name: fall back to the default.
        let ambiguous = SavedDevice { id: String::new(), name: "Headset Mic".into() };
        assert_eq!(pick(&devices, Some(&ambiguous), Some(&"b".to_string())), Some(1));
        assert_eq!(pick(&[], Some(&by_id), None), None);
    }

    #[test]
    fn cables_are_recognized_and_paired() {
        assert!(is_cable("CABLE Input (VB-Audio Virtual Cable)"));
        assert!(!is_cable("Headphones (Realtek Audio)"));
        assert_eq!(
            call_app_microphone("CABLE Input (VB-Audio Virtual Cable)"),
            "CABLE Output (VB-Audio Virtual Cable)"
        );
    }
}
