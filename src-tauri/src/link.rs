//! The phone link inside the app: runs the server, keeps what the window shows,
//! and turns phone notifications into notifications on this computer.

use seam_core::link::{self, LinkEvent, LinkServer, Message, PhoneNotification};
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

/// One row in the phone-notification list.
///
/// Serialized flat: `phone` and the `PhoneNotification` fields (`id`, `app`,
/// `app_name`, `title`, `text`, `time`) are top-level keys. The dismiss button
/// reads `n.phone` and `n.id` from that object, same as `n.app_name`.
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

impl Shared {
    /// Drop this phone's notification `id`, leaving every other entry in place.
    fn remove_notification(&mut self, phone: &str, id: &str) -> bool {
        let before = self.notifications.len();
        self.notifications
            .retain(|n| n.phone != phone || n.notification.id != id);
        self.notifications.len() != before
    }
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

    /// Remove one notification from the window and tell that phone to cancel it.
    /// If the phone is not connected, the notification is still removed locally.
    pub fn dismiss_notification(&self, phone: &str, id: &str) {
        self.shared.lock().unwrap().remove_notification(phone, id);
        if let Some(server) = &self.server {
            // False when the phone is offline; the list entry is already gone.
            let _sent = server.send_to(phone, Message::Dismiss { id: id.to_string() });
        }
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
                let port = listener.local_addr().map(|a| a.port()).ok();
                state.lock().unwrap().port = port;
                // Best-effort: a failure must not stop the link (the QR code still works).
                if let Some(port) = port {
                    advertise_on_network(srv.desktop_name(), srv.fingerprint(), port);
                }
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

/// Publish the link on the local network so phones can find this computer after
/// its address changes. Runs on its own thread; errors are only logged.
fn advertise_on_network(name: &str, fingerprint: &str, port: u16) {
    let name = name.to_string();
    let fingerprint = fingerprint.to_string();
    let spawned = std::thread::Builder::new()
        .name("seam-mdns".to_string())
        .spawn(move || {
            let desc = link::mdns::service_description(&name, port, &fingerprint);
            match link::mdns::advertise(&desc) {
                Ok(_advertiser) => {
                    eprintln!(
                        "Seam link: advertising {} on the local network (port {port})",
                        desc.instance_name
                    );
                    // Dropping the advertiser would stop the announcement.
                    loop {
                        std::thread::park();
                    }
                }
                Err(e) => {
                    eprintln!("Seam link: could not advertise on the local network: {e}");
                }
            }
        });
    if let Err(e) = spawned {
        eprintln!("Seam link: could not advertise on the local network: {e}");
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
            // Clipboard sync is not implemented in the app yet (see the issue tracker).
            LinkEvent::Clipboard { .. } => {}
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

#[cfg(test)]
mod tests {
    use super::{Link, NotificationView, Shared};
    use seam_core::link::{LinkServer, PhoneNotification};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    fn view(phone: &str, id: &str) -> NotificationView {
        NotificationView {
            phone: phone.to_string(),
            notification: PhoneNotification {
                id: id.to_string(),
                app: "com.example".into(),
                app_name: "Example".into(),
                title: String::new(),
                text: String::new(),
                time: 1,
            },
        }
    }

    fn ids(shared: &Shared) -> Vec<&str> {
        shared
            .notifications
            .iter()
            .map(|n| n.notification.id.as_str())
            .collect()
    }

    #[test]
    fn removing_by_id_keeps_the_order_of_the_rest() {
        let mut shared = Shared {
            notifications: VecDeque::from([
                view("phone-a", "n1"),
                view("phone-a", "n2"),
                view("phone-b", "n3"),
                view("phone-a", "n4"),
            ]),
            ..Shared::default()
        };

        assert!(shared.remove_notification("phone-a", "n2"));
        assert_eq!(ids(&shared).as_slice(), ["n1", "n3", "n4"]);

        assert!(shared.remove_notification("phone-b", "n3"));
        assert_eq!(ids(&shared).as_slice(), ["n1", "n4"]);

        // Wrong phone or unknown id must not disturb the list.
        assert!(!shared.remove_notification("phone-a", "missing"));
        assert!(!shared.remove_notification("phone-b", "n1"));
        assert_eq!(ids(&shared).as_slice(), ["n1", "n4"]);

        assert!(shared.remove_notification("phone-a", "n1"));
        assert!(shared.remove_notification("phone-a", "n4"));
        assert!(shared.notifications.is_empty());
        assert!(!shared.remove_notification("phone-a", "n1"));
    }

    #[test]
    fn removes_only_matching_phone_and_keeps_the_other_phones_order() {
        let mut shared = Shared {
            notifications: VecDeque::from([
                view("phone-a", "same"),
                view("phone-b", "same"),
                view("phone-a", "same"),
                view("phone-b", "other"),
            ]),
            ..Shared::default()
        };

        assert!(shared.remove_notification("phone-a", "same"));
        assert_eq!(ids(&shared).as_slice(), ["same", "other"]);
        assert_eq!(shared.notifications[0].phone, "phone-b");
        assert_eq!(shared.notifications[1].phone, "phone-b");
    }

    #[test]
    fn link_status_flattens_phone_and_id_for_the_dismiss_button() {
        let link = Link {
            server: None,
            shared: Arc::new(Mutex::new(Shared {
                notifications: VecDeque::from([view("phone-a", "n1"), view("phone-b", "n2")]),
                ..Shared::default()
            })),
        };
        let json = serde_json::to_value(link.status()).unwrap();
        let notes = json["notifications"].as_array().unwrap();
        assert_eq!(notes.len(), 2);
        // The window already reads app_name/title/text off this object. phone and id
        // are siblings of those fields, not nested under "notification".
        assert!(notes[0].get("notification").is_none());
        assert_eq!(notes[0]["phone"].as_str(), Some("phone-a"));
        assert_eq!(notes[0]["id"].as_str(), Some("n1"));
        assert_eq!(notes[0]["app"].as_str(), Some("com.example"));
        assert_eq!(notes[0]["app_name"].as_str(), Some("Example"));
        assert_eq!(notes[0]["title"].as_str(), Some(""));
        assert_eq!(notes[0]["text"].as_str(), Some(""));
        assert_eq!(notes[0]["time"].as_i64(), Some(1));
        assert_eq!(notes[1]["phone"].as_str(), Some("phone-b"));
        assert_eq!(notes[1]["id"].as_str(), Some("n2"));

        link.dismiss_notification("phone-a", "n1");
        let json = serde_json::to_value(link.status()).unwrap();
        let notes = json["notifications"].as_array().unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0]["phone"].as_str(), Some("phone-b"));
        assert_eq!(notes[0]["id"].as_str(), Some("n2"));
    }

    #[test]
    fn dismiss_removes_locally_when_the_phone_is_not_connected() {
        let dir = std::env::temp_dir().join(format!("seam-dismiss-offline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (server, _events) = LinkServer::new(&dir, "Test Desktop".into()).unwrap();
        assert!(server.connected().is_empty());
        let link = Link {
            server: Some(server),
            shared: Arc::new(Mutex::new(Shared {
                notifications: VecDeque::from([
                    view("phone-a", "n1"),
                    view("phone-a", "n2"),
                    view("phone-b", "n3"),
                ]),
                ..Shared::default()
            })),
        };

        link.dismiss_notification("phone-a", "n2");

        let notes = link.status().notifications;
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].phone, "phone-a");
        assert_eq!(notes[0].notification.id, "n1");
        assert_eq!(notes[1].phone, "phone-b");
        assert_eq!(notes[1].notification.id, "n3");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
