//! Talking to phones through adb.

use serde::Serialize;
use std::path::Path;
use std::process::Command;

/// Charge level from `adb shell dumpsys battery`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Battery {
    /// Percentage full, usually 0-100.
    pub level: u8,
    /// True when AC, USB or wireless power is on, or `status` is 2.
    pub charging: bool,
}

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
    /// Battery reading for a ready phone, when `dumpsys battery` succeeded.
    pub battery: Option<Battery>,
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
                battery: None,
            })
        })
        .collect()
}

/// Parse the output of `adb shell dumpsys battery`.
///
/// Returns `None` when `level` is missing or not a number. Charging is true when
/// `AC powered`, `USB powered` or `Wireless powered` is `true`, or `status` is 2.
pub fn parse_battery(output: &str) -> Option<Battery> {
    let mut level = None;
    let mut ac_powered = false;
    let mut usb_powered = false;
    let mut wireless_powered = false;
    let mut status: Option<u8> = None;

    for line in output.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            match key {
                "level" => level = value.parse().ok(),
                "AC powered" => ac_powered = value == "true",
                "USB powered" => usb_powered = value == "true",
                "Wireless powered" => wireless_powered = value == "true",
                "status" => status = value.parse().ok(),
                _ => {}
            }
        }
    }

    Some(Battery {
        level: level?,
        charging: ac_powered || usb_powered || wireless_powered || status == Some(2),
    })
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

/// Run `adb -s <serial> shell dumpsys battery` and parse it.
pub fn read_battery(adb: &Path, serial: &str) -> Result<Battery, String> {
    let out = Command::new(adb)
        .args(["-s", serial, "shell", "dumpsys", "battery"])
        .output()
        .map_err(|e| format!("could not run adb: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "adb dumpsys battery failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    parse_battery(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| "could not read battery level".to_string())
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

    const USB_CHARGING: &str = "\
Current Battery Service state:
  AC powered: false
  USB powered: true
  Wireless powered: false
  Max charging current: 500000
  Max charging voltage: 5000000
  Charge counter: 2500000
  status: 2
  health: 2
  present: true
  level: 82
  scale: 100
  voltage: 4234
  temperature: 251
  technology: Li-ion
";

    const DISCHARGING: &str = "\
Current Battery Service state:
  AC powered: false
  USB powered: false
  Wireless powered: false
  Max charging current: 0
  Max charging voltage: 0
  Charge counter: 1800000
  status: 3
  health: 2
  present: true
  level: 47
  scale: 100
  voltage: 3901
  temperature: 284
  technology: Li-ion
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

    #[test]
    fn listed_devices_start_without_battery() {
        let d = parse_devices(SAMPLE);
        assert!(d.iter().all(|device| device.battery.is_none()));
    }

    #[test]
    fn parses_usb_charging_sample() {
        assert_eq!(
            parse_battery(USB_CHARGING),
            Some(Battery {
                level: 82,
                charging: true,
            })
        );
    }

    #[test]
    fn parses_discharging_sample() {
        assert_eq!(
            parse_battery(DISCHARGING),
            Some(Battery {
                level: 47,
                charging: false,
            })
        );
    }

    #[test]
    fn ac_or_wireless_power_counts_as_charging() {
        let ac = "\
Current Battery Service state:
  AC powered: true
  USB powered: false
  Wireless powered: false
  status: 5
  level: 100
";
        assert_eq!(
            parse_battery(ac),
            Some(Battery {
                level: 100,
                charging: true,
            })
        );

        let wireless = "\
Current Battery Service state:
  AC powered: false
  USB powered: false
  Wireless powered: true
  status: 3
  level: 60
";
        assert_eq!(
            parse_battery(wireless),
            Some(Battery {
                level: 60,
                charging: true,
            })
        );
    }

    #[test]
    fn status_2_counts_as_charging_without_power_flags() {
        let out = "\
Current Battery Service state:
  status: 2
  level: 15
";
        assert_eq!(
            parse_battery(out),
            Some(Battery {
                level: 15,
                charging: true,
            })
        );
    }

    #[test]
    fn missing_fields_do_not_invent_a_level() {
        let missing_level = "\
Current Battery Service state:
  AC powered: true
  USB powered: false
  Wireless powered: false
  status: 2
  health: 2
  present: true
  scale: 100
  voltage: 4200
";
        assert_eq!(parse_battery(missing_level), None);
        assert_eq!(parse_battery(""), None);
        assert_eq!(parse_battery("not a battery dump\n"), None);

        // Level present, power and status missing: not charging.
        assert_eq!(
            parse_battery("Current Battery Service state:\n  level: 33\n"),
            Some(Battery {
                level: 33,
                charging: false,
            })
        );
    }
}
