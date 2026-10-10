//! The phone link inside the app: runs the server, keeps what the window shows,
//! and turns phone notifications into notifications on this computer.

use seam_core::link::{self, LinkEvent, LinkServer, Message, PhoneNotification};
use serde::Serialize;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};
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
/// `app_name`, `title`, `text`, `time`, `replyable`) are top-level keys. The
/// dismiss and reply buttons read `n.phone` and `n.id` from that object.
/// After a `reply_result`, `reply_seq` / `reply_ok` / `reply_error` are included
/// so the window can apply the result even if it missed the push.
#[derive(Serialize, Clone)]
pub struct NotificationView {
    phone: String,
    #[serde(flatten)]
    notification: PhoneNotification,
    /// Monotonic id of the latest reply_result for this row. Absent until one arrives.
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_error: Option<String>,
}

/// Pushed to the window as soon as a phone answers a reply.
#[derive(Clone, Serialize)]
struct ReplyResultPush {
    phone: String,
    id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    seq: u64,
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
    /// Monotonic so a replaced notification row cannot reuse a reply_result id.
    next_reply_seq: u64,
}

impl Shared {
    /// Drop this phone's notification `id`, leaving every other entry in place.
    fn remove_notification(&mut self, phone: &str, id: &str) -> bool {
        let before = self.notifications.len();
        self.notifications
            .retain(|n| n.phone != phone || n.notification.id != id);
        self.notifications.len() != before
    }

    /// Record a phone's reply_result on that notification. `None` if it is not listed.
    fn apply_reply_result(
        &mut self,
        phone: &str,
        id: &str,
        ok: bool,
        error: Option<String>,
    ) -> Option<u64> {
        let found = self
            .notifications
            .iter()
            .any(|n| n.phone == phone && n.notification.id == id);
        if !found {
            return None;
        }
        self.next_reply_seq = self.next_reply_seq.saturating_add(1);
        let seq = self.next_reply_seq;
        let n = self
            .notifications
            .iter_mut()
            .find(|n| n.phone == phone && n.notification.id == id)?;
        n.reply_seq = Some(seq);
        n.reply_ok = Some(ok);
        n.reply_error = if ok {
            None
        } else {
            Some(reply_error_text(error))
        };
        Some(seq)
    }
}

fn reply_error_text(error: Option<String>) -> String {
    match error {
        Some(text) if !text.trim().is_empty() => text,
        _ => "could not send the reply".to_string(),
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

    /// Send an inline reply to the phone that posted this notification.
    ///
    /// Text is trimmed. Empty text and text longer than 5 000 characters are rejected.
    pub fn reply_notification(&self, phone: &str, id: &str, text: &str) -> Result<(), String> {
        let text = link::protocol::normalize_reply_text(text).map_err(str::to_string)?;
        let server = self
            .server
            .as_ref()
            .ok_or("the phone link is not running")?;
        let replyable = {
            let shared = self.shared.lock().unwrap();
            shared
                .notifications
                .iter()
                .find(|n| n.phone == phone && n.notification.id == id)
                .map(|n| n.notification.replyable)
        };
        match replyable {
            Some(true) => {}
            Some(false) => return Err("this notification cannot be replied to".into()),
            None => return Err("this notification is gone".into()),
        }
        if server.send_to(
            phone,
            Message::Reply {
                id: id.to_string(),
                text,
            },
        ) {
            Ok(())
        } else {
            Err("this phone is not connected".into())
        }
    }

    /// Send this computer's clipboard text to a connected phone.
    ///
    /// Must run on the main thread (a synchronous Tauri command already does).
    /// Text is truncated to 100 000 characters. Returns an error if the phone
    /// is not connected or the clipboard has no text.
    pub fn send_clipboard(&self, id: &str) -> Result<(), String> {
        let server = self
            .server
            .as_ref()
            .ok_or("the phone link is not running")?;
        if !server.connected().iter().any(|c| c == id) {
            return Err("this phone is not connected".into());
        }
        // Already on the main thread. Calling `run_on_main_thread` and blocking
        // on the callback deadlocks: tao only queues it, and this thread is the
        // one that would have to drain that queue.
        let text = truncate_clipboard(&clipboard_text_on_main_thread()?).to_string();
        if text.is_empty() {
            return Err("the clipboard has no text".into());
        }
        if server.send_to(id, Message::Clipboard { text }) {
            Ok(())
        } else {
            Err("this phone is not connected".into())
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

/// Clipboard text is limited to 100 000 characters (see `protocol/PROTOCOL.md`).
const CLIPBOARD_MAX_CHARS: usize = 100_000;

// Kept on the main thread for the life of the process.
//
// Linux (X11) only serves clipboard contents while the `Clipboard` that set them
// is still alive, so a short-lived clipboard would drop the text immediately.
thread_local! {
    static SYSTEM_CLIPBOARD: RefCell<Option<arboard::Clipboard>> =
        const { RefCell::new(None) };
}

/// Truncate clipboard text to `CLIPBOARD_MAX_CHARS` Unicode scalar values.
///
/// Cuts on a character boundary, so a multibyte scalar is never split.
fn truncate_clipboard(text: &str) -> &str {
    match text.char_indices().nth(CLIPBOARD_MAX_CHARS) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

/// macOS pasteboard access must run on the main thread. Headless machines (CI)
/// have no clipboard: writes are ignored, reads return an error.
fn with_system_clipboard<T>(
    f: impl FnOnce(&mut arboard::Clipboard) -> Result<T, String>,
) -> Result<T, String> {
    SYSTEM_CLIPBOARD.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            match arboard::Clipboard::new() {
                Ok(clipboard) => *slot = Some(clipboard),
                Err(e) => return Err(format!("could not read the clipboard: {e}")),
            }
        }
        match slot.as_mut() {
            Some(clipboard) => f(clipboard),
            None => Err("could not read the clipboard".into()),
        }
    })
}

fn set_system_clipboard(app: &AppHandle, text: String) {
    // The link event loop is not the main thread. Queue the write and return;
    // do not wait, or a caller holding `shared` can deadlock the window.
    // Ignore failure: headless CI has no clipboard.
    let _ = app.run_on_main_thread(move || {
        let _ =
            with_system_clipboard(|clipboard| clipboard.set_text(text).map_err(|e| e.to_string()));
    });
}

fn clipboard_text_on_main_thread() -> Result<String, String> {
    with_system_clipboard(|clipboard| match clipboard.get_text() {
        Ok(text) => Ok(text),
        // Image-only or empty pasteboard: nothing to send.
        Err(arboard::Error::ContentNotAvailable) => Ok(String::new()),
        Err(e) => Err(format!("could not read the clipboard: {e}")),
    })
}

fn handle_event(
    app: &AppHandle,
    shared: &Mutex<Shared>,
    system_notifications: bool,
    event: LinkEvent,
) {
    let mut show: Option<PhoneNotification> = None;
    let mut reply_push: Option<ReplyResultPush> = None;
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
                    reply_seq: None,
                    reply_ok: None,
                    reply_error: None,
                });
                s.notifications.truncate(KEEP_NOTIFICATIONS);
                show = Some(notification);
            }
            LinkEvent::NotificationRemoved { id, .. } => {
                s.notifications.retain(|n| n.notification.id != id);
            }
            LinkEvent::Clipboard { text, .. } => {
                // Queues a main-thread write and returns. Do not wait here: this
                // runs while `shared` is locked, and the main thread may need it.
                // Failure is ignored: headless CI has no clipboard.
                set_system_clipboard(app, text);
            }
            LinkEvent::ReplyResult {
                device_id,
                id,
                ok,
                error,
            } => {
                let shown = if ok {
                    None
                } else {
                    Some(reply_error_text(error.clone()))
                };
                let seq = s
                    .apply_reply_result(&device_id, &id, ok, error)
                    .unwrap_or(0);
                reply_push = Some(ReplyResultPush {
                    phone: device_id,
                    id,
                    ok,
                    error: shown,
                    seq,
                });
            }
        }
    }
    if let Some(payload) = reply_push {
        // Push immediately. link_status still carries the result, so a missed event
        // is applied on the next refresh instead of leaving the composer open.
        let _ = app.emit("reply-result", payload);
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
                replyable: false,
            },
            reply_seq: None,
            reply_ok: None,
            reply_error: None,
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
        assert_eq!(notes[0]["replyable"].as_bool(), Some(false));
        assert!(notes[0].get("reply_seq").is_none());
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

    #[test]
    fn truncate_clipboard_leaves_short_text_unchanged() {
        assert_eq!(super::CLIPBOARD_MAX_CHARS, 100_000);
        assert_eq!(super::truncate_clipboard(""), "");
        assert_eq!(super::truncate_clipboard("hei"), "hei");
        // Count characters, not bytes: é is one character and two bytes.
        assert_eq!(super::truncate_clipboard("héllo"), "héllo");
        let exact = "a".repeat(super::CLIPBOARD_MAX_CHARS);
        assert_eq!(super::truncate_clipboard(&exact), exact.as_str());
    }

    #[test]
    fn truncate_clipboard_cuts_at_100_000_characters_not_bytes() {
        let exact = "a".repeat(100_000);
        let longer = format!("{exact}extra");
        assert_eq!(super::truncate_clipboard(&longer), exact.as_str());
        assert_eq!(super::truncate_clipboard(&longer).chars().count(), 100_000);

        // A multibyte character past the limit is dropped whole, never split.
        let han = "你".repeat(100_001);
        let got = super::truncate_clipboard(&han);
        assert_eq!(got.chars().count(), 100_000);
        assert_eq!(got, "你".repeat(100_000));
        assert!(got.is_char_boundary(got.len()));

        let mut mixed = "b".repeat(99_999);
        mixed.push('你');
        mixed.push('!');
        let got = super::truncate_clipboard(&mixed);
        assert_eq!(got.chars().count(), 100_000);
        assert!(got.ends_with('你'));
        assert!(!got.ends_with('!'));
    }

    #[test]
    fn send_clipboard_errors_when_the_phone_is_not_connected() {
        let offline = Link {
            server: None,
            shared: Arc::new(Mutex::new(Shared::default())),
        };
        assert_eq!(
            offline.send_clipboard("phone-1").unwrap_err(),
            "the phone link is not running"
        );

        let dir =
            std::env::temp_dir().join(format!("seam-clipboard-offline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (server, _events) = LinkServer::new(&dir, "Test Desktop".into()).unwrap();
        assert!(server.connected().is_empty());
        let link = Link {
            server: Some(server),
            shared: Arc::new(Mutex::new(Shared::default())),
        };
        // Returns before touching the clipboard, so headless CI can run this.
        assert_eq!(
            link.send_clipboard("phone-1").unwrap_err(),
            "this phone is not connected"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replyable_and_reply_result_are_on_the_notification_the_window_polls() {
        let mut note = view("phone-a", "n1");
        note.notification.replyable = true;
        note.notification.title = "Anna".into();
        let shared = Shared {
            notifications: VecDeque::from([note]),
            ..Shared::default()
        };
        let link = Link {
            server: None,
            shared: Arc::new(Mutex::new(shared)),
        };
        {
            let mut guard = link.shared.lock().unwrap();
            assert_eq!(
                guard.apply_reply_result("phone-a", "missing", true, None),
                None
            );
            assert_eq!(
                guard.apply_reply_result("phone-a", "n1", true, None),
                Some(1)
            );
            assert_eq!(
                guard.apply_reply_result(
                    "phone-a",
                    "n1",
                    false,
                    Some("notification is gone".into())
                ),
                Some(2)
            );
            assert_eq!(
                guard.apply_reply_result("phone-a", "n1", false, Some("  ".into())),
                Some(3)
            );
            assert_eq!(
                guard.notifications[0].reply_error.as_deref(),
                Some("could not send the reply")
            );
        }
        let json = serde_json::to_value(link.status()).unwrap();
        let note = &json["notifications"][0];
        assert!(note.get("notification").is_none());
        assert_eq!(note["phone"], "phone-a");
        assert_eq!(note["id"], "n1");
        assert_eq!(note["replyable"], true);
        assert_eq!(note["title"], "Anna");
        assert_eq!(note["reply_seq"], 3);
        assert_eq!(note["reply_ok"], false);
        assert_eq!(note["reply_error"], "could not send the reply");
    }

    #[test]
    fn reply_notification_rejects_empty_or_too_long_text() {
        let link = Link {
            server: None,
            shared: Arc::new(Mutex::new(Shared::default())),
        };
        assert_eq!(
            link.reply_notification("phone-a", "n1", "   ").unwrap_err(),
            "reply is empty"
        );
        assert_eq!(
            link.reply_notification("phone-a", "n1", "").unwrap_err(),
            "reply is empty"
        );
        assert_eq!(
            link.reply_notification("phone-a", "n1", &"x".repeat(5_001))
                .unwrap_err(),
            "reply is too long"
        );
        assert_eq!(
            link.reply_notification("phone-a", "n1", &"你".repeat(5_001))
                .unwrap_err(),
            "reply is too long"
        );
        // Trimmed 5 000 characters is allowed; the link itself is what fails here.
        let padded = format!("  {}  ", "x".repeat(5_000));
        assert_eq!(
            link.reply_notification("phone-a", "n1", &padded)
                .unwrap_err(),
            "the phone link is not running"
        );
    }

    #[test]
    fn reply_notification_requires_a_replyable_notification_on_a_connected_phone() {
        let dir = std::env::temp_dir().join(format!("seam-reply-offline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (server, _events) = LinkServer::new(&dir, "Test Desktop".into()).unwrap();
        assert!(server.connected().is_empty());
        let mut replyable = view("phone-a", "n1");
        replyable.notification.replyable = true;
        let link = Link {
            server: Some(server),
            shared: Arc::new(Mutex::new(Shared {
                notifications: VecDeque::from([replyable, view("phone-a", "n2")]),
                ..Shared::default()
            })),
        };
        assert_eq!(
            link.reply_notification("phone-a", "n2", "hello")
                .unwrap_err(),
            "this notification cannot be replied to"
        );
        assert_eq!(
            link.reply_notification("phone-a", "missing", "hello")
                .unwrap_err(),
            "this notification is gone"
        );
        assert_eq!(
            link.reply_notification("phone-a", "n1", "  hello  ")
                .unwrap_err(),
            "this phone is not connected"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
