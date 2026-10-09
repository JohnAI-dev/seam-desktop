//! Talking to phones through adb.

use serde::Serialize;
use std::path::Path;
use std::process::Command;

/// A phone (or emulator) adb can see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Device {
    /// adb serial, e.g. `R5CW12345` or `192.168.1.20:5555`.
    pub serial: String,
    /// adb state: `device` (ready), `unauthorized`, `offline`, ...
    pub state: String,
    /// Human-readable model, e.g. `SM S928B`, when adb reports it.
    pub model: Option<String>,
    /// Whether the phone is connected over Wi-Fi rather than USB.
    pub wireless: bool,
}

impl Device {
    /// The phone is connected and has accepted this computer.
    pub fn is_ready(&self) -> bool {
        self.state == "device"
    }

    /// Best name to show a person.
    pub fn display_name(&self) -> &str {
        self.model.as_deref().unwrap_or(&self.serial)
    }
}

/// Parse the output of `adb devices -l`.
pub fn parse_devices(output: &str) -> Vec<Device> {
    output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('*') && !l.starts_with("List of devices"))
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let serial = parts.next()?.to_string();
            let state = parts.next()?.to_string();
            let model = parts
                .find_map(|kv| kv.strip_prefix("model:"))
                .map(|m| m.replace('_', " "));
            let wireless = serial.contains(':') || serial.starts_with("adb-");
            Some(Device {
                serial,
                state,
                model,
                wireless,
            })
        })
        .collect()
}

/// Ask adb for connected devices.
pub fn list_devices(adb: &Path) -> Result<Vec<Device>, String> {
    let out = Command::new(adb)
        .args(["devices", "-l"])
        .output()
        .map_err(|e| format!("could not run adb: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "adb devices failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(parse_devices(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
* daemon not running; starting now at tcp:5037
* daemon started successfully
List of devices attached
R5CW12345ABC           device usb:1-1 product:e3qxeea model:SM_S928B device:e3q transport_id:1
192.168.1.20:5555      device product:e3qxeea model:SM_S928B device:e3q transport_id:2
emulator-5554          offline transport_id:3
ZY22ABCD               unauthorized usb:2-1 transport_id:4

";

    #[test]
    fn parses_all_devices() {
        let d = parse_devices(SAMPLE);
        assert_eq!(d.len(), 4);
        assert_eq!(d[0].serial, "R5CW12345ABC");
        assert_eq!(d[0].model.as_deref(), Some("SM S928B"));
        assert!(d[0].is_ready());
        assert!(!d[0].wireless);
    }

    #[test]
    fn detects_wireless_and_states() {
        let d = parse_devices(SAMPLE);
        assert!(d[1].wireless);
        assert_eq!(d[2].state, "offline");
        assert!(!d[2].is_ready());
        assert_eq!(d[3].state, "unauthorized");
        assert_eq!(d[3].display_name(), "ZY22ABCD");
    }

    #[test]
    fn empty_output_means_no_devices() {
        assert!(parse_devices("List of devices attached\n\n").is_empty());
        assert!(parse_devices("").is_empty());
    }
}
