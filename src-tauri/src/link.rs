//! The phone link inside the app: runs the server, keeps what the window shows,
//! and turns phone notifications into notifications on this computer.

use seam_core::link::{self, LinkEvent, LinkServer, PhoneNotification};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

/// Ports tried for the link, in order.
const PORTS: std::ops::RangeInclusive<u16> = 47100..=47109;
/// How many recent phone notifications the window keeps.
const KEEP_NOTIFICATIONS: usize = 50;

#[derive(Serialize, Clone)]
pub struct PhoneView {
    id: String,
    name: String,
    connected: bool,
    battery: Option<(u8, bool)>,
}

#[derive(Serialize, Clone)]
pub struct NotificationView {
    phone: String,
    #[serde(flatten)]
    notification: PhoneNotification,
}

#[derive(Serialize)]
pub struct LinkStatus {
    running: bool,
    port: Option<u16>,
    error: Option<String>,
    phones: Vec<PhoneView>,
    notifications: Vec<NotificationView>,
}

#[derive(Serialize)]
pub struct PairingView {
    uri: String,
    qr_svg: String,
    expires_in_secs: u64,
}

#[derive(Default)]
struct Shared {
    port: Option<u16>,
    error: Option<String>,
    phones: HashMap<String, PhoneView>,
    notifications: VecDeque<NotificationView>,
}

/// App state for the link. `server` is `None` if it could not start.
pub struct Link {
    server: Option<LinkServer>,
    shared: Arc<Mutex<Shared>>,
}

impl Link {
    pub fn status(&self) -> LinkStatus {
        let s = self.shared.lock().unwrap();
        let mut phones: Vec<_> = s.phones.values().cloned().collect();
        phones.sort_by(|a, b| a.name.cmp(&b.name));
        LinkStatus {
            running: self.server.is_some() && s.port.is_some(),
            port: s.port,
            error: s.error.clone(),
            phones,
            notifications: s.notifications.iter().cloned().collect(),
        }
    }

    pub fn start_pairing(&self) -> Result<PairingView, String> {
        let server = self
            .server
            .as_ref()
            .ok_or("the phone link is not running")?;
        let port = self
            .shared
            .lock()
            .unwrap()
            .port
            .ok_or("the phone link is still starting")?;
        let host = link::local_ip()
            .ok_or("this computer is not on a network; connect to Wi-Fi and try again")?;
        let uri = server.start_pairing(vec![host], port).to_uri();
        Ok(PairingView {
            qr_svg: link::qr_svg(&uri)?,
            uri,
            expires_in_secs: link::server::PAIRING_TTL.as_secs(),
        })
    }

    pub fn forget(&self, id: &str) -> Result<(), String> {
        let server = self
            .server
            .as_ref()
            .ok_or("the phone link is not running")?;
        server.forget(id).map_err(|e| e.to_string())?;
        let mut s = self.shared.lock().unwrap();
        s.phones.remove(id);
        s.notifications.retain(|n| n.phone != id);
        Ok(())
    }
}

fn desktop_name() -> String {
    let run = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let name = if cfg!(target_os = "macos") {
        run("scutil", &["--get", "ComputerName"])
    } else {
        None
    };
    name.or_else(|| run("hostname", &[]))
        .unwrap_or_else(|| "Seam".to_string())
}

/// Start the link server and its event loop. Never fails the app: problems are
/// shown in the window instead.
pub fn start(app: &AppHandle, system_notifications: bool) -> Link {
    let dir = match app.path().app_local_data_dir() {
        Ok(d) => d.join("link"),
        Err(e) => return failed(format!("no data folder: {e}")),
    };
    let (server, mut events) = match LinkServer::new(&dir, desktop_name()) {
        Ok(x) => x,
        Err(e) => return failed(format!("could not start: {e}")),
    };
    let mut initial = Shared::default();
    for d in server.paired_devices() {
        initial.phones.insert(
            d.id.clone(),
            PhoneView {
                id: d.id,
                name: d.name,
                connected: false,
                battery: None,
            },
        );
    }

    let shared = Arc::new(Mutex::new(initial));

    let srv = server.clone();
    let state = shared.clone();
    tauri::async_runtime::spawn(async move {
        match LinkServer::bind(PORTS).await {
            Ok(listener) => {
                state.lock().unwrap().port = listener.local_addr().map(|a| a.port()).ok();
                srv.serve(listener).await;
            }
            Err(e) => {
                state.lock().unwrap().error =
                    Some(format!("could not listen on ports 47100-47109: {e}"));
            }
        }
    });

    let handle = app.clone();
    let state = shared.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            handle_event(&handle, &state, system_notifications, event);
        }
    });

    Link {
        server: Some(server),
        shared,
    }
}

fn failed(error: String) -> Link {
    eprintln!("Seam link: {error}");
    Link {
        server: None,
        shared: Arc::new(Mutex::new(Shared {
            error: Some(error),
            ..Shared::default()
        })),
    }
}

fn handle_event(
    app: &AppHandle,
    shared: &Mutex<Shared>,
    system_notifications: bool,
    event: LinkEvent,
) {
    let mut show: Option<PhoneNotification> = None;
    {
        let mut s = shared.lock().unwrap();
        match event {
            LinkEvent::Paired { device_id, name } | LinkEvent::Connected { device_id, name } => {
                let phone = s.phones.entry(device_id.clone()).or_insert(PhoneView {
                    id: device_id,
                    name: name.clone(),
                    connected: false,
                    battery: None,
                });
                phone.name = name;
                phone.connected = true;
            }
            LinkEvent::Disconnected { device_id } => {
                if let Some(p) = s.phones.get_mut(&device_id) {
                    p.connected = false;
                }
            }
            LinkEvent::Battery {
                device_id,
                level,
                charging,
            } => {
                if let Some(p) = s.phones.get_mut(&device_id) {
                    p.battery = Some((level, charging));
                }
            }
            LinkEvent::Notification {
                device_id,
                notification,
            } => {
                s.notifications
                    .retain(|n| n.notification.id != notification.id);
                s.notifications.push_front(NotificationView {
                    phone: device_id,
                    notification: notification.clone(),
                });
                s.notifications.truncate(KEEP_NOTIFICATIONS);
                show = Some(notification);
            }
            LinkEvent::NotificationRemoved { id, .. } => {
                s.notifications.retain(|n| n.notification.id != id);
            }
        }
    }
    if let (Some(n), true) = (show, system_notifications) {
        let title = if n.title.is_empty() {
            n.app_name.clone()
        } else {
            format!("{} · {}", n.app_name, n.title)
        };
        // Headless machines have no notification service; that's fine.
        let _ = app
            .notification()
            .builder()
            .title(title)
            .body(n.text)
            .show();
    }
}
