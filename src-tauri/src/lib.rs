//! Seam desktop app: the window, and the commands its UI calls.

use seam_core::{adb, scrcpy, tools, tools::Tool};
use serde::Serialize;
use std::time::Duration;
use tauri::{AppHandle, State};

/// Whether the app was started with `--self-test` (launch, render, report, exit).
struct SelfTest(bool);

/// One external tool as the UI sees it.
#[derive(Serialize)]
struct ToolStatus {
    found: bool,
    version: Option<String>,
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
            Ok(d) => (d, None),
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
    scrcpy::spawn_mirror(
        &scrcpy_path,
        &serial,
        &format!("Seam - {name}"),
        &scrcpy::MirrorOptions::default(),
    )
    .map(|_child| ())
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
        .manage(SelfTest(self_test))
        .invoke_handler(tauri::generate_handler![
            status,
            start_mirror,
            frontend_ready
        ])
        .run(tauri::generate_context!())
        .expect("error while running Seam");
}
