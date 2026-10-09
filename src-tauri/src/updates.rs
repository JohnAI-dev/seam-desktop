//! Automatic updates: check GitHub for a newer signed version, download it in the
//! background, and offer "Restart to update". Only versions signed with Seam's private
//! updater key are installed (the public key is in tauri.conf.json).

use serde::Serialize;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

/// How often to look for a new version while the app is running.
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// What the window shows about updates.
#[derive(Serialize, Clone, PartialEq, Debug)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum UpdateStatus {
    /// No newer version known (or not checked yet).
    UpToDate { current: String },
    /// A newer version is downloaded and ready; restarting installs it.
    Ready { current: String, version: String },
    /// The last check failed; we'll try again later. Shown discreetly.
    Error { current: String, message: String },
}

/// A downloaded update waiting for the restart.
struct Pending {
    update: Update,
    bytes: Vec<u8>,
}

#[derive(Default)]
pub struct Updates {
    status: Mutex<Option<UpdateStatus>>,
    pending: Mutex<Option<Pending>>,
}

impl Updates {
    pub fn status(&self, current: &str) -> UpdateStatus {
        self.status
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(UpdateStatus::UpToDate {
                current: current.to_string(),
            })
    }
}

/// True when this build was configured with an updater public key.
pub fn enabled(app: &AppHandle) -> bool {
    app.config()
        .plugins
        .0
        .get("updater")
        .and_then(|u| u.get("pubkey"))
        .and_then(|k| k.as_str())
        .is_some_and(|k| !k.trim().is_empty())
}

/// Check now and then every few hours, downloading anything new in the background.
pub fn start(app: &AppHandle) {
    if !enabled(app) {
        eprintln!("Seam: updater not configured in this build");
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // Let the window come up first.
        tokio::time::sleep(Duration::from_secs(20)).await;
        loop {
            check_and_download(&app).await;
            tokio::time::sleep(CHECK_EVERY).await;
        }
    });
}

async fn check_and_download(app: &AppHandle) {
    let current = app.package_info().version.to_string();
    let updates = app.state::<Updates>();
    if updates.pending.lock().unwrap().is_some() {
        return; // Already have one waiting for a restart.
    }
    let result = async {
        let updater = app.updater().map_err(|e| e.to_string())?;
        let Some(update) = updater.check().await.map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        // Downloading also verifies the signature against our public key.
        let bytes = update
            .download(|_, _| {}, || {})
            .await
            .map_err(|e| e.to_string())?;
        Ok::<_, String>(Some((update, bytes)))
    }
    .await;
    let status = match result {
        Ok(Some((update, bytes))) => {
            let version = update.version.clone();
            *updates.pending.lock().unwrap() = Some(Pending { update, bytes });
            UpdateStatus::Ready { current, version }
        }
        Ok(None) => UpdateStatus::UpToDate { current },
        Err(message) => {
            eprintln!("Seam: update check failed: {message}");
            UpdateStatus::Error { current, message }
        }
    };
    *updates.status.lock().unwrap() = Some(status);
}

/// Install the downloaded update and restart into it.
pub fn install_and_restart(app: &AppHandle) -> Result<(), String> {
    let pending = app
        .state::<Updates>()
        .pending
        .lock()
        .unwrap()
        .take()
        .ok_or("no update is ready")?;
    // On Windows this hands over to the installer, which closes the app itself.
    pending
        .update
        .install(pending.bytes)
        .map_err(|e| e.to_string())?;
    app.restart();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_json_matches_what_the_window_expects() {
        let ready = UpdateStatus::Ready {
            current: "0.1.40".into(),
            version: "0.1.42".into(),
        };
        let v = serde_json::to_value(&ready).unwrap();
        assert_eq!(v["state"], "ready");
        assert_eq!(v["version"], "0.1.42");
        assert_eq!(v["current"], "0.1.40");

        let idle = Updates::default().status("0.1.40");
        assert_eq!(serde_json::to_value(&idle).unwrap()["state"], "up_to_date");
    }
}
