//! Seam desktop app: the window, and the commands its UI calls.

mod link;
mod updates;

use seam_core::{adb, scrcpy, tools, tools::Tool};
use serde::Serialize;
use std::time::Duration;
use tauri::{AppHandle, Manager, State};

/// Whether the app was started with `--self-test` (launch, render, report, exit).
struct SelfTest(bool);

/// One external tool as the UI sees it.
#[derive(Serialize)]
struct ToolStatus {
    found: bool,
    version: Option<String>,
    /// True when Seam is using the copy that ships inside the app.
    bundled: bool,
}

/// Everything the main window shows.
#[derive(Serialize)]
struct Status {
    adb: ToolStatus,
    scrcpy: ToolStatus,
    devices: Vec<adb::Device>,
    error: Option<String>,
}

fn tool_status(tool: Tool) -> (ToolStatus, Option<std::path::PathBuf>) {
    let path = tools::find(tool);
    let version = path.as_deref().and_then(|p| tools::version(tool, p));
    (
        ToolStatus {
            found: path.is_some(),
            version,
            bundled: path.as_deref().is_some_and(tools::is_bundled),
        },
        path,
    )
}

#[tauri::command]
fn status() -> Status {
    let (adb_status, adb_path) = tool_status(Tool::Adb);
    let (scrcpy_status, _) = tool_status(Tool::Scrcpy);
    let (devices, error) = match adb_path {
        Some(p) => match adb::list_devices(&p) {
            Ok(mut devices) => {
                for device in &mut devices {
                    if device.is_ready() {
                        // A battery read failure must not hide the phone.
                        device.battery = adb::read_battery(&p, &device.serial).ok();
                    }
                }
                (devices, None)
            }
            Err(e) => (Vec::new(), Some(e)),
        },
        None => (Vec::new(), None),
    };
    Status {
        adb: adb_status,
        scrcpy: scrcpy_status,
        devices,
        error,
    }
}

#[tauri::command]
fn start_mirror(serial: String, name: String) -> Result<(), String> {
    let scrcpy_path = tools::find(Tool::Scrcpy).ok_or("scrcpy is not installed")?;
    let adb_path = tools::find(Tool::Adb);
    scrcpy::spawn_mirror(
        &scrcpy_path,
        adb_path.as_deref(),
        &serial,
        &format!("Seam - {name}"),
        &scrcpy::MirrorOptions::default(),
    )
    .map(|_child| ())
}

#[tauri::command]
fn link_status(phone_link: State<link::Link>) -> link::LinkStatus {
    phone_link.status()
}

#[tauri::command]
fn start_pairing(phone_link: State<link::Link>) -> Result<link::PairingView, String> {
    phone_link.start_pairing()
}

#[tauri::command]
fn forget_phone(phone_link: State<link::Link>, id: String) -> Result<(), String> {
    phone_link.forget(&id)
}

#[tauri::command]
fn dismiss_notification(phone_link: State<link::Link>, phone: String, id: String) {
    phone_link.dismiss_notification(&phone, &id);
}

#[tauri::command]
fn reply_notification(
    phone_link: State<link::Link>,
    phone: String,
    id: String,
    text: String,
) -> Result<(), String> {
    phone_link.reply_notification(&phone, &id, &text)
}

#[tauri::command]
fn send_clipboard(phone_link: State<link::Link>, id: String) -> Result<(), String> {
    // Synchronous commands already run on the main thread. Read the clipboard
    // there; do not hop through run_on_main_thread and block on the result.
    phone_link.send_clipboard(&id)
}

#[tauri::command]
fn update_status(app: AppHandle, pending: State<updates::Updates>) -> updates::UpdateStatus {
    pending.status(&app.package_info().version.to_string())
}

#[tauri::command]
fn restart_to_update(app: AppHandle) -> Result<(), String> {
    updates::install_and_restart(&app)
}

/// Called by the UI once it has rendered. In self-test mode this ends the app with
/// a pass/fail exit code, so CI can prove the real window starts and works.
#[tauri::command]
fn frontend_ready(app: AppHandle, self_test: State<SelfTest>, ok: bool, detail: String) {
    if self_test.0 {
        println!("SELF-TEST {}: {detail}", if ok { "PASS" } else { "FAIL" });
        app.exit(if ok { 0 } else { 1 });
    }
}

/// Point tool lookup at the adb and scrcpy shipped inside the app, if present.
/// Failure is not fatal: Seam then falls back to tools installed on the computer.
fn use_bundled_tools(app: &AppHandle) {
    let found = app
        .path()
        .resource_dir()
        .map(|d| d.join("tools"))
        .ok()
        .filter(|d| d.join(tools::VERSION_FILE).exists());
    let Some(src) = found else {
        eprintln!("Seam: no built-in tools found; using tools installed on this computer");
        return;
    };
    let cache = app
        .path()
        .app_local_data_dir()
        .map(|d| d.join("tools"))
        .unwrap_or_else(|_| std::env::temp_dir().join("seam-tools"));
    match tools::prepare_bundled(&src, &cache) {
        Ok(dir) => tools::set_bundled_dir(dir),
        Err(e) => eprintln!("Seam: could not prepare built-in tools: {e}"),
    }
}

pub fn run() {
    let self_test = std::env::args().any(|a| a == "--self-test");
    if self_test {
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(60));
            eprintln!("SELF-TEST FAIL: the window never reported that it rendered");
            std::process::exit(1);
        });
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(updates::Updates::default())
        .manage(SelfTest(self_test))
        .setup(move |app| {
            use_bundled_tools(app.handle());
            app.manage(link::start(app.handle(), !self_test));
            if !self_test {
                updates::start(app.handle());
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            status,
            start_mirror,
            link_status,
            start_pairing,
            forget_phone,
            dismiss_notification,
            reply_notification,
            send_clipboard,
            update_status,
            restart_to_update,
            frontend_ready
        ])
        .run(tauri::generate_context!())
        .expect("error while running Seam");
}
