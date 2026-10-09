//! Mirroring a phone's screen with scrcpy.

use std::path::Path;
use std::process::{Child, Command, Stdio};

/// Options for a mirroring window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorOptions {
    /// Keep the phone awake while mirroring.
    pub stay_awake: bool,
    /// Turn the phone's own screen off while mirroring (saves battery).
    pub phone_screen_off: bool,
}

impl Default for MirrorOptions {
    fn default() -> Self {
        Self {
            stay_awake: true,
            phone_screen_off: false,
        }
    }
}

/// Command-line arguments for mirroring the phone with adb serial `serial`.
pub fn mirror_args(serial: &str, window_title: &str, opts: &MirrorOptions) -> Vec<String> {
    let mut args = vec![
        "--serial".to_string(),
        serial.to_string(),
        "--window-title".to_string(),
        window_title.to_string(),
    ];
    if opts.stay_awake {
        args.push("--stay-awake".to_string());
    }
    if opts.phone_screen_off {
        args.push("--turn-screen-off".to_string());
    }
    args
}

/// Start mirroring in its own window. Returns the running scrcpy process.
pub fn spawn_mirror(
    scrcpy: &Path,
    serial: &str,
    window_title: &str,
    opts: &MirrorOptions,
) -> Result<Child, String> {
    Command::new(scrcpy)
        .args(mirror_args(serial, window_title, opts))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start scrcpy: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_args() {
        assert_eq!(
            mirror_args("R5CW1", "Seam - S24", &MirrorOptions::default()),
            [
                "--serial",
                "R5CW1",
                "--window-title",
                "Seam - S24",
                "--stay-awake"
            ]
        );
    }

    #[test]
    fn screen_off_option() {
        let opts = MirrorOptions {
            stay_awake: false,
            phone_screen_off: true,
        };
        let args = mirror_args("x", "t", &opts);
        assert!(args.contains(&"--turn-screen-off".to_string()));
        assert!(!args.contains(&"--stay-awake".to_string()));
    }
}
