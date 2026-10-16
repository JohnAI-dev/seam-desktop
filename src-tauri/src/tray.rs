//! Menu bar / system tray icon.
//!
//! Closing the window hides it and leaves the phone link running. Quit, from the
//! tray menu, is what actually exits. On macOS the Dock icon is hidden while the
//! window is hidden. The headless self-test does not create a tray icon: a missing
//! status bar can stall process exit.

use crate::link::{Link, PhoneSummary};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};
use unicode_segmentation::UnicodeSegmentation;

const TRAY_ID: &str = "seam";
/// Tray menus are short. Longer phone names are cut and marked with an ellipsis.
const MAX_PHONE_NAME_CHARS: usize = 40;

/// Set when Quit is chosen, so the window close that follows is allowed to exit.
pub struct RunFlags {
    quitting: AtomicBool,
}

impl Default for RunFlags {
    fn default() -> Self {
        Self {
            quitting: AtomicBool::new(false),
        }
    }
}

struct TrayUi {
    menu: Menu<Wry>,
    autostart: CheckMenuItem<Wry>,
    phones: Mutex<Vec<MenuItem<Wry>>>,
}

/// Text for one phone in the tray menu. `&` is escaped because menus treat it as a mnemonic.
pub fn phone_menu_label(name: &str, connected: bool, battery: Option<(u8, bool)>) -> String {
    let name = cap_phone_name(name).replace('&', "&&");
    let name = if name.is_empty() {
        "Phone".to_string()
    } else {
        name
    };
    if !connected {
        return format!("{name} — Not connected");
    }
    match battery {
        Some((level, true)) => format!("{name} — Connected · {level}% ⚡"),
        Some((level, false)) => format!("{name} — Connected · {level}%"),
        None => format!("{name} — Connected"),
    }
}

/// Keep the phone name short enough for a menu row.
///
/// The limit is grapheme clusters, so a combining character or emoji is not split.
fn cap_phone_name(name: &str) -> String {
    let name = name.trim();
    let mut graphemes = name.graphemes(true);
    let mut capped: String = graphemes.by_ref().take(MAX_PHONE_NAME_CHARS).collect();
    if graphemes.next().is_some() {
        capped = name
            .graphemes(true)
            .take(MAX_PHONE_NAME_CHARS.saturating_sub(1))
            .collect();
        capped.push('…');
    }
    capped
}

/// Closing the window hides it, except during the headless self-test and after Quit.
pub fn close_should_hide(self_test: bool, quitting: bool) -> bool {
    !self_test && !quitting
}

/// `--hidden` is passed by the login entry so a start at login does not open the window.
pub fn wants_hidden_start(args: impl IntoIterator<Item = impl AsRef<str>>) -> bool {
    args.into_iter().any(|arg| arg.as_ref() == "--hidden")
}

/// Hide instead of quitting, except during self-test and after Quit.
pub fn install_window_behavior(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        eprintln!("Seam: main window is missing");
        return;
    };
    let app = app.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            let self_test = app
                .try_state::<crate::SelfTest>()
                .is_some_and(|state| state.0);
            let quitting = app
                .try_state::<RunFlags>()
                .is_some_and(|flags| flags.quitting.load(Ordering::SeqCst));
            if !close_should_hide(self_test, quitting) {
                return;
            }
            api.prevent_close();
            hide_window(&app);
        }
    });
}

/// Create the tray icon and its menu. Failure is reported by the caller; the app still runs.
pub fn install(app: &AppHandle) -> Result<(), String> {
    let show = MenuItem::with_id(app, "show", "Show Seam", true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "Start at login",
        true,
        autostart_enabled(app),
        None::<&str>,
    )
    .map_err(|e| e.to_string())?;
    let quit =
        MenuItem::with_id(app, "quit", "Quit", true, None::<&str>).map_err(|e| e.to_string())?;
    let above_phones = PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?;
    let above_quit = PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?;
    let menu = Menu::with_items(app, &[&above_phones, &show, &autostart, &above_quit, &quit])
        .map_err(|e| e.to_string())?;

    let icon = tray_icon(app)?;
    let _tray = TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .tooltip("Seam")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| on_menu(app, event.id().as_ref()))
        .on_tray_icon_event(|tray, event| {
            if click_shows_window(&event) {
                show_window(tray.app_handle());
            }
        })
        .build(app)
        .map_err(|e| e.to_string())?;

    app.manage(TrayUi {
        menu,
        autostart,
        phones: Mutex::new(Vec::new()),
    });
    // setup already runs on the main thread, where menu changes are safe.
    apply_menu(app);
    Ok(())
}

/// Rebuild the phone rows. Safe to call when no tray was installed (self-test).
pub fn refresh(app: &AppHandle) {
    let app = app.clone();
    let queued = app.clone().run_on_main_thread(move || apply_menu(&app));
    if let Err(e) = queued {
        eprintln!("Seam: could not update the tray menu: {e}");
    }
}

pub fn autostart_enabled(app: &AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// Turn the operating system login entry on or off.
///
/// Uninstalling the app does not remove that entry: this plugin has no uninstall
/// hook. Turning start-at-login off deletes it. The entry is
/// `~/.config/autostart/Seam.desktop` on Linux,
/// `~/Library/LaunchAgents/Seam.plist` on macOS, and the `Seam` value under
/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` on Windows.
pub fn set_autostart(app: &AppHandle, enabled: bool) -> Result<(), String> {
    set_os_autostart(app, enabled)?;
    if let Some(ui) = app.try_state::<TrayUi>() {
        let _ = ui.autostart.set_checked(enabled);
    }
    Ok(())
}

fn set_os_autostart(app: &AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let auto = app.autolaunch();
    if enabled {
        auto.enable().map_err(|e| e.to_string())
    } else {
        auto.disable().map_err(|e| e.to_string())
    }
}

fn quit(app: &AppHandle) {
    if let Some(flags) = app.try_state::<RunFlags>() {
        flags.quitting.store(true, Ordering::SeqCst);
    }
    app.exit(0);
}

fn on_menu(app: &AppHandle, id: &str) {
    match id {
        "show" => show_window(app),
        "quit" => quit(app),
        "autostart" => {
            let want = app
                .try_state::<TrayUi>()
                .and_then(|ui| ui.autostart.is_checked().ok())
                .unwrap_or(false);
            if let Err(e) = set_os_autostart(app, want) {
                eprintln!("Seam: start at login: {e}");
                if let Some(ui) = app.try_state::<TrayUi>() {
                    let _ = ui.autostart.set_checked(!want);
                }
            }
        }
        other if other.starts_with("phone-") => show_window(app),
        _ => {}
    }
}

fn apply_menu(app: &AppHandle) {
    let Some(ui) = app.try_state::<TrayUi>() else {
        return;
    };
    let phones = app
        .try_state::<Link>()
        .map(|link| link.phone_summaries())
        .unwrap_or_default();
    if let Err(e) = replace_phone_items(app, &ui, &phones) {
        eprintln!("Seam: could not update the tray phones: {e}");
    }
    let enabled = autostart_enabled(app);
    if ui.autostart.is_checked().ok() != Some(enabled) {
        let _ = ui.autostart.set_checked(enabled);
    }
}

fn replace_phone_items(
    app: &AppHandle,
    ui: &TrayUi,
    phones: &[PhoneSummary],
) -> Result<(), String> {
    let mut slots = ui.phones.lock().unwrap();
    for item in slots.iter() {
        ui.menu.remove(item).map_err(|e| e.to_string())?;
    }
    slots.clear();
    if phones.is_empty() {
        let item = MenuItem::with_id(app, "phones-empty", "No phone paired", false, None::<&str>)
            .map_err(|e| e.to_string())?;
        ui.menu.insert(&item, 0).map_err(|e| e.to_string())?;
        slots.push(item);
        return Ok(());
    }
    for (index, phone) in phones.iter().enumerate() {
        let label = phone_menu_label(&phone.name, phone.connected, phone.battery);
        let item = MenuItem::with_id(app, format!("phone-{index}"), label, true, None::<&str>)
            .map_err(|e| e.to_string())?;
        ui.menu.insert(&item, index).map_err(|e| e.to_string())?;
        slots.push(item);
    }
    Ok(())
}

pub fn show_window(app: &AppHandle) {
    // Regular before show, so macOS puts the Dock icon back and the window can take focus.
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

pub fn hide_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
    // Accessory hides the Dock icon while Seam stays in the menu bar.
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
}

fn click_shows_window(event: &TrayIconEvent) -> bool {
    matches!(
        event,
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } | TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        }
    )
}

fn tray_icon(app: &AppHandle) -> Result<tauri::image::Image<'_>, String> {
    if let Some(icon) = app.default_window_icon() {
        return Ok(icon.clone());
    }
    tauri::image::Image::from_bytes(include_bytes!("../icons/128x128.png"))
        .map_err(|e| format!("could not load the tray icon: {e}"))
}

#[cfg(test)]
mod tests {
    use super::phone_menu_label;

    #[test]
    fn menu_label_includes_connection_and_battery() {
        assert_eq!(
            phone_menu_label("Pixel", true, Some((82, true))),
            "Pixel — Connected · 82% ⚡"
        );
        assert_eq!(
            phone_menu_label("Pixel", true, Some((10, false))),
            "Pixel — Connected · 10%"
        );
        assert_eq!(phone_menu_label("Pixel", true, None), "Pixel — Connected");
        assert_eq!(
            phone_menu_label("Pixel", false, Some((10, true))),
            "Pixel — Not connected"
        );
        assert_eq!(phone_menu_label("  ", true, None), "Phone — Connected");
        assert_eq!(phone_menu_label("A & B", true, None), "A && B — Connected");
    }

    #[test]
    fn menu_label_caps_a_long_phone_name() {
        use unicode_segmentation::UnicodeSegmentation;
        let name = "Pixel ".repeat(20);
        let label = phone_menu_label(&name, true, Some((50, false)));
        let shown = label.split(" — ").next().expect("name");
        assert_eq!(shown.graphemes(true).count(), super::MAX_PHONE_NAME_CHARS);
        assert!(shown.ends_with('…'));
        assert!(label.ends_with("Connected · 50%"));
    }

    #[test]
    fn menu_label_caps_on_grapheme_boundaries() {
        use unicode_segmentation::UnicodeSegmentation;
        let unit = "e\u{0301}";
        assert!(unit.chars().count() > 1);
        assert_eq!(unit.graphemes(true).count(), 1);

        let kept = unit.repeat(super::MAX_PHONE_NAME_CHARS);
        let label = phone_menu_label(&kept, true, None);
        let shown = label.split(" — ").next().expect("name");
        assert_eq!(shown, kept);

        let cut = unit.repeat(super::MAX_PHONE_NAME_CHARS + 1);
        let label = phone_menu_label(&cut, true, None);
        let shown = label.split(" — ").next().expect("name");
        assert_eq!(
            shown,
            format!("{}…", unit.repeat(super::MAX_PHONE_NAME_CHARS - 1))
        );
        assert_eq!(shown.graphemes(true).count(), super::MAX_PHONE_NAME_CHARS);
    }

    #[test]
    fn close_hides_the_window_until_quit_or_self_test() {
        assert!(super::close_should_hide(false, false));
        assert!(!super::close_should_hide(true, false));
        assert!(!super::close_should_hide(false, true));
        assert!(!super::close_should_hide(true, true));
    }

    #[test]
    fn hidden_argument_starts_without_a_window() {
        assert!(super::wants_hidden_start(["seam-desktop", "--hidden"]));
        assert!(!super::wants_hidden_start(["seam-desktop"]));
        assert!(!super::wants_hidden_start(["seam-desktop", "--self-test"]));
    }

    #[test]
    fn tray_labels_follow_connection_and_battery() {
        let phones = [
            crate::link::PhoneSummary {
                name: "Pixel".into(),
                connected: true,
                battery: Some((82, true)),
            },
            crate::link::PhoneSummary {
                name: "Other".into(),
                connected: false,
                battery: Some((10, false)),
            },
        ];
        let labels: Vec<String> = phones
            .iter()
            .map(|phone| phone_menu_label(&phone.name, phone.connected, phone.battery))
            .collect();
        assert_eq!(
            labels,
            vec![
                "Pixel — Connected · 82% ⚡".to_string(),
                "Other — Not connected".to_string(),
            ]
        );
    }
}
