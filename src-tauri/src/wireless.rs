//! Wireless debugging pairing in the window: QR code, a two-minute wait, and
//! automatic `adb connect` for phones that were paired before.

use seam_core::tools::{self, Tool};
use seam_core::{link, wireless};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

struct Inner {
    generation: u64,
    active: bool,
    qr_svg: Option<String>,
    message: Option<String>,
    error: Option<String>,
    just_paired: bool,
    remembered: Vec<wireless::RememberedPhone>,
    store_path: Option<PathBuf>,
}

/// App state for cable-free mirroring. Discovery keeps running in the background.
pub struct Wireless {
    inner: Arc<Mutex<Inner>>,
    services: Arc<Mutex<Vec<wireless::DiscoveredService>>>,
    adb_ops: Arc<Mutex<()>>,
    app: AppHandle,
}

#[derive(Serialize)]
pub struct WirelessStatus {
    active: bool,
    qr_svg: Option<String>,
    message: Option<String>,
    error: Option<String>,
    /// True once, on the status read that first observes a successful pair.
    paired: bool,
}

#[derive(Serialize)]
pub struct WirelessPairingView {
    qr_svg: String,
    expires_in_secs: u64,
}

struct PairingJob {
    app: AppHandle,
    inner: Arc<Mutex<Inner>>,
    services: Arc<Mutex<Vec<wireless::DiscoveredService>>>,
    adb_ops: Arc<Mutex<()>>,
    generation: u64,
    creds: wireless::PairingCredentials,
    adb: PathBuf,
}

struct ConnectAttempt {
    at: Instant,
    succeeded: bool,
}

/// Start discovery and the later-session connect loop. Never fails the app.
pub fn start(app: &AppHandle) -> Wireless {
    let services = Arc::new(Mutex::new(Vec::new()));
    wireless::spawn_discovery(services.clone());
    let store_path = app
        .path()
        .app_local_data_dir()
        .ok()
        .map(|dir| dir.join("wireless.json"));
    let remembered = store_path
        .as_ref()
        .map(|path| wireless::load_remembered(path))
        .unwrap_or_default();
    let inner = Arc::new(Mutex::new(Inner {
        generation: 0,
        active: false,
        qr_svg: None,
        message: None,
        error: None,
        just_paired: false,
        remembered,
        store_path,
    }));
    let adb_ops = Arc::new(Mutex::new(()));
    let auto_inner = inner.clone();
    let auto_services = services.clone();
    let auto_ops = adb_ops.clone();
    let spawned = thread::Builder::new()
        .name("seam-wireless-connect".into())
        .spawn(move || auto_connect_loop(auto_inner, auto_services, auto_ops));
    if let Err(e) = spawned {
        eprintln!("Seam: could not start wireless auto-connect: {e}");
    }
    Wireless {
        inner,
        services,
        adb_ops,
        app: app.clone(),
    }
}

impl Wireless {
    pub fn status(&self) -> WirelessStatus {
        let mut guard = self.inner.lock().unwrap();
        let paired = guard.just_paired;
        guard.just_paired = false;
        WirelessStatus {
            active: guard.active,
            qr_svg: guard.qr_svg.clone(),
            message: guard.message.clone(),
            error: guard.error.clone(),
            paired,
        }
    }

    /// Show a fresh QR code and wait up to two minutes for the phone to scan it.
    pub fn start_pairing(&self) -> Result<WirelessPairingView, String> {
        let adb = tools::find(Tool::Adb).ok_or("adb is not available")?;
        if link::local_ip().is_none() {
            return Err("this computer is not on a network; connect to Wi-Fi and try again".into());
        }
        let creds = wireless::generate_pairing_credentials();
        let payload = wireless::pairing_qr_payload(&creds.service_name, &creds.password);
        let qr_svg = link::qr_svg(&payload)?;
        let generation = {
            let mut guard = self.inner.lock().unwrap();
            guard.generation = guard.generation.wrapping_add(1);
            guard.active = true;
            guard.qr_svg = Some(qr_svg.clone());
            guard.message = Some("Waiting for the phone to scan the QR code...".into());
            guard.error = None;
            guard.just_paired = false;
            guard.generation
        };
        let job = PairingJob {
            app: self.app.clone(),
            inner: self.inner.clone(),
            services: self.services.clone(),
            adb_ops: self.adb_ops.clone(),
            generation,
            creds,
            adb,
        };
        let spawned = thread::Builder::new()
            .name("seam-wireless-pair".into())
            .spawn(move || pairing_worker(job));
        if let Err(e) = spawned {
            self.cancel();
            return Err(format!("could not start pairing: {e}"));
        }
        notify(&self.app);
        Ok(WirelessPairingView {
            qr_svg,
            expires_in_secs: wireless::PAIRING_TIMEOUT.as_secs(),
        })
    }

    pub fn cancel(&self) {
        {
            let mut guard = self.inner.lock().unwrap();
            guard.generation = guard.generation.wrapping_add(1);
            guard.active = false;
            guard.qr_svg = None;
            guard.message = None;
            guard.error = None;
            guard.just_paired = false;
        }
        notify(&self.app);
    }
}

fn notify(app: &AppHandle) {
    let _ = app.emit("wireless-pairing", ());
}

fn stale(inner: &Mutex<Inner>, generation: u64) -> bool {
    inner.lock().unwrap().generation != generation
}

fn set_message(app: &AppHandle, inner: &Mutex<Inner>, generation: u64, message: &str) {
    let changed = {
        let mut guard = inner.lock().unwrap();
        if guard.generation != generation {
            return;
        }
        if guard.message.as_deref() == Some(message) {
            false
        } else {
            guard.message = Some(message.to_string());
            true
        }
    };
    if changed {
        notify(app);
    }
}

fn finish_ok(app: &AppHandle, inner: &Mutex<Inner>, generation: u64, message: &str) {
    let should_notify = {
        let mut guard = inner.lock().unwrap();
        if guard.generation != generation {
            false
        } else {
            guard.generation = guard.generation.wrapping_add(1);
            guard.active = false;
            guard.qr_svg = None;
            guard.error = None;
            guard.message = Some(message.to_string());
            guard.just_paired = true;
            true
        }
    };
    if should_notify {
        notify(app);
    }
}

fn finish_err(app: &AppHandle, inner: &Mutex<Inner>, generation: u64, message: &str) {
    let should_notify = {
        let mut guard = inner.lock().unwrap();
        if guard.generation != generation {
            false
        } else {
            guard.generation = guard.generation.wrapping_add(1);
            guard.active = false;
            guard.qr_svg = None;
            guard.message = None;
            guard.error = Some(message.to_string());
            guard.just_paired = false;
            true
        }
    };
    if should_notify {
        notify(app);
    }
}

fn remember_connect(inner: &Mutex<Inner>, generation: u64, instance: &str, address: &str) {
    let to_save = {
        let mut guard = inner.lock().unwrap();
        if guard.generation != generation {
            return;
        }
        wireless::upsert_remembered(&mut guard.remembered, instance, address);
        guard
            .store_path
            .clone()
            .map(|path| (path, guard.remembered.clone()))
    };
    if let Some((path, phones)) = to_save {
        if let Err(e) = wireless::save_remembered(&path, &phones) {
            eprintln!("Seam: could not remember this phone: {e}");
        }
    }
}

fn observe(
    shared: &Mutex<Vec<wireless::DiscoveredService>>,
    adb: &Path,
    last_adb: &mut Option<Instant>,
) -> Vec<wireless::DiscoveredService> {
    let mut current = shared.lock().unwrap().clone();
    let due = match *last_adb {
        None => true,
        Some(then) => then.elapsed() >= Duration::from_secs(2),
    };
    if !due {
        return current;
    }
    *last_adb = Some(Instant::now());
    if let Ok(extra) = wireless::query_adb_mdns(adb) {
        wireless::merge_services(&mut current, &extra);
    }
    current
}

fn pairing_worker(job: PairingJob) {
    let deadline = Instant::now() + wireless::PAIRING_TIMEOUT;
    set_message(
        &job.app,
        &job.inner,
        job.generation,
        "Waiting for the phone to scan the QR code...",
    );
    let mut paired_host: Option<String> = None;
    let mut saw_pairing = false;
    let mut last_pair_error = String::new();
    let mut last_adb_poll = None;
    let mut last_pair_try: Option<Instant> = None;
    let mut last_connect_try: Option<Instant> = None;

    while Instant::now() < deadline {
        if stale(&job.inner, job.generation) {
            return;
        }
        let found = observe(&job.services, &job.adb, &mut last_adb_poll);
        if paired_host.is_none() {
            if let Some(svc) = wireless::match_pairing_service(&found, &job.creds.service_name) {
                saw_pairing = true;
                let pair_due = match last_pair_try {
                    None => true,
                    Some(then) => then.elapsed() >= Duration::from_secs(2),
                };
                if pair_due {
                    last_pair_try = Some(Instant::now());
                    set_message(
                        &job.app,
                        &job.inner,
                        job.generation,
                        "Pairing with the phone...",
                    );
                    let address = svc.address.clone();
                    let port = svc.port;
                    let pair_result = {
                        let _guard = job.adb_ops.lock().unwrap();
                        wireless::adb_pair(&job.adb, &address, port, &job.creds.password)
                    };
                    if stale(&job.inner, job.generation) {
                        return;
                    }
                    match pair_result {
                        Ok(()) => {
                            paired_host = Some(address);
                            set_message(
                                &job.app,
                                &job.inner,
                                job.generation,
                                "Paired. Connecting...",
                            );
                        }
                        Err(e) => last_pair_error = e,
                    }
                }
            }
        }
        if let Some(host) = paired_host.as_deref() {
            let connect_due = match last_connect_try {
                None => true,
                Some(then) => then.elapsed() >= Duration::from_secs(2),
            };
            if connect_due {
                if let Some(svc) = wireless::connect_service_on_host(&found, host) {
                    last_connect_try = Some(Instant::now());
                    set_message(
                        &job.app,
                        &job.inner,
                        job.generation,
                        "Paired. Connecting...",
                    );
                    let instance = svc.instance.clone();
                    let address = svc.address.clone();
                    let port = svc.port;
                    let connect_result = {
                        let _guard = job.adb_ops.lock().unwrap();
                        wireless::adb_connect(&job.adb, &address, port)
                    };
                    if stale(&job.inner, job.generation) {
                        return;
                    }
                    if connect_result.is_ok() {
                        remember_connect(&job.inner, job.generation, &instance, &address);
                        finish_ok(
                            &job.app,
                            &job.inner,
                            job.generation,
                            "Connected. You can Mirror without the cable.",
                        );
                        return;
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(400));
    }

    if stale(&job.inner, job.generation) {
        return;
    }
    let message = if paired_host.is_some() {
        wireless::CONNECT_TIMEOUT_MESSAGE.to_string()
    } else if saw_pairing {
        if last_pair_error.is_empty() {
            "Found the phone, but pairing failed. Turn Wireless debugging off and on, then try again.".to_string()
        } else {
            format!(
                "Found the phone, but pairing failed: {last_pair_error}. Turn Wireless debugging off and on, then try again."
            )
        }
    } else {
        wireless::PAIRING_TIMEOUT_MESSAGE.to_string()
    };
    finish_err(&job.app, &job.inner, job.generation, &message);
}

fn auto_connect_loop(
    inner: Arc<Mutex<Inner>>,
    services: Arc<Mutex<Vec<wireless::DiscoveredService>>>,
    adb_ops: Arc<Mutex<()>>,
) {
    let mut attempts: HashMap<String, ConnectAttempt> = HashMap::new();
    loop {
        thread::sleep(Duration::from_secs(3));
        let remembered = {
            let guard = inner.lock().unwrap();
            if guard.active || guard.remembered.is_empty() {
                continue;
            }
            guard.remembered.clone()
        };
        let Some(adb) = tools::find(Tool::Adb) else {
            continue;
        };
        let found = services.lock().unwrap().clone();
        let ready = {
            let _guard = adb_ops.lock().unwrap();
            wireless::ready_serials(&adb)
        };
        for svc in wireless::select_auto_connects(&found, &remembered, &ready) {
            let endpoint = wireless::adb_endpoint(&svc.address, svc.port);
            if let Some(prev) = attempts.get(&endpoint) {
                let wait = if prev.succeeded {
                    Duration::from_secs(60)
                } else {
                    Duration::from_secs(20)
                };
                if prev.at.elapsed() < wait {
                    continue;
                }
            }
            let ok = {
                let _guard = adb_ops.lock().unwrap();
                wireless::adb_connect(&adb, &svc.address, svc.port).is_ok()
            };
            if ok {
                eprintln!("Seam: connected {} over Wi-Fi", svc.instance);
            }
            attempts.insert(
                endpoint,
                ConnectAttempt {
                    at: Instant::now(),
                    succeeded: ok,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WirelessPairingView, WirelessStatus};

    #[test]
    fn status_json_uses_the_fields_the_window_reads() {
        let status = WirelessStatus {
            active: true,
            qr_svg: Some("<svg></svg>".into()),
            message: Some("Waiting for the phone to scan the QR code...".into()),
            error: None,
            paired: false,
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["active"], true);
        assert_eq!(json["qr_svg"], "<svg></svg>");
        assert_eq!(
            json["message"],
            "Waiting for the phone to scan the QR code..."
        );
        assert!(json["error"].is_null());
        assert_eq!(json["paired"], false);

        let view = WirelessPairingView {
            qr_svg: "<svg></svg>".into(),
            expires_in_secs: 120,
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["qr_svg"], "<svg></svg>");
        assert_eq!(json["expires_in_secs"], 120);
    }
}
