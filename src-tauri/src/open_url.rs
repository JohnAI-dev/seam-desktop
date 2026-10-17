//! Confirm-before-open for links a phone sends, and sending links back.
//!
//! A received `open_url` is never opened here by itself. On Linux, notify-rust
//! opens it when the person clicks the notification. On Windows and macOS a
//! click does nothing; the Links list is the only way in.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, Emitter, Listener};
use tauri_plugin_notification::NotificationExt;

/// How many click-watching notifications may be outstanding at once.
/// Matches the stored-link cap so a phone cannot leak a thread per `open_url`.
const MAX_WATCHERS: usize = 30;

struct PendingOpen {
    url: String,
}

fn pending() -> &'static Mutex<HashMap<i32, PendingOpen>> {
    static PENDING: OnceLock<Mutex<HashMap<i32, PendingOpen>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_id() -> i32 {
    static NEXT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Show a notification for each incoming link and tell the window to refresh.
///
/// The listener returns immediately. Click handling is registered once and does
/// not block a thread per link.
pub fn install(app: &AppHandle) {
    install_click_handler(app);
    let app = app.clone();
    seam_core::link::on_incoming_link(move |link| {
        let _ = app.emit("open-url", &link);
        let title = seam_core::link::notification_title(&link.phone_name);
        confirm_notification(&app, title, link.host, link.url);
    });
}

/// Open a stored link. Called only from the Links list.
#[tauri::command]
pub fn open_link(id: String) -> Result<(), String> {
    let link = seam_core::link::lookup_incoming_link(&id)?;
    open_in_browser(&link.url)
}

/// Validate and send `open_url` to a connected phone.
#[tauri::command]
pub fn send_open_url(phone: String, url: String) -> Result<(), String> {
    seam_core::link::dispatch_open_url(&phone, &url)
}

/// Links waiting for the person to open them, newest first.
#[tauri::command]
pub fn pending_links() -> Vec<seam_core::link::IncomingLink> {
    seam_core::link::all_incoming_links()
}

fn open_in_browser(url: &str) -> Result<(), String> {
    let target = seam_core::link::url_to_open(url).map_err(|e| e.to_string())?;
    // Shell open (xdg-open / open / start), the same job as tauri-plugin-opener.
    // Never called except from a click handler or the Links list.
    open::that_detached(&target).map_err(|e| format!("could not open the link: {e}"))
}

/// Listen for `plugin:notification://action-performed`.
///
/// tauri-plugin-notification 2.4.0 never sends `action-performed` on desktop.
/// On Windows and macOS a click does nothing. The Links list is the only way in.
fn install_click_handler(app: &AppHandle) {
    let app = app.clone();
    // This is not a cross-platform click path. The desktop plugin does not emit
    // the event, so the closure never runs on Linux, macOS, or Windows.
    let _ = app.listen("plugin:notification://action-performed", move |event| {
        let id = action_notification_id(event.payload());
        let Some(id) = id else {
            return;
        };
        let url = pending().lock().unwrap().remove(&id).map(|p| p.url);
        if let Some(url) = url {
            if let Err(e) = open_in_browser(&url) {
                eprintln!("Seam: could not open link: {e}");
            }
        }
    });
}

fn action_notification_id(payload: &str) -> Option<i32> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    value
        .get("notificationId")
        .or_else(|| value.get("notification_id"))
        .or_else(|| value.get("id"))
        .and_then(|v| v.as_i64())
        .and_then(|n| i32::try_from(n).ok())
}

fn confirm_notification(app: &AppHandle, title: String, host: String, url: String) {
    // Linux opens the link from notify-rust's `wait_for_action`. macOS and
    // Windows stay on the plugin notification: `close` does not compile on
    // those handles. A click there does nothing. The Links list is the only
    // way in.
    #[cfg(all(unix, not(target_os = "macos")))]
    if show_actionable(app, &title, &host, url.clone()) {
        return;
    }
    show_plugin_notification(app, title, host, url);
}

/// Show via notify-rust and watch for the default action on a capped thread.
/// Returns false when the notification could not be shown. The caller then
/// shows a plugin notification, which never reports a click on desktop, so
/// the Links list is how that link gets opened.
///
/// Linux only: `all(unix, not(target_os = "macos"))`. `close` does not compile
/// on the macOS or Windows handle.
#[cfg(all(unix, not(target_os = "macos")))]
fn show_actionable(app: &AppHandle, title: &str, host: &str, url: String) -> bool {
    let mut note = notify_rust::Notification::new();
    note.summary(title)
        .body(host)
        .timeout(notify_rust::Timeout::Never)
        .action("default", "Open");
    let handle = match note.show() {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("Seam: could not show link notification: {e}");
            return false;
        }
    };
    if !reserve_watcher() {
        // Still shown, but we will not block a new thread forever. The Links
        // list remains the way to open it. Close this one so it does not linger
        // without a watcher.
        handle.close();
        return false;
    }
    let app = app.clone();
    std::thread::Builder::new()
        .name("seam-link-notify".into())
        .spawn(move || {
            let _slot = WatcherGuard;
            // `wait_for_action` returns when the person clicks or dismisses.
            // Only the default / Open action opens the browser.
            handle.wait_for_action(move |action| {
                if action == "default" || action.eq_ignore_ascii_case("open") {
                    if let Err(e) = open_in_browser(&url) {
                        eprintln!("Seam: could not open link: {e}");
                    }
                    let _ = app.emit("open-url-opened", ());
                }
            });
        })
        .is_ok()
}

fn show_plugin_notification(app: &AppHandle, title: String, host: String, url: String) {
    let id = next_id();
    {
        let mut map = pending().lock().unwrap();
        if map.len() >= MAX_WATCHERS {
            // Drop the oldest id so a flood cannot grow without bound.
            if let Some(oldest) = map.keys().copied().min() {
                map.remove(&oldest);
            }
        }
        map.insert(id, PendingOpen { url });
    }
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        // The id is stored with the URL. tauri-plugin-notification 2.4.0 never
        // sends `action-performed` on desktop, so a click does not open it.
        let _ = app2
            .notification()
            .builder()
            .title(title)
            .body(host)
            .id(id)
            .show();
    });
}

#[cfg(all(unix, not(target_os = "macos")))]
fn watcher_count() -> &'static Mutex<usize> {
    static COUNT: OnceLock<Mutex<usize>> = OnceLock::new();
    COUNT.get_or_init(|| Mutex::new(0))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn reserve_watcher() -> bool {
    let mut n = watcher_count().lock().unwrap();
    if *n >= MAX_WATCHERS {
        return false;
    }
    *n += 1;
    true
}

#[cfg(all(unix, not(target_os = "macos")))]
struct WatcherGuard;

#[cfg(all(unix, not(target_os = "macos")))]
impl Drop for WatcherGuard {
    fn drop(&mut self) {
        let mut n = watcher_count().lock().unwrap();
        *n = n.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_payload_id_is_read_from_either_field() {
        assert_eq!(action_notification_id(r#"{"notificationId":7}"#), Some(7));
        assert_eq!(action_notification_id(r#"{"notification_id":3}"#), Some(3));
        assert_eq!(action_notification_id(r#"{"id":9}"#), Some(9));
        assert_eq!(action_notification_id("not json"), None);
        assert_eq!(action_notification_id("{}"), None);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn watcher_cap_stops_a_flood() {
        *watcher_count().lock().unwrap() = 0;
        let mut held = Vec::new();
        for _ in 0..MAX_WATCHERS {
            assert!(reserve_watcher());
            held.push(WatcherGuard);
        }
        assert!(!reserve_watcher());
        drop(held);
        assert!(reserve_watcher());
        *watcher_count().lock().unwrap() = 0;
    }
}
