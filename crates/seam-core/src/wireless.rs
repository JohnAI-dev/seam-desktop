//! Wireless adb pairing (Android 11+ Wireless debugging).
//!
//! The computer shows a QR code `WIFI:T:ADB;S:seam-<random>;P:<10 digits>;;`.
//! The phone scans it and advertises `_adb-tls-pairing._tcp` under that service
//! name. Seam runs `adb pair`, then `adb connect` to `_adb-tls-connect._tcp`.
//! A remembered phone that later advertises the connect service is connected
//! automatically. Discovery and adb run outside the pure helpers; unit tests
//! never touch the network.

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use rand::{Rng, RngCore};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long one QR pairing attempt waits for the phone.
pub const PAIRING_TIMEOUT: Duration = Duration::from_secs(120);

/// Shown when the phone never appears.
pub const PAIRING_TIMEOUT_MESSAGE: &str = "Timed out after 2 minutes. On the phone, open Settings \u{2192} Developer options \u{2192} Wireless debugging, tap \"Pair device with QR code\", and scan the code.";

/// Shown when pairing worked but the connect service never accepted `adb connect`.
pub const CONNECT_TIMEOUT_MESSAGE: &str = "Paired, but could not connect to the phone. Turn Wireless debugging off and on, then try again.";

/// mDNS type the phone advertises while the QR scanner is open.
pub const PAIRING_TYPE: &str = "_adb-tls-pairing._tcp";
/// mDNS type the phone advertises for later `adb connect` sessions.
pub const CONNECT_TYPE: &str = "_adb-tls-connect._tcp";
/// Browse name for the pairing service, including the local domain.
pub const PAIRING_SERVICE: &str = "_adb-tls-pairing._tcp.local.";
/// Browse name for the connect service, including the local domain.
pub const CONNECT_SERVICE: &str = "_adb-tls-connect._tcp.local.";

const ADB_MDNS_INTERVAL: Duration = Duration::from_secs(4);
const ADB_MDNS_TIMEOUT: Duration = Duration::from_secs(3);
const ADB_PAIR_TIMEOUT: Duration = Duration::from_secs(15);
const ADB_CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const DISCOVERY_TICK: Duration = Duration::from_millis(200);

/// Service name and pairing code encoded in the QR code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCredentials {
    /// `seam-` plus random hex. This is the mDNS instance name.
    pub service_name: String,
    /// Ten decimal digits, possibly with leading zeros.
    pub password: String,
}

/// One resolved wireless-debugging service. Pure helpers match on this; nothing here opens a socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredService {
    /// Instance label, or a full DNS-SD name (the label is taken from the first segment).
    pub instance: String,
    /// `_adb-tls-pairing._tcp` or `_adb-tls-connect._tcp`, with or without `.local`.
    pub service_type: String,
    /// Numeric address, without a port and without brackets.
    pub address: String,
    pub port: u16,
}

/// A phone that has completed wireless pairing, remembered for later sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RememberedPhone {
    /// Instance name of `_adb-tls-connect._tcp`.
    pub instance: String,
    /// Last address we connected to. Not used for matching (DHCP changes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// When pairing happened, milliseconds since the epoch.
    #[serde(default)]
    pub paired_at: i64,
}

#[derive(Clone)]
struct CacheEntry {
    service: DiscoveredService,
    fullname: String,
    from_adb: bool,
}

/// `WIFI:T:ADB;S:<service name>;P:<password>;;`
///
/// `service_name` and `password` must not contain `;`, `:`, `\` or `,` (the generator never does).
pub fn pairing_qr_payload(service_name: &str, password: &str) -> String {
    format!("WIFI:T:ADB;S:{service_name};P:{password};;")
}

/// Split a payload from [`pairing_qr_payload`] back into service name and password.
pub fn parse_pairing_qr(payload: &str) -> Option<(String, String)> {
    let rest = payload.strip_prefix("WIFI:T:ADB;S:")?;
    let (name, rest) = rest.split_once(";P:")?;
    let password = rest.strip_suffix(";;")?;
    if name.is_empty() || password.is_empty() || name.contains(';') || password.contains(';') {
        return None;
    }
    Some((name.to_string(), password.to_string()))
}

/// `S` is `seam-` plus 16 hex chars; `P` is 10 digits. Both are safe in the QR and as a DNS label.
pub fn generate_pairing_credentials() -> PairingCredentials {
    let mut raw = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut raw);
    let service_name = format!("seam-{}", hex::encode(raw));
    let password = format!("{:010}", rand::thread_rng().gen_range(0..10_000_000_000u64));
    PairingCredentials {
        service_name,
        password,
    }
}

/// First DNS label of an instance name or a full `name._type._tcp.local` string.
pub fn instance_label(name: &str) -> String {
    name.trim()
        .trim_matches('.')
        .split('.')
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// True when `instance_or_fullname` is exactly `service_name` (case-insensitive), not a prefix.
pub fn instance_matches(instance_or_fullname: &str, service_name: &str) -> bool {
    let wanted = instance_label(service_name);
    if wanted.is_empty() {
        return false;
    }
    instance_label(instance_or_fullname).eq_ignore_ascii_case(&wanted)
}

/// Canonical `_adb-tls-pairing._tcp` / `_adb-tls-connect._tcp`, or `None` for anything else.
pub fn normalize_service_type(service_type: &str) -> Option<&'static str> {
    let normalized = service_type
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if normalized.contains(PAIRING_TYPE) {
        Some(PAIRING_TYPE)
    } else if normalized.contains(CONNECT_TYPE) {
        Some(CONNECT_TYPE)
    } else {
        None
    }
}

pub fn is_pairing_type(service_type: &str) -> bool {
    normalize_service_type(service_type) == Some(PAIRING_TYPE)
}

pub fn is_connect_type(service_type: &str) -> bool {
    normalize_service_type(service_type) == Some(CONNECT_TYPE)
}

/// The pairing service whose instance name is the generated `S` value.
///
/// A connect service that happens to use the same label is not a match: only
/// `_adb-tls-pairing._tcp` is the service the phone opens for the QR scan.
pub fn match_pairing_service<'a>(
    services: &'a [DiscoveredService],
    service_name: &str,
) -> Option<&'a DiscoveredService> {
    services.iter().find(|svc| {
        is_pairing_type(&svc.service_type)
            && svc.port != 0
            && instance_matches(&svc.instance, service_name)
    })
}

/// Every discovered service whose instance label equals `service_name`.
pub fn matching_services<'a>(
    services: &'a [DiscoveredService],
    service_name: &str,
) -> Vec<&'a DiscoveredService> {
    services
        .iter()
        .filter(|svc| instance_matches(&svc.instance, service_name))
        .collect()
}

/// `_adb-tls-connect._tcp` advertised by the phone we just paired (same address).
pub fn connect_service_on_host<'a>(
    services: &'a [DiscoveredService],
    host: &str,
) -> Option<&'a DiscoveredService> {
    services.iter().find(|svc| {
        is_connect_type(&svc.service_type) && svc.port != 0 && same_host(&svc.address, host)
    })
}

/// Connect services for remembered phones that are not already a ready adb device.
pub fn select_auto_connects(
    services: &[DiscoveredService],
    remembered: &[RememberedPhone],
    ready_serials: &[String],
) -> Vec<DiscoveredService> {
    let mut chosen = Vec::new();
    for phone in remembered {
        let Some(svc) = services.iter().find(|svc| {
            is_connect_type(&svc.service_type)
                && svc.port != 0
                && instance_matches(&svc.instance, &phone.instance)
        }) else {
            continue;
        };
        if ready_serials
            .iter()
            .any(|serial| serial_matches(serial, svc))
        {
            continue;
        }
        if chosen
            .iter()
            .any(|have: &DiscoveredService| have.address == svc.address && have.port == svc.port)
        {
            continue;
        }
        chosen.push(svc.clone());
    }
    chosen
}

fn serial_matches(serial: &str, svc: &DiscoveredService) -> bool {
    let endpoint = adb_endpoint(&svc.address, svc.port);
    if serial == endpoint {
        return true;
    }
    let label = instance_label(&svc.instance);
    if label.is_empty() {
        return false;
    }
    if let Some(rest) = serial.strip_prefix(&label) {
        if rest.is_empty() || rest.starts_with('.') {
            return true;
        }
    }
    false
}

fn same_host(a: &str, b: &str) -> bool {
    match (a.parse::<IpAddr>(), b.parse::<IpAddr>()) {
        (Ok(left), Ok(right)) => left == right,
        _ => !a.is_empty() && a.eq_ignore_ascii_case(b),
    }
}

fn same_service(a: &DiscoveredService, b: &DiscoveredService) -> bool {
    instance_matches(&a.instance, &b.instance)
        && normalize_service_type(&a.service_type) == normalize_service_type(&b.service_type)
}

/// Add wireless-debugging services that are not already present. Existing rows
/// are left alone so a slower `adb mdns` snapshot cannot overwrite a fresher
/// mDNS address. Other service types are ignored.
pub fn merge_services(base: &mut Vec<DiscoveredService>, extra: &[DiscoveredService]) {
    for svc in extra {
        if svc.port == 0
            || svc.address.is_empty()
            || normalize_service_type(&svc.service_type).is_none()
        {
            continue;
        }
        if !base.iter().any(|have| same_service(have, svc)) {
            base.push(svc.clone());
        }
    }
}

/// `ip:port`, or `[ipv6]:port` when the address contains a colon.
pub fn adb_endpoint(address: &str, port: u16) -> String {
    if address.starts_with('[') {
        format!("{address}:{port}")
    } else if address.contains(':') {
        format!("[{address}]:{port}")
    } else {
        format!("{address}:{port}")
    }
}

/// Prefer a normal IPv4 address. Link-local and loopback addresses are skipped when another exists.
pub fn preferred_address(addrs: impl IntoIterator<Item = IpAddr>) -> Option<IpAddr> {
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    for ip in addrs {
        match ip {
            IpAddr::V4(v) if usable_v4(v) => v4.push(ip),
            IpAddr::V6(v) if usable_v6(v) => v6.push(ip),
            _ => {}
        }
    }
    v4.sort();
    v6.sort();
    v4.into_iter().next().or_else(|| v6.into_iter().next())
}

fn usable_v4(ip: Ipv4Addr) -> bool {
    !ip.is_loopback()
        && !ip.is_unspecified()
        && !ip.is_multicast()
        && !ip.is_broadcast()
        && !is_v4_link_local(ip)
}

fn is_v4_link_local(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 169 && o[1] == 254
}

fn usable_v6(ip: Ipv6Addr) -> bool {
    !ip.is_loopback() && !ip.is_unspecified() && !ip.is_multicast() && !is_v6_link_local(&ip)
}

fn is_v6_link_local(ip: &Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

/// Parse `adb mdns services`. Lines that are not a wireless-debugging service are ignored.
pub fn parse_adb_mdns(output: &str) -> Vec<DiscoveredService> {
    output.lines().filter_map(parse_adb_mdns_line).collect()
}

fn parse_adb_mdns_line(line: &str) -> Option<DiscoveredService> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('*') || line.starts_with("List of") {
        return None;
    }
    let (instance, service_type, hostport) = split_mdns_columns(line)?;
    let service_type = normalize_service_type(service_type)?;
    let (address, port) = parse_host_port(hostport)?;
    let instance = instance_label(instance);
    if instance.is_empty() {
        return None;
    }
    Some(DiscoveredService {
        instance,
        service_type: service_type.to_string(),
        address,
        port,
    })
}

fn split_mdns_columns(line: &str) -> Option<(&str, &str, &str)> {
    if line.contains('\t') {
        let mut parts = line
            .split('\t')
            .map(str::trim)
            .filter(|part| !part.is_empty());
        return Some((parts.next()?, parts.next()?, parts.next()?));
    }
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() != 3 || normalize_service_type(parts[1]).is_none() {
        return None;
    }
    Some((parts[0], parts[1], parts[2]))
}

fn parse_host_port(text: &str) -> Option<(String, u16)> {
    let text = text.trim();
    let (host, port_text) = if let Some(rest) = text.strip_prefix('[') {
        let (host, rest) = rest.split_once("]:")?;
        (host, rest)
    } else {
        let (host, port_text) = text.rsplit_once(':')?;
        if host.is_empty() || host.contains(':') {
            return None;
        }
        (host, port_text)
    };
    let port: u16 = port_text.parse().ok()?;
    if port == 0 {
        return None;
    }
    let ip: IpAddr = host.parse().ok()?;
    Some((ip.to_string(), port))
}

/// Insert or update a remembered connect-service name. Empty names are ignored.
pub fn upsert_remembered(phones: &mut Vec<RememberedPhone>, instance: &str, address: &str) {
    let instance = instance_label(instance);
    if instance.is_empty() {
        return;
    }
    if let Some(phone) = phones
        .iter_mut()
        .find(|phone| instance_matches(&phone.instance, &instance))
    {
        phone.instance = instance;
        phone.address = Some(address.to_string());
        return;
    }
    phones.push(RememberedPhone {
        instance,
        address: Some(address.to_string()),
        paired_at: now_ms(),
    });
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Load remembered phones. A missing or unreadable file is an empty list, not an error.
pub fn load_remembered(path: &Path) -> Vec<RememberedPhone> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    let phones: Vec<RememberedPhone> = serde_json::from_slice(&bytes).unwrap_or_default();
    phones
        .into_iter()
        .filter_map(|mut phone| {
            let instance = instance_label(&phone.instance);
            if instance.is_empty() {
                None
            } else {
                phone.instance = instance;
                Some(phone)
            }
        })
        .collect()
}

/// Write remembered phones. The parent directory is created if needed.
pub fn save_remembered(path: &Path, phones: &[RememberedPhone]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
    }
    let json = serde_json::to_vec_pretty(phones).map_err(|e| e.to_string())?;
    fs::write(path, json).map_err(|e| e.to_string())
}

/// Serials of phones that are connected and authorized.
pub fn ready_serials(adb: &Path) -> Vec<String> {
    crate::adb::list_devices(adb)
        .unwrap_or_default()
        .into_iter()
        .filter(|device| device.is_ready())
        .map(|device| device.serial)
        .collect()
}

/// `adb pair <ip>:<port> <password>` using the built-in adb.
pub fn adb_pair(adb: &Path, address: &str, port: u16, password: &str) -> Result<(), String> {
    if address.parse::<IpAddr>().is_err() || port == 0 {
        return Err("invalid phone address".into());
    }
    if password.len() != 10 || !password.chars().all(|c| c.is_ascii_digit()) {
        return Err("invalid pairing code".into());
    }
    let endpoint = adb_endpoint(address, port);
    let (ok, text) = run_adb(adb, &["pair", &endpoint, password], ADB_PAIR_TIMEOUT)?;
    if pair_output_ok(ok, &text) {
        Ok(())
    } else {
        Err(brief_failure(&text, "pairing failed"))
    }
}

/// `adb connect <ip>:<port>`.
pub fn adb_connect(adb: &Path, address: &str, port: u16) -> Result<(), String> {
    if address.parse::<IpAddr>().is_err() || port == 0 {
        return Err("invalid phone address".into());
    }
    let endpoint = adb_endpoint(address, port);
    let (_ok, text) = run_adb(adb, &["connect", &endpoint], ADB_CONNECT_TIMEOUT)?;
    if connect_output_ok(&text) {
        Ok(())
    } else {
        Err(brief_failure(&text, "could not connect"))
    }
}

/// `adb mdns services`, parsed. Failure (including timeout) is returned; an empty network is `Ok`.
pub fn query_adb_mdns(adb: &Path) -> Result<Vec<DiscoveredService>, String> {
    let (ok, text) = run_adb(adb, &["mdns", "services"], ADB_MDNS_TIMEOUT)?;
    if !ok && !text.to_ascii_lowercase().contains("list of") {
        return Err(brief_failure(&text, "adb mdns services failed"));
    }
    Ok(parse_adb_mdns(&text))
}

fn pair_output_ok(exit_ok: bool, text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("failed") || lower.contains("error") {
        return false;
    }
    exit_ok && lower.contains("paired")
}

fn connect_output_ok(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("failed") || lower.contains("error") || lower.contains("cannot") {
        return false;
    }
    lower.contains("connected to") || lower.contains("already connected")
}

fn brief_failure(text: &str, fallback: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty() && !line.starts_with('*') && !line.starts_with("List of"))
        .unwrap_or(fallback);
    let brief: String = line.chars().take(180).collect();
    if brief.is_empty() {
        fallback.to_string()
    } else {
        brief
    }
}

fn run_adb(program: &Path, args: &[&str], timeout: Duration) -> Result<(bool, String), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run adb: {e}"))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let reader = thread::spawn(move || {
        let mut out = String::new();
        if let Some(mut pipe) = stdout {
            let _ = pipe.read_to_string(&mut out);
        }
        if let Some(mut pipe) = stderr {
            let _ = pipe.read_to_string(&mut out);
        }
        out
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let text = reader.join().unwrap_or_default();
                return Ok((status.success(), text));
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err("adb timed out".into());
            }
            Ok(None) => thread::sleep(Duration::from_millis(40)),
            Err(e) => {
                let _ = child.kill();
                let _ = reader.join();
                return Err(format!("could not run adb: {e}"));
            }
        }
    }
}

/// Browse `_adb-tls-pairing._tcp` and `_adb-tls-connect._tcp`, falling back to `adb mdns services`.
///
/// The thread keeps updating `services` until the process exits. A failure to start mDNS
/// does not stop the adb fallback, and this function never panics on a missing network.
pub fn spawn_discovery(services: Arc<Mutex<Vec<DiscoveredService>>>) {
    let spawned = thread::Builder::new()
        .name("seam-wireless-mdns".into())
        .spawn(move || discovery_main(services));
    if let Err(e) = spawned {
        eprintln!("Seam: could not start wireless discovery: {e}");
    }
}

fn discovery_main(services: Arc<Mutex<Vec<DiscoveredService>>>) {
    // The phone link also binds mDNS. Give that advertiser a head start.
    thread::sleep(Duration::from_millis(500));
    if let Err(e) = run_mdns(&services) {
        eprintln!("Seam: wireless mDNS unavailable ({e}); using adb mdns");
        run_adb_only(&services);
    }
}

fn run_mdns(services: &Mutex<Vec<DiscoveredService>>) -> Result<(), String> {
    let daemon = ServiceDaemon::new().map_err(|e| e.to_string())?;
    let pairing_rx = daemon.browse(PAIRING_SERVICE).map_err(|e| e.to_string())?;
    let connect_rx = daemon.browse(CONNECT_SERVICE).map_err(|e| e.to_string())?;
    let mut cache = Vec::new();
    let mut next_adb = Instant::now();
    let mut adb_warned = false;
    loop {
        let mut changed = false;
        changed |= drain(&pairing_rx, &mut cache);
        changed |= drain(&connect_rx, &mut cache);
        if Instant::now() >= next_adb {
            next_adb = Instant::now() + ADB_MDNS_INTERVAL;
            changed |= refresh_adb(&mut cache, &mut adb_warned);
        }
        if changed {
            publish(services, &cache);
        }
        thread::sleep(DISCOVERY_TICK);
    }
}

fn run_adb_only(services: &Mutex<Vec<DiscoveredService>>) {
    let mut cache = Vec::new();
    let mut warned = false;
    loop {
        if refresh_adb(&mut cache, &mut warned) {
            publish(services, &cache);
        }
        thread::sleep(ADB_MDNS_INTERVAL);
    }
}

fn drain(rx: &mdns_sd::Receiver<ServiceEvent>, cache: &mut Vec<CacheEntry>) -> bool {
    let mut changed = false;
    while let Ok(event) = rx.try_recv() {
        changed |= apply_event(cache, event);
    }
    changed
}

fn apply_event(cache: &mut Vec<CacheEntry>, event: ServiceEvent) -> bool {
    match event {
        ServiceEvent::ServiceResolved(info) => upsert_resolved(cache, &info),
        ServiceEvent::ServiceRemoved(a, b) => remove_named(cache, &a, &b),
        _ => false,
    }
}

fn upsert_resolved(cache: &mut Vec<CacheEntry>, info: &ServiceInfo) -> bool {
    let port = info.get_port();
    if port == 0 {
        return false;
    }
    let Some(address) = preferred_address(info.get_addresses().iter().copied()) else {
        return false;
    };
    let Some(service_type) = normalize_service_type(info.get_type()) else {
        return false;
    };
    let fullname = info.get_fullname().to_string();
    let instance = instance_label(&fullname);
    if instance.is_empty() {
        return false;
    }
    let service = DiscoveredService {
        instance,
        service_type: service_type.to_string(),
        address: address.to_string(),
        port,
    };
    if let Some(existing) = cache
        .iter_mut()
        .find(|entry| same_service(&entry.service, &service))
    {
        let changed = existing.service != service || existing.fullname != fullname;
        existing.service = service;
        existing.fullname = fullname;
        existing.from_adb = false;
        return changed;
    }
    cache.push(CacheEntry {
        service,
        fullname,
        from_adb: false,
    });
    true
}

fn remove_named(cache: &mut Vec<CacheEntry>, a: &str, b: &str) -> bool {
    let before = cache.len();
    cache.retain(|entry| {
        if entry.from_adb {
            return true;
        }
        !same_fullname(&entry.fullname, a) && !same_fullname(&entry.fullname, b)
    });
    cache.len() != before
}

fn same_fullname(stored: &str, event: &str) -> bool {
    let stored = stored.trim().trim_end_matches('.');
    let event = event.trim().trim_end_matches('.');
    !stored.is_empty() && stored.eq_ignore_ascii_case(event)
}

fn refresh_adb(cache: &mut Vec<CacheEntry>, warned: &mut bool) -> bool {
    let Some(adb) = crate::tools::find(crate::tools::Tool::Adb) else {
        return false;
    };
    let found = match query_adb_mdns(&adb) {
        Ok(found) => found,
        Err(e) => {
            if !*warned {
                eprintln!("Seam: adb mdns services failed: {e}");
                *warned = true;
            }
            return false;
        }
    };
    let before = cache.clone();
    cache.retain(|entry| !entry.from_adb);
    for svc in found {
        if cache.iter().any(|entry| same_service(&entry.service, &svc)) {
            continue;
        }
        cache.push(CacheEntry {
            fullname: String::new(),
            from_adb: true,
            service: svc,
        });
    }
    cache_changed(&before, cache)
}

fn cache_changed(before: &[CacheEntry], after: &[CacheEntry]) -> bool {
    if before.len() != after.len() {
        return true;
    }
    before
        .iter()
        .zip(after.iter())
        .any(|(left, right)| left.service != right.service || left.from_adb != right.from_adb)
}

fn publish(services: &Mutex<Vec<DiscoveredService>>, cache: &[CacheEntry]) {
    let list = cache.iter().map(|entry| entry.service.clone()).collect();
    *services.lock().unwrap() = list;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn svc(instance: &str, service_type: &str, address: &str, port: u16) -> DiscoveredService {
        DiscoveredService {
            instance: instance.into(),
            service_type: service_type.into(),
            address: address.into(),
            port,
        }
    }

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "seam-wireless-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn builds_the_wireless_debugging_qr_payload() {
        assert_eq!(
            pairing_qr_payload("seam-abc123", "0123456789"),
            "WIFI:T:ADB;S:seam-abc123;P:0123456789;;"
        );
        assert_eq!(
            parse_pairing_qr("WIFI:T:ADB;S:seam-abc123;P:0123456789;;").unwrap(),
            ("seam-abc123".into(), "0123456789".into())
        );
        assert!(parse_pairing_qr("WIFI:T:ADB;S:seam-abc123;P:0123456789;").is_none());
        assert!(parse_pairing_qr("https://example.invalid").is_none());
        assert!(parse_pairing_qr("WIFI:T:ADB;S:;P:0123456789;;").is_none());
    }

    #[test]
    fn generated_credentials_are_a_seam_name_and_a_10_digit_code() {
        let first = generate_pairing_credentials();
        let second = generate_pairing_credentials();
        assert!(first.service_name.starts_with("seam-"));
        let suffix = first.service_name.strip_prefix("seam-").unwrap();
        assert_eq!(suffix.len(), 16);
        assert!(first.service_name.len() <= 63);
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!first.service_name.contains(';') && !first.service_name.contains(':'));
        assert_eq!(first.password.len(), 10);
        assert!(first.password.chars().all(|c| c.is_ascii_digit()));
        assert!(!first.password.contains(';'));
        assert_ne!(first.service_name, second.service_name);

        let payload = pairing_qr_payload(&first.service_name, &first.password);
        assert_eq!(
            parse_pairing_qr(&payload).unwrap(),
            (first.service_name.clone(), first.password.clone())
        );
        let svg = crate::link::qr_svg(&payload).unwrap();
        assert!(svg.contains("<svg"));
    }

    #[test]
    fn matches_discovered_services_to_the_generated_service_name() {
        let creds = generate_pairing_credentials();
        let payload = pairing_qr_payload(&creds.service_name, &creds.password);
        let (name, password) = parse_pairing_qr(&payload).unwrap();
        assert_eq!(name, creds.service_name);
        assert_eq!(password, creds.password);

        let services = vec![
            svc("other-phone", "_adb-tls-pairing._tcp", "192.168.1.9", 1),
            svc(
                &format!("{name}._adb-tls-pairing._tcp.local."),
                "_adb-tls-pairing._tcp.local.",
                "192.168.1.20",
                37123,
            ),
            svc(&name, "_adb-tls-connect._tcp", "192.168.1.20", 42123),
            svc("seam-deadbeef", PAIRING_TYPE, "10.0.0.2", 9),
        ];
        let found = match_pairing_service(&services, &creds.service_name).unwrap();
        assert_eq!(found.address, "192.168.1.20");
        assert_eq!(found.port, 37123);
        assert!(is_pairing_type(&found.service_type));

        // The generated name must not match a different phone, a prefix, or the connect service.
        assert!(match_pairing_service(&services, "seam-nope").is_none());
        assert!(match_pairing_service(&services, "seam-").is_none());
        assert!(match_pairing_service(&[], &creds.service_name).is_none());
        assert!(match_pairing_service(&services, "").is_none());
        let prefix = &creds.service_name[..creds.service_name.len() - 1];
        assert!(match_pairing_service(&services, prefix).is_none());

        let named = matching_services(&services, &creds.service_name);
        assert_eq!(named.len(), 2);
        assert!(named.iter().any(|svc| is_pairing_type(&svc.service_type)));
        assert!(named.iter().any(|svc| is_connect_type(&svc.service_type)));
    }

    #[test]
    fn instance_label_and_type_checks_are_exact() {
        assert_eq!(instance_label("seam-abc"), "seam-abc");
        assert_eq!(
            instance_label("seam-abc._adb-tls-pairing._tcp.local."),
            "seam-abc"
        );
        assert_eq!(instance_label("  .seam-abc.  "), "seam-abc");
        assert_eq!(instance_label(""), "");
        assert_eq!(instance_label("..."), "");
        assert!(instance_matches(
            "Seam-ABC._adb-tls-pairing._tcp.local",
            "seam-abc"
        ));
        assert!(!instance_matches("seam-abcd", "seam-abc"));
        assert!(!instance_matches("seam-ab", "seam-abc"));
        assert!(is_pairing_type("_adb-tls-pairing._tcp.local."));
        assert!(!is_pairing_type("_adb-tls-connect._tcp"));
        assert!(is_connect_type("_adb-tls-connect._tcp.local."));
        assert!(!is_connect_type("_adb-tls-pairing._tcp"));
        assert!(normalize_service_type("_seam._tcp").is_none());
    }

    #[test]
    fn connect_service_is_chosen_by_host_and_remembered_name() {
        let services = vec![
            svc("seam-abc", PAIRING_TYPE, "192.168.1.20", 37123),
            svc("adb-pixel", CONNECT_TYPE, "192.168.1.20", 42123),
            svc("adb-other", "_adb-tls-connect._tcp.local.", "10.0.0.5", 9),
            svc("adb-zero", CONNECT_TYPE, "192.168.1.20", 0),
        ];
        let found = connect_service_on_host(&services, "192.168.1.20").unwrap();
        assert_eq!(found.instance, "adb-pixel");
        assert_eq!(found.port, 42123);
        assert!(connect_service_on_host(&services, "192.168.1.21").is_none());
        // A pairing service on that host is not a connect target.
        assert!(connect_service_on_host(
            &[svc("seam-abc", PAIRING_TYPE, "192.168.1.20", 37123)],
            "192.168.1.20"
        )
        .is_none());

        let remembered = vec![RememberedPhone {
            instance: "adb-pixel._adb-tls-connect._tcp.local.".into(),
            address: None,
            paired_at: 1,
        }];
        let ready = vec!["192.168.1.20:42123".to_string()];
        assert!(select_auto_connects(&services, &remembered, &ready).is_empty());
        let targets = select_auto_connects(&services, &remembered, &[]);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].port, 42123);

        let by_instance = vec!["adb-pixel._adb-tls-connect._tcp".to_string()];
        assert!(select_auto_connects(&services, &remembered, &by_instance).is_empty());
        // A remembered connect name must not select a pairing service.
        assert!(select_auto_connects(
            &[svc("adb-pixel", PAIRING_TYPE, "192.168.1.20", 1)],
            &remembered,
            &[]
        )
        .is_empty());
    }

    #[test]
    fn parses_adb_mdns_services_and_ignores_noise() {
        let output = "\
* daemon not running; starting now at tcp:5037
* daemon started successfully
List of discovered mdns services
seam-abc123\t_adb-tls-pairing._tcp\t192.168.1.20:37123
adb-pixel\t_adb-tls-connect._tcp\t192.168.1.20:42123
not a service line
seam-space _adb-tls-pairing._tcp 10.0.0.8:9
[2001:db8::1]:5555 _adb-tls-connect._tcp nope
";
        let found = parse_adb_mdns(output);
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].instance, "seam-abc123");
        assert_eq!(found[0].service_type, PAIRING_TYPE);
        assert_eq!(found[0].address, "192.168.1.20");
        assert_eq!(found[0].port, 37123);
        assert_eq!(found[1].instance, "adb-pixel");
        assert_eq!(found[1].port, 42123);
        assert!(is_connect_type(&found[1].service_type));
        assert_eq!(found[2].address, "10.0.0.8");
        assert_eq!(found[2].port, 9);
        assert!(parse_adb_mdns("").is_empty());
        assert!(parse_adb_mdns("List of discovered mdns services\n").is_empty());
    }

    #[test]
    fn merge_keeps_an_existing_service_and_adds_a_new_one() {
        let mut base = vec![svc("adb-pixel", CONNECT_TYPE, "192.168.1.20", 1)];
        merge_services(
            &mut base,
            &[
                svc("adb-pixel", "_adb-tls-connect._tcp.local.", "10.0.0.1", 2),
                svc("seam-new", PAIRING_TYPE, "192.168.1.20", 3),
                svc("nope", "_http._tcp", "192.168.1.20", 4),
                svc("zero", CONNECT_TYPE, "192.168.1.20", 0),
            ],
        );
        assert_eq!(base.len(), 2);
        assert_eq!(base[0].port, 1, "an existing mDNS row is not overwritten");
        assert_eq!(base[1].instance, "seam-new");
    }

    #[test]
    fn preferred_address_skips_link_local_and_prefers_ipv4() {
        let link_local = "169.254.1.1".parse().unwrap();
        let v6 = "2001:db8::1".parse().unwrap();
        let v4 = "192.168.1.20".parse().unwrap();
        assert_eq!(preferred_address([link_local, v6, v4]), Some(v4));
        assert_eq!(preferred_address([link_local]), None);
        assert_eq!(
            preferred_address(["fe80::1".parse().unwrap(), v6]),
            Some(v6)
        );
        assert_eq!(adb_endpoint("192.168.1.20", 5555), "192.168.1.20:5555");
        assert_eq!(adb_endpoint("2001:db8::1", 5555), "[2001:db8::1]:5555");
        assert_eq!(adb_endpoint("[2001:db8::1]", 5555), "[2001:db8::1]:5555");
    }

    #[test]
    fn remembered_phones_round_trip_and_update_in_place() {
        let mut phones = Vec::new();
        upsert_remembered(
            &mut phones,
            "adb-pixel._adb-tls-connect._tcp.local.",
            "192.168.1.20",
        );
        upsert_remembered(&mut phones, "adb-pixel", "10.0.0.5");
        upsert_remembered(&mut phones, "", "10.0.0.5");
        upsert_remembered(&mut phones, "adb-other", "10.0.0.6");
        assert_eq!(phones.len(), 2);
        assert_eq!(phones[0].instance, "adb-pixel");
        assert_eq!(phones[0].address.as_deref(), Some("10.0.0.5"));
        assert!(phones[0].paired_at >= 0);

        let dir = scratch("remember");
        let path = dir.join("wireless.json");
        assert!(load_remembered(&path).is_empty());
        save_remembered(&path, &phones).unwrap();
        assert_eq!(load_remembered(&path), phones);
        fs::write(&path, b"not json").unwrap();
        assert!(load_remembered(&path).is_empty());
        fs::write(&path, b"[{\"instance\":\"\",\"paired_at\":1}]").unwrap();
        assert!(load_remembered(&path).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pairing_timeout_is_two_minutes_with_a_clear_message() {
        assert_eq!(PAIRING_TIMEOUT, Duration::from_secs(120));
        assert!(PAIRING_TIMEOUT_MESSAGE.contains("2 minutes"));
        assert!(PAIRING_TIMEOUT_MESSAGE
            .to_ascii_lowercase()
            .contains("wireless debugging"));
        assert!(PAIRING_TIMEOUT_MESSAGE.contains("Pair device with QR code"));
        assert!(!CONNECT_TIMEOUT_MESSAGE.is_empty());
    }

    #[test]
    fn adb_result_text_is_classified_without_running_adb() {
        assert!(pair_output_ok(
            true,
            "Successfully paired to 192.168.1.20:37123 [guid=adb-1]\n"
        ));
        assert!(!pair_output_ok(true, "Failed: connection refused\n"));
        assert!(!pair_output_ok(false, "Successfully paired to 1.2.3.4:1\n"));
        assert!(connect_output_ok("connected to 192.168.1.20:42123\n"));
        assert!(connect_output_ok(
            "already connected to 192.168.1.20:42123\n"
        ));
        assert!(!connect_output_ok(
            "failed to connect to 192.168.1.20:42123\n"
        ));
        assert!(!connect_output_ok(""));
        assert!(!connect_output_ok("cannot connect to 192.168.1.20:1\n"));

        let noisy = "\
* daemon not running; starting now at tcp:5037
* daemon started successfully
Failed: pairing rejected
";
        assert_eq!(
            brief_failure(noisy, "pairing failed"),
            "Failed: pairing rejected"
        );
        assert_eq!(brief_failure("\n", "pairing failed"), "pairing failed");
        assert_eq!(brief_failure("", "could not connect"), "could not connect");
    }
}
