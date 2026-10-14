//! Seam desktop app: the window, and the commands its UI calls.

mod link;
mod open_url;
mod tray;
mod updates;
mod wireless;

use seam_core::{adb, scrcpy, tools, tools::Tool};
use serde::Serialize;
use std::time::Duration;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;

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
fn forget_phone(app: AppHandle, phone_link: State<link::Link>, id: String) -> Result<(), String> {
    phone_link.forget(&id)?;
    tray::refresh(&app);
    Ok(())
}

/// Whether Seam is registered to open at login. Off until the person turns it on.
#[tauri::command]
fn autostart_enabled(app: AppHandle) -> bool {
    tray::autostart_enabled(&app)
}

/// Turn start-at-login on or off, and keep the tray checkbox in step.
#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    tray::set_autostart(&app, enabled)
}

#[tauri::command]
fn dismiss_notification(phone_link: State<link::Link>, phone: String, id: String) {
    phone_link.dismiss_notification(&phone, &id);
}

#[tauri::command]
fn set_dnd(phone_link: State<link::Link>, phone: String, on: bool) -> Result<(), String> {
    phone_link.set_dnd(&phone, on)
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
fn call_action(phone_link: State<link::Link>, phone: String, action: String) -> Result<(), String> {
    phone_link.call_action(&phone, &action)
}

#[tauri::command]
fn ring_phone(phone_link: State<link::Link>, id: String, action: String) -> Result<(), String> {
    phone_link.ring_phone(&id, &action)
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

/// Open the system file picker. `None` means the person cancelled.
#[tauri::command]
async fn pick_file(app: AppHandle) -> Result<Option<String>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Send file")
        .pick_file(move |file| {
            let result = match file {
                None => Ok(None),
                Some(file) => file
                    .into_path()
                    .map(|path| Some(path.display().to_string()))
                    .map_err(|e| format!("could not use that file: {e}")),
            };
            let _ = tx.send(result);
        });
    rx.await.map_err(|_| "file picker failed".to_string())?
}

/// Async so the phone link can spawn the transfer on the running runtime.
#[tauri::command]
async fn send_file(app: AppHandle, phone: String, path: String) -> Result<String, String> {
    app.state::<link::Link>().send_file(&phone, &path)
}

#[tauri::command]
fn cancel_file_send(phone_link: State<link::Link>, transfer: String) -> Result<(), String> {
    phone_link.cancel_send(&transfer)
}

/// Reveal runs off the main thread so a slow file manager cannot freeze the window.
#[tauri::command]
async fn reveal_file(app: AppHandle, path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || app.state::<link::Link>().reveal_received(&path))
        .await
        .map_err(|_| "could not show the file".to_string())?
}

/// Ask a connected phone to send a recent photo. The file arrives as a normal transfer.
#[tauri::command]
fn request_photo(phone_link: State<link::Link>, phone: String, id: String) -> Result<(), String> {
    phone_link.request_photo(&phone, &id)
}

/// Send dropped files to the only connected phone, or ask the window which one.
#[tauri::command]
async fn handle_file_drop(app: AppHandle, paths: Vec<String>) -> Result<link::DropPlan, String> {
    app.state::<link::Link>().deliver_drop(&paths)
}

#[tauri::command]
fn wireless_status(wireless: State<wireless::Wireless>) -> wireless::WirelessStatus {
    wireless.status()
}

#[tauri::command]
fn start_wireless_pairing(
    wireless: State<wireless::Wireless>,
) -> Result<wireless::WirelessPairingView, String> {
    wireless.start_pairing()
}

#[tauri::command]
fn cancel_wireless_pairing(wireless: State<wireless::Wireless>) {
    wireless.cancel();
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
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(updates::Updates::default())
        .manage(SelfTest(self_test))
        .manage(tray::RunFlags::default())
        .setup(move |app| {
            use_bundled_tools(app.handle());
            open_url::install(app.handle());
            app.manage(link::start(app.handle(), !self_test));
            link::watch_drops(app.handle());
            app.manage(wireless::start(app.handle()));
            tray::install_window_behavior(app.handle());
            // No tray in the headless self-test: creating one can wait forever
            // when the virtual display has no status bar, and the test would hang.
            if !self_test {
                if let Err(e) = tray::install(app.handle()) {
                    eprintln!("Seam: tray icon unavailable: {e}");
                }
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
            set_dnd,
            reply_notification,
            call_action,
            ring_phone,
            send_clipboard,
            pick_file,
            send_file,
            cancel_file_send,
            reveal_file,
            handle_file_drop,
            update_status,
            restart_to_update,
            wireless_status,
            start_wireless_pairing,
            cancel_wireless_pairing,
            request_photo,
            open_url::pending_links,
            open_url::send_open_url,
            open_url::open_link,
            autostart_enabled,
            set_autostart,
            frontend_ready
        ])
        .run(tauri::generate_context!())
        .expect("error while running Seam");
}

#[cfg(test)]
mod capability_tests {
    #[test]
    fn photo_request_is_allowed_for_the_window() {
        let caps = include_str!("../capabilities/default.json");
        let perm = include_str!("../permissions/app.toml");
        assert!(
            caps.contains("seam-commands"),
            "the window capability must grant seam-commands or request_photo is denied"
        );
        assert!(
            perm.contains("request_photo"),
            "seam-commands must allow request_photo"
        );
        assert!(perm.contains("frontend_ready"));
        assert!(perm.contains("link_status"));
    }

    #[test]
    fn open_link_commands_are_allowed_for_the_window() {
        let caps = include_str!("../capabilities/default.json");
        let perm = include_str!("../permissions/app.toml");
        assert!(caps.contains("seam-commands"));
        assert!(perm.contains("pending_links"));
        assert!(perm.contains("send_open_url"));
        assert!(perm.contains("open_link"));
    }

    #[test]
    fn dnd_command_is_allowed_for_the_window() {
        let caps = include_str!("../capabilities/default.json");
        let perm = include_str!("../permissions/app.toml");
        assert!(caps.contains("seam-commands"));
        assert!(perm.contains("set_dnd"), "seam-commands must allow set_dnd");
    }

    #[test]
    fn autostart_commands_are_allowed_for_the_window() {
        let caps = include_str!("../capabilities/default.json");
        let perm = include_str!("../permissions/app.toml");
        assert!(caps.contains("seam-commands"));
        assert!(
            perm.contains("autostart_enabled"),
            "seam-commands must allow autostart_enabled"
        );
        assert!(
            perm.contains("set_autostart"),
            "seam-commands must allow set_autostart"
        );
    }
}
