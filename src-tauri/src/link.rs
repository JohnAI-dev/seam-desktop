//! The phone link inside the app: runs the server, keeps what the window shows,
//! and turns phone notifications and incoming calls into notifications on this computer.

use seam_core::link::{self, LinkEvent, LinkServer, Message, PhoneNotification};
use serde::Serialize;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

/// Ports tried for the link, in order.
const PORTS: std::ops::RangeInclusive<u16> = 47100..=47109;
/// How many recent phone notifications the window keeps.
const KEEP_NOTIFICATIONS: usize = 50;
/// Recent outbound file transfers kept in the window, newest first.
const KEEP_SENDS: usize = 20;
/// Received files kept in the window, newest first.
const KEEP_RECEIVED: usize = 50;
const SEND_SENDING: &str = "sending";
const SEND_DONE: &str = "done";
const SEND_FAILED: &str = "failed";
/// ShowItems must answer quickly; otherwise Show in folder opens the folder instead.
const DBUS_REPLY_TIMEOUT_MS: u64 = 2_000;

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

/// A ringing call the window shows as a banner.
///
/// `caller` is the contact name, or the number, or "Unknown caller".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CallView {
    phone: String,
    state: String,
    number: String,
    name: String,
    caller: String,
}

/// One outbound file, shown with progress, Cancel, and the result.
#[derive(Debug, Clone, Serialize)]
struct SendView {
    phone: String,
    transfer: String,
    name: String,
    sent: u64,
    size: u64,
    /// `sending`, `done`, or `failed`.
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// A file saved from a phone.
#[derive(Debug, Clone, Serialize)]
struct ReceivedFileView {
    phone: String,
    phone_name: String,
    name: String,
    path: String,
}

/// One connected phone the window can offer as a drop target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DropPhone {
    pub id: String,
    pub name: String,
}

/// What to do with files dropped on the window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DropPlan {
    Error {
        message: String,
    },
    /// Sent to the only connected phone (its display name).
    Sent {
        phone: String,
    },
    /// Several phones are connected; the window must ask. `paths` are real files only.
    Ask {
        phones: Vec<DropPhone>,
        paths: Vec<String>,
    },
}

#[derive(Serialize)]
pub struct LinkStatus {
    running: bool,
    port: Option<u16>,
    error: Option<String>,
    phones: Vec<PhoneView>,
    notifications: Vec<NotificationView>,
    /// Ringing call to show, or null when the banner should be hidden.
    call: Option<CallView>,
    /// How long the Ring button stays on Stop, in seconds.
    ring_secs: u64,
    /// Outbound file transfers, newest first.
    sends: Vec<SendView>,
    /// Files received from phones, newest first.
    received: Vec<ReceivedFileView>,
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
    /// Ringing call shown in the window, if any.
    call: Option<CallView>,
    /// Outbound transfers, newest first.
    sends: VecDeque<SendView>,
    /// Files saved from phones, newest first.
    received: VecDeque<ReceivedFileView>,
    /// Last time a send-progress refresh was pushed to the window.
    last_transfer_emit: Option<Instant>,
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

    /// Apply a phone call update.
    ///
    /// Ringing replaces the banner and returns the label to notify with.
    /// Active and ended from the same phone hide it. Another phone's call is left
    /// alone, and an unknown state changes nothing.
    fn apply_call(
        &mut self,
        phone: &str,
        state: link::protocol::CallState,
        number: &str,
        name: &str,
    ) -> Option<String> {
        match state {
            link::protocol::CallState::Ringing => {
                let caller = link::protocol::caller_label(name, number);
                self.call = Some(CallView {
                    phone: phone.to_string(),
                    state: "ringing".to_string(),
                    number: number.to_string(),
                    name: name.to_string(),
                    caller: caller.clone(),
                });
                Some(caller)
            }
            link::protocol::CallState::Active | link::protocol::CallState::Ended => {
                self.clear_call(phone);
                None
            }
            link::protocol::CallState::Unknown => None,
        }
    }

    /// Drop the banner if it belongs to `phone`. Returns whether it was showing.
    fn clear_call(&mut self, phone: &str) -> bool {
        if self.call.as_ref().is_some_and(|c| c.phone == phone) {
            self.call = None;
            true
        } else {
            false
        }
    }

    fn note_send_progress(
        &mut self,
        phone: &str,
        transfer: &str,
        name: &str,
        sent: u64,
        size: u64,
    ) {
        if let Some(row) = self.sends.iter_mut().find(|row| row.transfer == transfer) {
            if row.state != SEND_SENDING {
                return;
            }
            row.sent = sent;
            row.size = size;
            if !name.is_empty() {
                row.name = name.to_string();
            }
            return;
        }
        self.sends.push_front(SendView {
            phone: phone.to_string(),
            transfer: transfer.to_string(),
            name: display_file_name(name),
            sent,
            size,
            state: SEND_SENDING.to_string(),
            error: None,
        });
        self.sends.truncate(KEEP_SENDS);
    }

    fn note_send_finished(
        &mut self,
        phone: &str,
        transfer: &str,
        name: &str,
        ok: bool,
        error: Option<String>,
    ) {
        let state = if ok { SEND_DONE } else { SEND_FAILED };
        let error = if ok {
            None
        } else {
            Some(send_error_text(error))
        };
        if let Some(row) = self.sends.iter_mut().find(|row| row.transfer == transfer) {
            if ok {
                row.sent = row.size;
            }
            row.state = state.to_string();
            row.error = error;
            if !name.is_empty() {
                row.name = name.to_string();
            }
            return;
        }
        self.sends.push_front(SendView {
            phone: phone.to_string(),
            transfer: transfer.to_string(),
            name: display_file_name(name),
            sent: 0,
            size: 0,
            state: state.to_string(),
            error,
        });
        self.sends.truncate(KEEP_SENDS);
    }

    fn record_received(&mut self, phone: &str, phone_name: &str, path: &Path) -> String {
        let name = file_label(path);
        let notice = received_notice(&name, phone_name);
        self.received.push_front(ReceivedFileView {
            phone: phone.to_string(),
            phone_name: phone_name.to_string(),
            name,
            path: path.to_string_lossy().into_owned(),
        });
        self.received.truncate(KEEP_RECEIVED);
        notice
    }

    /// True when a progress event should refresh the window. Finished transfers always emit.
    fn transfer_emit_due(&mut self) -> bool {
        let now = Instant::now();
        let due = match self.last_transfer_emit {
            Some(then) => now.duration_since(then) >= Duration::from_millis(200),
            None => true,
        };
        if due {
            self.last_transfer_emit = Some(now);
        }
        due
    }
}

fn reply_error_text(error: Option<String>) -> String {
    match error {
        Some(text) if !text.trim().is_empty() => text,
        _ => "could not send the reply".to_string(),
    }
}

/// Frame for Find my phone: `ring` starts, `ring_stop` stops.
fn ring_message(action: &str) -> Result<Message, &'static str> {
    match action {
        "ring" => Ok(Message::Ring),
        "ring_stop" => Ok(Message::RingStop),
        _ => Err("unknown ring action"),
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
            call: s.call.clone(),
            ring_secs: link::protocol::RING_SECS,
            sends: s.sends.iter().cloned().collect(),
            received: s.received.iter().cloned().collect(),
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
        s.clear_call(id);
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

    /// Decline or silence the ringing call from `phone`.
    ///
    /// `action` is `decline` or `silence`. Sends a `call_action` frame. The banner
    /// stays until the phone reports `active` or `ended`.
    pub fn call_action(&self, phone: &str, action: &str) -> Result<(), String> {
        let kind = match action {
            "decline" => link::protocol::CallActionKind::Decline,
            "silence" => link::protocol::CallActionKind::Silence,
            _ => return Err("unknown call action".into()),
        };
        let server = self
            .server
            .as_ref()
            .ok_or("the phone link is not running")?;
        let ringing = self
            .shared
            .lock()
            .unwrap()
            .call
            .as_ref()
            .is_some_and(|c| c.phone == phone && c.state == "ringing");
        if !ringing {
            return Err("there is no ringing call".into());
        }
        if server.send_to(phone, Message::CallAction { action: kind }) {
            Ok(())
        } else {
            Err("this phone is not connected".into())
        }
    }

    /// Start or stop ringing a connected phone (Find my phone).
    ///
    /// `action` is `ring` or `ring_stop`. The phone rings at alarm volume until
    /// `ring_stop` arrives, its "Found it" button is pressed, or 60 seconds pass.
    /// The window shows Stop while ringing and reverts after that timeout.
    pub fn ring_phone(&self, id: &str, action: &str) -> Result<(), String> {
        let msg = ring_message(action).map_err(str::to_string)?;
        let server = self
            .server
            .as_ref()
            .ok_or("the phone link is not running")?;
        if server.send_to(id, msg) {
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

    /// Start sending `path` to a connected phone. Returns the transfer id.
    ///
    /// A failure is also listed so a multi-file drop can show which file did not start.
    pub fn send_file(&self, phone: &str, path: &str) -> Result<String, String> {
        self.queue_send(phone, path).inspect_err(|error| {
            if !path.is_empty() {
                self.note_local_send_failure(phone, path, error);
            }
        })
    }

    fn queue_send(&self, phone: &str, path: &str) -> Result<String, String> {
        if path.is_empty() {
            return Err("no file chosen".into());
        }
        let server = self
            .server
            .as_ref()
            .ok_or("the phone link is not running")?;
        server.send_file(phone, Path::new(path))
    }

    fn note_local_send_failure(&self, phone: &str, path: &str, error: &str) {
        let mut shared = self.shared.lock().unwrap();
        shared.sends.push_front(SendView {
            phone: phone.to_string(),
            transfer: local_failure_id(),
            name: file_label(Path::new(path)),
            sent: 0,
            size: 0,
            state: SEND_FAILED.to_string(),
            error: Some(send_error_text(Some(error.to_string()))),
        });
        shared.sends.truncate(KEEP_SENDS);
    }

    /// Stop an outbound transfer started by [`Self::send_file`].
    ///
    /// Already-finished transfers are a no-op so a late Cancel is not an error.
    pub fn cancel_send(&self, transfer: &str) -> Result<(), String> {
        let server = self
            .server
            .as_ref()
            .ok_or("the phone link is not running")?;
        if server.cancel_send(transfer) {
            return Ok(());
        }
        let already_finished = self
            .shared
            .lock()
            .unwrap()
            .sends
            .iter()
            .any(|send| send.transfer == transfer && send.state != SEND_SENDING);
        if already_finished {
            Ok(())
        } else {
            Err("this transfer is not running".into())
        }
    }

    /// Reveal a received file in Finder, Explorer, or the file manager.
    ///
    /// Only paths this session actually received are accepted.
    pub fn reveal_received(&self, path: &str) -> Result<(), String> {
        if path.is_empty() || !self.owns_received_path(path) {
            return Err("unknown file".into());
        }
        let file = PathBuf::from(path);
        if !file.is_file() {
            return Err("file is gone".into());
        }
        reveal_path(&file)
    }

    fn owns_received_path(&self, path: &str) -> bool {
        self.shared
            .lock()
            .unwrap()
            .received
            .iter()
            .any(|file| file.path == path)
    }

    fn connected_phone_choices(&self) -> Vec<DropPhone> {
        let mut phones: Vec<_> = {
            let shared = self.shared.lock().unwrap();
            shared
                .phones
                .values()
                .filter(|phone| phone.connected)
                .map(|phone| DropPhone {
                    id: phone.id.clone(),
                    name: phone.name.clone(),
                })
                .collect()
        };
        phones.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        phones
    }

    /// Send dropped files to the only connected phone, or ask when there are several.
    ///
    /// Directories are skipped. A drop that contains no files is reported, not sent.
    /// If at least one file was queued, this is [`DropPlan::Sent`] even when another
    /// file could not start. The window must not treat a partial send as a total failure.
    pub fn deliver_drop(&self, paths: &[String]) -> Result<DropPlan, String> {
        if paths.is_empty() || paths.iter().all(|path| path.is_empty()) {
            return Ok(DropPlan::Error {
                message: "no file chosen".into(),
            });
        }
        let files: Vec<String> = paths
            .iter()
            .filter(|path| !path.is_empty() && Path::new(path).is_file())
            .cloned()
            .collect();
        if files.is_empty() {
            return Ok(DropPlan::Error {
                message: "not a file".into(),
            });
        }
        let phones = self.connected_phone_choices();
        match phones.len() {
            0 => Ok(DropPlan::Error {
                message: "no phone is connected".into(),
            }),
            1 => {
                let phone = &phones[0];
                self.send_paths(&phone.id, &files)?;
                Ok(DropPlan::Sent {
                    phone: phone.name.clone(),
                })
            }
            _ => Ok(DropPlan::Ask {
                phones,
                paths: files,
            }),
        }
    }

    fn send_paths(&self, phone: &str, paths: &[String]) -> Result<(), String> {
        let mut started = 0usize;
        let mut first_error = None;
        for path in paths {
            match self.send_file(phone, path) {
                Ok(_) => started += 1,
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        classify_send_batch(started, first_error)
    }
}

/// A drop succeeds when at least one file was queued. Later errors must not hide that.
fn classify_send_batch(started: usize, first_error: Option<String>) -> Result<(), String> {
    if started > 0 {
        Ok(())
    } else {
        Err(first_error.unwrap_or_else(|| "no file chosen".to_string()))
    }
}

fn local_failure_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("local-{n}")
}

/// Forward files dropped on the window to the UI (`files-dropped`).
pub fn watch_drops(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let app = app.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::DragDrop(tauri::DragDropEvent::Drop { paths, .. }) = event {
            let paths = dropped_path_list(paths);
            if !paths.is_empty() {
                let _ = app.emit("files-dropped", paths);
            }
        }
    });
}

/// Paths from a Tauri drag-drop event, skipping blanks.
fn dropped_path_list(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .filter(|path| !path.is_empty())
        .collect()
}

fn phone_display_name(name: &str, id: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        id.to_string()
    } else {
        name.to_string()
    }
}

fn display_file_name(name: &str) -> String {
    if name.is_empty() {
        "file".to_string()
    } else {
        name.to_string()
    }
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty() && name != "." && name != "..")
        .unwrap_or_else(|| "file".to_string())
}

/// System notification text: `Received <name> from <phone>`.
fn received_notice(name: &str, phone: &str) -> String {
    format!("Received {name} from {phone}")
}

fn send_error_text(error: Option<String>) -> String {
    match error {
        Some(text) if !text.trim().is_empty() => text,
        _ => "could not send the file".to_string(),
    }
}

fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                uri.push(byte as char);
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

fn folder_to_open(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => path,
    }
}

fn macos_reveal_args(path: &Path) -> Vec<String> {
    vec!["-R".to_string(), path.display().to_string()]
}

/// One raw command-line token: `/select,"<path>"`.
///
/// Explorer only honors `/select` when that switch is not wrapped in quotes.
/// `Command` quotes any argument that contains a space, which hides the switch
/// for user profiles and names like `photo (1).jpg`.
fn windows_select_arg(path: &Path) -> String {
    let shown = path.display().to_string().replace('"', "");
    format!("/select,\"{shown}\"")
}

fn linux_show_items_args(uri: &str) -> Vec<String> {
    // Quotes are part of the text dbus-send parses. An unquoted `file://` URI is
    // split on colons, so ShowItems can exit 0 without revealing anything.
    let uri = uri.replace('"', "%22");
    vec![
        "--session".to_string(),
        "--dest=org.freedesktop.FileManager1".to_string(),
        "--type=method_call".to_string(),
        "--print-reply".to_string(),
        format!("--reply-timeout={DBUS_REPLY_TIMEOUT_MS}"),
        "/org/freedesktop/FileManager1".to_string(),
        "org.freedesktop.FileManager1.ShowItems".to_string(),
        format!("array:string:\"{uri}\""),
        "string:\"\"".to_string(),
    ]
}

fn spawn_detached(program: &str, args: &[String]) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not show the file: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Explorer only honors `/select` when that switch is not wrapped in quotes.
/// Pass the argument with `raw_arg` so the command line is `/select,"<path>"`.
#[cfg(windows)]
fn spawn_explorer(raw: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let mut child = Command::new("explorer")
        .raw_arg(raw)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not show the file: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(not(windows))]
fn spawn_explorer(_raw: &str) -> Result<(), String> {
    Err("could not show the file".into())
}

fn reveal_with_explorer(path: &Path) -> Result<(), String> {
    spawn_explorer(&windows_select_arg(path))
}

fn dbus_show_items_ok(args: &[String]) -> bool {
    let args = args.to_vec();
    let (tx, rx) = std::sync::mpsc::channel();
    let started = std::thread::Builder::new()
        .name("seam-reveal".to_string())
        .spawn(move || {
            let ok = Command::new("dbus-send")
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            let _ = tx.send(ok);
        });
    if started.is_err() {
        return false;
    }
    // A little past dbus-send's own reply timeout, so a hung bus cannot stick.
    rx.recv_timeout(Duration::from_millis(DBUS_REPLY_TIMEOUT_MS + 500))
        .unwrap_or(false)
}

fn reveal_on_linux(path: &Path) -> Result<(), String> {
    let args = linux_show_items_args(&file_uri(path));
    if dbus_show_items_ok(&args) {
        return Ok(());
    }
    let dir = folder_to_open(path).to_string_lossy().into_owned();
    spawn_detached("xdg-open", &[dir])
}

fn reveal_path(path: &Path) -> Result<(), String> {
    match std::env::consts::OS {
        "macos" => spawn_detached("open", &macos_reveal_args(path)),
        "windows" => reveal_with_explorer(path),
        _ => reveal_on_linux(path),
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
    let mut notify_caller: Option<String> = None;
    let mut call_changed = false;
    let mut received_notice_text: Option<String> = None;
    let mut transfer_changed = false;
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
                call_changed = s.clear_call(&device_id);
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
            LinkEvent::Call {
                device_id,
                state,
                number,
                name,
            } => {
                notify_caller = s.apply_call(&device_id, state, &number, &name);
                call_changed = true;
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
            LinkEvent::FileReceived { phone, path } => {
                let known = s
                    .phones
                    .get(&phone)
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                let phone_name = phone_display_name(&known, &phone);
                // "Received <name> from <phone>" — saved file name and the phone's name.
                received_notice_text = Some(s.record_received(&phone, &phone_name, &path));
                transfer_changed = true;
            }
            LinkEvent::FileSendProgress {
                phone,
                transfer,
                name,
                sent,
                size,
            } => {
                s.note_send_progress(&phone, &transfer, &name, sent, size);
                transfer_changed = s.transfer_emit_due();
            }
            LinkEvent::FileSendFinished {
                phone,
                transfer,
                name,
                ok,
                error,
            } => {
                s.note_send_finished(&phone, &transfer, &name, ok, error);
                transfer_changed = true;
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
    if call_changed {
        // Polling is every 2s; push so a ringing banner appears, and active/ended hides it, now.
        let _ = app.emit("incoming-call", ());
    }
    if let (Some(caller), true) = (notify_caller, system_notifications) {
        // Headless machines have no notification service; that's fine.
        let _ = app
            .notification()
            .builder()
            .title("Incoming call")
            .body(caller)
            .show();
    }
    if transfer_changed {
        let _ = app.emit("file-transfer", ());
    }
    if let (Some(text), true) = (received_notice_text, system_notifications) {
        // Headless machines have no notification service; that's fine.
        let _ = app.notification().builder().title(text).show();
    }
}

#[cfg(test)]
mod tests {
    use super::{DropPhone, DropPlan, Link, NotificationView, PhoneView, Shared};
    use seam_core::link::protocol::CallState;
    use seam_core::link::{LinkServer, Message, PhoneNotification};
    use std::collections::{HashMap, VecDeque};
    use std::path::{Path, PathBuf};
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

    #[test]
    fn ringing_banner_uses_name_or_number_or_unknown_caller() {
        let link = Link {
            server: None,
            shared: Arc::new(Mutex::new(Shared::default())),
        };
        assert!(serde_json::to_value(link.status()).unwrap()["call"].is_null());

        {
            let mut shared = link.shared.lock().unwrap();
            assert_eq!(
                shared
                    .apply_call("phone-a", CallState::Ringing, "+47123", "Anna")
                    .as_deref(),
                Some("Anna")
            );
        }
        let call = &serde_json::to_value(link.status()).unwrap()["call"];
        assert_eq!(call["phone"].as_str(), Some("phone-a"));
        assert_eq!(call["state"].as_str(), Some("ringing"));
        assert_eq!(call["number"].as_str(), Some("+47123"));
        assert_eq!(call["name"].as_str(), Some("Anna"));
        assert_eq!(call["caller"].as_str(), Some("Anna"));

        {
            let mut shared = link.shared.lock().unwrap();
            assert_eq!(
                shared
                    .apply_call("phone-a", CallState::Ringing, "+47 00", "  ")
                    .as_deref(),
                Some("+47 00")
            );
            assert_eq!(
                shared
                    .apply_call("phone-a", CallState::Ringing, " \t", "   ")
                    .as_deref(),
                Some("Unknown caller")
            );
        }
        assert_eq!(
            link.status().call.as_ref().map(|c| c.caller.as_str()),
            Some("Unknown caller")
        );
    }

    #[test]
    fn active_or_ended_hides_only_that_phones_banner() {
        let mut shared = Shared::default();
        shared.apply_call("phone-a", CallState::Ringing, "", "Anna");
        shared.apply_call("phone-b", CallState::Ringing, "555", "");
        assert_eq!(
            shared.call.as_ref().map(|c| c.phone.as_str()),
            Some("phone-b")
        );
        assert_eq!(shared.call.as_ref().map(|c| c.caller.as_str()), Some("555"));

        // Ending a different phone, or an unknown state, leaves the banner up.
        assert!(shared
            .apply_call("phone-a", CallState::Ended, "", "")
            .is_none());
        assert_eq!(
            shared.call.as_ref().map(|c| c.phone.as_str()),
            Some("phone-b")
        );
        assert!(shared
            .apply_call("phone-b", CallState::Unknown, "1", "Other")
            .is_none());
        assert_eq!(shared.call.as_ref().map(|c| c.caller.as_str()), Some("555"));
        assert!(!shared.clear_call("phone-a"));

        assert!(shared
            .apply_call("phone-b", CallState::Active, "555", "")
            .is_none());
        assert!(shared.call.is_none());

        shared.apply_call("phone-a", CallState::Ringing, "", "");
        assert_eq!(
            shared.call.as_ref().map(|c| c.caller.as_str()),
            Some("Unknown caller")
        );
        assert!(shared.clear_call("phone-a"));
        assert!(shared.call.is_none());
        assert!(shared
            .apply_call("phone-a", CallState::Ended, "", "")
            .is_none());
        assert!(shared.call.is_none());
    }

    #[test]
    fn call_action_checks_the_action_and_the_ringing_call() {
        let offline = Link {
            server: None,
            shared: Arc::new(Mutex::new(Shared::default())),
        };
        assert_eq!(
            offline.call_action("phone-a", "hangup").unwrap_err(),
            "unknown call action"
        );
        assert_eq!(
            offline.call_action("phone-a", "decline").unwrap_err(),
            "the phone link is not running"
        );

        let dir = std::env::temp_dir().join(format!("seam-call-action-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (server, _events) = LinkServer::new(&dir, "Test Desktop".into()).unwrap();
        let link = Link {
            server: Some(server),
            shared: Arc::new(Mutex::new(Shared::default())),
        };
        assert_eq!(
            link.call_action("phone-a", "silence").unwrap_err(),
            "there is no ringing call"
        );
        link.shared
            .lock()
            .unwrap()
            .apply_call("phone-a", CallState::Ringing, "+47123", "Anna");
        assert_eq!(
            link.call_action("phone-b", "decline").unwrap_err(),
            "there is no ringing call"
        );
        assert_eq!(
            link.call_action("phone-a", "decline").unwrap_err(),
            "this phone is not connected"
        );
        assert_eq!(
            link.call_action("phone-a", "silence").unwrap_err(),
            "this phone is not connected"
        );
        // A failed send must not hide the banner; only active/ended do.
        assert_eq!(
            link.status().call.as_ref().map(|c| c.state.as_str()),
            Some("ringing")
        );
        assert_eq!(
            link.status().call.as_ref().map(|c| c.caller.as_str()),
            Some("Anna")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ring_phone_sends_ring_or_ring_stop_only_when_connected() {
        assert_eq!(super::ring_message("ring").unwrap(), Message::Ring);
        assert_eq!(
            super::ring_message("ring").unwrap().to_line(),
            r#"{"type":"ring"}"#,
        );
        assert_eq!(super::ring_message("ring_stop").unwrap(), Message::RingStop);
        assert_eq!(
            super::ring_message("ring_stop").unwrap().to_line(),
            r#"{"type":"ring_stop"}"#,
        );
        assert_eq!(
            super::ring_message("stop").unwrap_err(),
            "unknown ring action"
        );
        assert_eq!(super::ring_message("").unwrap_err(), "unknown ring action");

        let offline = Link {
            server: None,
            shared: Arc::new(Mutex::new(Shared::default())),
        };
        assert_eq!(
            serde_json::to_value(offline.status()).unwrap()["ring_secs"],
            60
        );
        assert_eq!(
            offline.ring_phone("phone-a", "nope").unwrap_err(),
            "unknown ring action"
        );
        assert_eq!(
            offline.ring_phone("phone-a", "ring").unwrap_err(),
            "the phone link is not running"
        );

        let dir = std::env::temp_dir().join(format!("seam-ring-offline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (server, _events) = LinkServer::new(&dir, "Test Desktop".into()).unwrap();
        let link = Link {
            server: Some(server),
            shared: Arc::new(Mutex::new(Shared::default())),
        };
        assert_eq!(
            link.ring_phone("phone-a", "ring").unwrap_err(),
            "this phone is not connected"
        );
        assert_eq!(
            link.ring_phone("phone-a", "ring_stop").unwrap_err(),
            "this phone is not connected"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forget_hides_only_that_phones_ringing_call() {
        let dir = std::env::temp_dir().join(format!("seam-call-forget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (server, _events) = LinkServer::new(&dir, "Test Desktop".into()).unwrap();
        let link = Link {
            server: Some(server),
            shared: Arc::new(Mutex::new(Shared::default())),
        };
        {
            let mut shared = link.shared.lock().unwrap();
            shared.apply_call("phone-a", CallState::Ringing, "", "Anna");
            shared.apply_call("phone-b", CallState::Ringing, "555", "");
        }
        assert_eq!(
            link.status().call.as_ref().map(|c| c.phone.as_str()),
            Some("phone-b")
        );
        link.forget("phone-a").unwrap();
        assert_eq!(
            link.status().call.as_ref().map(|c| c.phone.as_str()),
            Some("phone-b")
        );
        link.forget("phone-b").unwrap();
        assert!(link.status().call.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn phone_view(id: &str, name: &str, connected: bool) -> PhoneView {
        PhoneView {
            id: id.to_string(),
            name: name.to_string(),
            connected,
            battery: None,
        }
    }

    fn link_with(phones: Vec<PhoneView>) -> Link {
        let mut map = HashMap::new();
        for phone in phones {
            map.insert(phone.id.clone(), phone);
        }
        Link {
            server: None,
            shared: Arc::new(Mutex::new(Shared {
                phones: map,
                ..Shared::default()
            })),
        }
    }

    #[test]
    fn send_progress_keeps_one_row_and_records_the_result() {
        let mut shared = Shared::default();
        let id = "ab".repeat(16);
        shared.note_send_progress("phone-a", &id, "photo.jpg", 0, 10);
        shared.note_send_progress("phone-a", &id, "photo.jpg", 4, 10);
        shared.note_send_progress("phone-a", "bb".repeat(16).as_str(), "other.txt", 1, 2);
        assert_eq!(shared.sends.len(), 2);
        assert_eq!(shared.sends[0].name, "other.txt");
        assert_eq!(shared.sends[1].transfer, id);
        assert_eq!(shared.sends[1].sent, 4);
        assert_eq!(shared.sends[1].state, "sending");
        assert!(shared.sends[1].error.is_none());

        shared.note_send_finished("phone-a", &id, "photo.jpg", false, Some("cancelled".into()));
        assert_eq!(shared.sends[1].state, "failed");
        assert_eq!(shared.sends[1].error.as_deref(), Some("cancelled"));
        assert_eq!(shared.sends[1].sent, 4);
        // A late progress event must not reopen a finished transfer.
        shared.note_send_progress("phone-a", &id, "photo.jpg", 0, 10);
        assert_eq!(shared.sends[1].state, "failed");
        assert_eq!(shared.sends[1].sent, 4);

        shared.note_send_finished("phone-a", "missing", "", false, Some("  ".into()));
        assert_eq!(shared.sends[0].name, "file");
        assert_eq!(
            shared.sends[0].error.as_deref(),
            Some("could not send the file")
        );
        shared.note_send_finished("phone-a", "missing", "a.txt", true, Some("nope".into()));
        assert_eq!(shared.sends[0].state, "done");
        assert_eq!(shared.sends[0].name, "a.txt");
        assert!(shared.sends[0].error.is_none());
    }

    #[test]
    fn received_files_are_listed_newest_first_with_the_notice_text() {
        assert_eq!(
            super::received_notice("photo.jpg", "Pixel"),
            "Received photo.jpg from Pixel"
        );
        assert_eq!(
            super::received_notice("photo (1).jpg", "Ada's Phone"),
            "Received photo (1).jpg from Ada's Phone"
        );
        assert_eq!(super::file_label(Path::new("../../etc/passwd")), "passwd");
        assert_eq!(super::file_label(Path::new("/")), "file");
        assert_eq!(super::phone_display_name("  Pixel  ", "id"), "Pixel");
        assert_eq!(super::phone_display_name("   ", "phone-1"), "phone-1");

        let mut shared = Shared::default();
        let first = shared.record_received("phone-1", "Pixel", Path::new("/tmp/a/old.txt"));
        assert_eq!(first, "Received old.txt from Pixel");
        let second = shared.record_received("phone-1", "Pixel", Path::new("/tmp/a/new.txt"));
        assert_eq!(second, "Received new.txt from Pixel");
        assert_eq!(shared.received[0].name, "new.txt");
        assert_eq!(shared.received[1].name, "old.txt");
        assert_eq!(
            shared.received[0].path,
            Path::new("/tmp/a/new.txt").to_string_lossy()
        );
        assert_eq!(shared.received[0].phone, "phone-1");
        assert_eq!(shared.received[0].phone_name, "Pixel");

        for i in 0..(super::KEEP_RECEIVED + 3) {
            shared.record_received("phone-1", "Pixel", Path::new(&format!("/tmp/n{i}.txt")));
        }
        assert_eq!(shared.received.len(), super::KEEP_RECEIVED);
        assert_eq!(
            shared.received[0].name,
            format!("n{}.txt", super::KEEP_RECEIVED + 2)
        );
    }

    #[test]
    fn file_views_are_on_link_status_for_the_window() {
        let link = Link {
            server: None,
            shared: Arc::new(Mutex::new(Shared::default())),
        };
        let empty = serde_json::to_value(link.status()).unwrap();
        assert!(empty["sends"].as_array().unwrap().is_empty());
        assert!(empty["received"].as_array().unwrap().is_empty());

        let id = "cd".repeat(16);
        {
            let mut shared = link.shared.lock().unwrap();
            shared.note_send_progress("phone-a", &id, "photo.jpg", 3, 10);
            shared.record_received("phone-a", "Pixel", Path::new("/tmp/seam-dl/photo.jpg"));
        }
        let mid = serde_json::to_value(link.status()).unwrap();
        assert!(mid["sends"][0].get("error").is_none());
        assert_eq!(mid["sends"][0]["state"], "sending");
        assert_eq!(mid["sends"][0]["sent"], 3);
        assert_eq!(mid["sends"][0]["size"], 10);

        link.shared.lock().unwrap().note_send_finished(
            "phone-a",
            &id,
            "photo.jpg",
            false,
            Some("cancelled".into()),
        );
        let json = serde_json::to_value(link.status()).unwrap();
        let send = &json["sends"][0];
        assert_eq!(send["phone"], "phone-a");
        assert_eq!(send["name"], "photo.jpg");
        assert_eq!(send["state"], "failed");
        assert_eq!(send["error"], "cancelled");
        assert_eq!(send["transfer"], id);
        let file = &json["received"][0];
        assert_eq!(file["name"], "photo.jpg");
        assert_eq!(file["phone_name"], "Pixel");
        assert_eq!(file["phone"], "phone-a");
        assert_eq!(
            file["path"],
            Path::new("/tmp/seam-dl/photo.jpg")
                .to_string_lossy()
                .as_ref()
        );
    }

    #[test]
    fn drop_plan_json_uses_the_action_the_window_reads() {
        let ask = DropPlan::Ask {
            phones: vec![DropPhone {
                id: "a".into(),
                name: "Pixel".into(),
            }],
            paths: vec!["/tmp/a.txt".into()],
        };
        let json = serde_json::to_value(&ask).unwrap();
        assert_eq!(json["action"], "ask");
        assert_eq!(json["phones"][0]["id"], "a");
        assert_eq!(json["phones"][0]["name"], "Pixel");
        assert_eq!(json["paths"][0], "/tmp/a.txt");

        let err = DropPlan::Error {
            message: "no phone is connected".into(),
        };
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["action"], "error");
        assert_eq!(json["message"], "no phone is connected");

        let sent = DropPlan::Sent {
            phone: "Pixel".into(),
        };
        let json = serde_json::to_value(&sent).unwrap();
        assert_eq!(json["action"], "sent");
        assert_eq!(json["phone"], "Pixel");
    }

    #[test]
    fn drop_goes_to_the_only_phone_and_asks_when_several_are_connected() {
        let offline = Link {
            server: None,
            shared: Arc::new(Mutex::new(Shared::default())),
        };
        match offline.deliver_drop(&[]).unwrap() {
            DropPlan::Error { message } => assert_eq!(message, "no file chosen"),
            other => panic!("expected error, got {other:?}"),
        }
        match offline.deliver_drop(&[String::new()]).unwrap() {
            DropPlan::Error { message } => assert_eq!(message, "no file chosen"),
            other => panic!("expected error, got {other:?}"),
        }
        match offline
            .deliver_drop(&[std::env::temp_dir().to_string_lossy().into_owned()])
            .unwrap()
        {
            DropPlan::Error { message } => assert_eq!(message, "not a file"),
            other => panic!("expected not a file, got {other:?}"),
        }

        let disconnected = link_with(vec![phone_view("a", "Pixel", false)]);
        match disconnected
            .deliver_drop(&["/tmp/seam-no-such-file.txt".into()])
            .unwrap()
        {
            DropPlan::Error { message } => assert_eq!(message, "not a file"),
            other => panic!("expected not a file, got {other:?}"),
        }

        let dir = std::env::temp_dir().join(format!("seam-drop-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("note.txt");
        std::fs::write(&file, b"hi").unwrap();
        let file_s = file.to_string_lossy().into_owned();

        let one = link_with(vec![
            phone_view("a", "Pixel", true),
            phone_view("b", "Galaxy", false),
        ]);
        // The disconnected phone is not a target, so this tries to send and hits the offline link.
        assert_eq!(
            one.deliver_drop(&[file_s.clone(), dir.to_string_lossy().into_owned()])
                .unwrap_err(),
            "the phone link is not running"
        );

        let two = link_with(vec![
            phone_view("b", "Pixel", true),
            phone_view("a", "Galaxy", true),
        ]);
        match two
            .deliver_drop(&[file_s.clone(), dir.to_string_lossy().into_owned()])
            .unwrap()
        {
            DropPlan::Ask { phones, paths } => {
                assert_eq!(phones[0].name, "Galaxy");
                assert_eq!(phones[0].id, "a");
                assert_eq!(phones[1].name, "Pixel");
                assert_eq!(paths, vec![file_s]);
            }
            other => panic!("expected ask, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn queued_files_are_not_hidden_by_a_later_failure() {
        // The command returns Sent (Ok) when any file was queued, so the window
        // does not treat a partial multi-file drop as a total failure.
        assert!(super::classify_send_batch(2, Some("could not open the file".into())).is_ok());
        assert!(super::classify_send_batch(1, None).is_ok());
        assert_eq!(
            super::classify_send_batch(0, Some("not a file".into())).unwrap_err(),
            "not a file"
        );
        assert_eq!(
            super::classify_send_batch(0, None).unwrap_err(),
            "no file chosen"
        );
    }

    #[test]
    fn a_send_that_never_starts_is_listed_as_failed() {
        let offline = Link {
            server: None,
            shared: Arc::new(Mutex::new(Shared::default())),
        };
        let err = offline
            .send_file("phone-1", "/tmp/photo (1).jpg")
            .unwrap_err();
        assert_eq!(err, "the phone link is not running");
        let sends = offline.status().sends;
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].name, "photo (1).jpg");
        assert_eq!(sends[0].state, "failed");
        assert_eq!(
            sends[0].error.as_deref(),
            Some("the phone link is not running")
        );
        assert!(sends[0].transfer.starts_with("local-"));
        assert_eq!(
            offline.send_file("phone-1", "").unwrap_err(),
            "no file chosen"
        );
        assert_eq!(offline.status().sends.len(), 1);
    }

    #[test]
    fn send_file_requires_a_connected_phone_and_cancel_reports_unknown() {
        let offline = Link {
            server: None,
            shared: Arc::new(Mutex::new(Shared::default())),
        };
        assert_eq!(
            offline.send_file("phone-1", "").unwrap_err(),
            "no file chosen"
        );
        assert_eq!(
            offline.cancel_send("ab").unwrap_err(),
            "the phone link is not running"
        );

        let dir = std::env::temp_dir().join(format!("seam-send-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (server, _events) = LinkServer::new(&dir, "Desk".into()).unwrap();
        let link = Link {
            server: Some(server),
            shared: Arc::new(Mutex::new(Shared::default())),
        };
        let path = dir.join("note.txt");
        std::fs::write(&path, b"hi").unwrap();
        assert_eq!(
            link.send_file("phone-1", &path.to_string_lossy())
                .unwrap_err(),
            "this phone is not connected"
        );
        assert_eq!(
            link.cancel_send(&"ab".repeat(16)).unwrap_err(),
            "this transfer is not running"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reveal_only_lists_received_files_and_reports_missing() {
        let link = Link {
            server: None,
            shared: Arc::new(Mutex::new(Shared::default())),
        };
        assert_eq!(link.reveal_received("").unwrap_err(), "unknown file");
        assert_eq!(
            link.reveal_received("/etc/passwd").unwrap_err(),
            "unknown file"
        );

        let missing = std::env::temp_dir().join(format!(
            "seam-missing-reveal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&missing);
        assert!(!missing.exists());
        link.shared
            .lock()
            .unwrap()
            .record_received("phone-1", "Pixel", &missing);
        let stored = link.shared.lock().unwrap().received[0].path.clone();
        assert_eq!(link.reveal_received(&stored).unwrap_err(), "file is gone");
        assert_eq!(
            link.reveal_received(&(stored.clone() + ".nope"))
                .unwrap_err(),
            "unknown file"
        );
        assert_eq!(
            link.reveal_received("/etc/passwd").unwrap_err(),
            "unknown file"
        );
    }

    #[test]
    fn dropped_paths_skip_blank_entries() {
        let paths = [
            PathBuf::from("/tmp/a.txt"),
            PathBuf::from(""),
            PathBuf::from("/tmp/b.txt"),
        ];
        assert_eq!(
            super::dropped_path_list(&paths),
            vec!["/tmp/a.txt".to_string(), "/tmp/b.txt".to_string()]
        );
        assert!(super::dropped_path_list(&[]).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn file_uri_encodes_spaces_and_keeps_slashes() {
        assert_eq!(
            super::file_uri(Path::new("/tmp/a b.txt")),
            "file:///tmp/a%20b.txt"
        );
        let nested = Path::new("/tmp/seam").join("photo (1).jpg");
        let uri = super::file_uri(&nested);
        assert!(uri.starts_with("file://"));
        assert!(!uri.contains(' '));
        assert!(uri.contains("photo"));
        assert!(uri.contains("%28"));
    }

    #[test]
    fn windows_select_arg_is_a_raw_quoted_switch() {
        // Must be passed with raw_arg. Command::arg would wrap this whole token
        // in another pair of quotes and Explorer would miss /select.
        assert_eq!(
            super::windows_select_arg(Path::new("photo (1).jpg")),
            r#"/select,"photo (1).jpg""#
        );
        let nested = Path::new("Ada Lovelace").join("photo (1).jpg");
        let arg = super::windows_select_arg(&nested);
        assert!(arg.starts_with("/select,\""));
        assert!(arg.ends_with('"'));
        assert!(!arg.starts_with('"'));
        assert!(arg.contains("Ada Lovelace"));
        assert!(arg.contains("photo (1).jpg"));
    }

    #[test]
    fn linux_show_items_quotes_the_uri_and_bounds_the_wait() {
        let uri = "file:///home/ada/photo%20(1).jpg";
        let args = super::linux_show_items_args(uri);
        assert!(args
            .iter()
            .any(|arg| { arg == r#"array:string:"file:///home/ada/photo%20(1).jpg""# }));
        assert!(args.iter().any(|arg| arg == r#"string:"""#));
        assert!(args.iter().any(|arg| arg == "--print-reply"));
        assert!(args.iter().any(|arg| {
            arg.strip_prefix("--reply-timeout=")
                .and_then(|ms| ms.parse::<u64>().ok())
                .is_some_and(|ms| ms > 0 && ms <= 5_000)
        }));
        assert!(!args
            .iter()
            .any(|arg| arg == "string:" || arg.starts_with("array:string:file:")));
        let path = Path::new("/tmp/seam").join("photo (1).jpg");
        assert_eq!(super::folder_to_open(&path), path.parent().unwrap());
        assert_eq!(
            super::macos_reveal_args(&path),
            vec!["-R".to_string(), path.display().to_string()]
        );
    }
}
