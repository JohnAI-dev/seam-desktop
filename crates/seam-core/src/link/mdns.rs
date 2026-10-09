//! Advertise this computer on the local network (mDNS/DNS-SD) so a paired phone
//! can find it after the computer's IP address changes. See `protocol/PROTOCOL.md`.

use mdns_sd::{ServiceDaemon, ServiceInfo};

/// Service type phones browse for: `_seam._tcp` on the local domain.
pub const SERVICE_TYPE: &str = "_seam._tcp.local.";

/// DNS-SD instance names are a single label, at most 63 bytes.
const MAX_INSTANCE_BYTES: usize = 63;

/// TXT key carrying the desktop certificate fingerprint.
const TXT_FP: &str = "fp";

/// The service registered on the local network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceDescription {
    /// `_seam._tcp.local.`
    pub service_type: &'static str,
    /// Desktop name, sanitized for use as an instance name.
    pub instance_name: String,
    /// Link port.
    pub port: u16,
    /// Value of the TXT `fp` record (certificate fingerprint).
    pub txt_fp: String,
}

impl ServiceDescription {
    /// TXT record `fp=<fingerprint>`.
    pub fn txt_record(&self) -> String {
        format!("{TXT_FP}={}", self.txt_fp)
    }
}

/// Build the service description for `desktop_name` listening on `port`.
pub fn service_description(desktop_name: &str, port: u16, fingerprint: &str) -> ServiceDescription {
    ServiceDescription {
        service_type: SERVICE_TYPE,
        instance_name: sanitize_instance_name(desktop_name),
        port,
        txt_fp: fingerprint.to_string(),
    }
}

/// Reduce `name` to letters, digits, spaces and hyphens, at most 63 bytes.
///
/// Other characters are dropped. A name that is empty after sanitizing becomes
/// `"Seam"`.
pub fn sanitize_instance_name(name: &str) -> String {
    let filtered: String = name.chars().filter(|c| is_instance_char(*c)).collect();
    let trimmed = filtered.trim();
    let mut out = String::new();
    for c in trimmed.chars() {
        if out.len() + c.len_utf8() > MAX_INSTANCE_BYTES {
            break;
        }
        out.push(c);
    }
    let out = out.trim();
    if out.is_empty() {
        "Seam".to_string()
    } else {
        out.to_string()
    }
}

fn is_instance_char(c: char) -> bool {
    c.is_alphanumeric() || c == ' ' || c == '-'
}

/// Hostname published with the service. Addresses come from `enable_addr_auto`,
/// not from this name.
fn hostname_for(instance: &str) -> String {
    let mut label = String::new();
    for c in instance.chars() {
        if label.len() >= MAX_INSTANCE_BYTES {
            break;
        }
        if c.is_ascii_alphanumeric() {
            label.push(c.to_ascii_lowercase());
        } else if !label.is_empty() && !label.ends_with('-') {
            label.push('-');
        }
    }
    let label = label.trim_matches('-');
    let label = if label.is_empty() { "seam" } else { label };
    format!("{label}.local.")
}

fn build_service_info(desc: &ServiceDescription) -> Result<ServiceInfo, String> {
    let host = hostname_for(&desc.instance_name);
    let properties = [(TXT_FP, desc.txt_fp.as_str())];
    // An empty address list lets mdns-sd fill in the computer's current local IPs.
    ServiceInfo::new(
        desc.service_type,
        &desc.instance_name,
        &host,
        "",
        desc.port,
        &properties[..],
    )
    .map(|info| info.enable_addr_auto())
    .map_err(|e| e.to_string())
}

/// Register `desc` on the local network.
///
/// The returned value must be kept alive; dropping it stops advertising.
/// Addresses are filled in by mdns-sd (`enable_addr_auto`), so a new DHCP lease
/// is picked up without a restart.
pub fn advertise(desc: &ServiceDescription) -> Result<Advertiser, String> {
    let daemon = ServiceDaemon::new().map_err(|e| e.to_string())?;
    let info = build_service_info(desc)?;
    // mdns-sd 0.13 queues the service and returns `()`.
    daemon.register(info).map_err(|e| e.to_string())?;
    Ok(Advertiser { _daemon: daemon })
}

/// Keeps an mDNS advertisement registered. Dropping it stops advertising.
#[must_use = "dropping the advertiser stops advertising"]
pub struct Advertiser {
    _daemon: ServiceDaemon,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_service_description() {
        let fp = "ab".repeat(32);
        let desc = service_description("Ada's Laptop #1", 47100, &fp);
        assert_eq!(
            desc,
            ServiceDescription {
                service_type: "_seam._tcp.local.",
                instance_name: "Adas Laptop 1".into(),
                port: 47100,
                txt_fp: fp.clone(),
            }
        );
        assert_eq!(desc.txt_record(), format!("fp={fp}"));
        assert_eq!(desc.service_type, SERVICE_TYPE);
    }

    #[test]
    fn protocol_example_desktop() {
        let fp = "abababababababababababababababababababababababababababababababab";
        let desc = service_description("Johns MacBook Air", 47100, fp);
        assert_eq!(desc.service_type, "_seam._tcp.local.");
        assert_eq!(desc.instance_name, "Johns MacBook Air");
        assert_eq!(desc.port, 47100);
        assert_eq!(desc.txt_fp, fp);
        assert_eq!(desc.txt_record(), format!("fp={fp}"));
    }

    #[test]
    fn description_sanitizes_a_long_name() {
        let desc = service_description(&"n".repeat(80), 47109, "abcd");
        assert_eq!(desc.instance_name, "n".repeat(63));
        assert_eq!(desc.port, 47109);
        assert_eq!(desc.txt_fp, "abcd");
        assert_eq!(desc.txt_record(), "fp=abcd");
    }

    #[test]
    fn keeps_letters_digits_spaces_and_hyphens() {
        assert_eq!(sanitize_instance_name("Seam Desktop"), "Seam Desktop");
        assert_eq!(sanitize_instance_name("PC-01"), "PC-01");
        assert_eq!(sanitize_instance_name("a b-c 1"), "a b-c 1");
        assert_eq!(sanitize_instance_name("  Seam Desktop  "), "Seam Desktop");
        assert_eq!(sanitize_instance_name(" -hello- "), "-hello-");
        assert_eq!(sanitize_instance_name("a  b"), "a  b");
        assert_eq!(sanitize_instance_name("Jøhns PC"), "Jøhns PC");
    }

    #[test]
    fn strips_other_characters() {
        assert_eq!(sanitize_instance_name("John's MacBook!"), "Johns MacBook");
        assert_eq!(sanitize_instance_name("hello_world"), "helloworld");
        assert_eq!(sanitize_instance_name("a.b"), "ab");
        assert_eq!(sanitize_instance_name("a!b"), "ab");
    }

    #[test]
    fn empty_names_fall_back_to_seam() {
        assert_eq!(sanitize_instance_name(""), "Seam");
        assert_eq!(sanitize_instance_name("!!!"), "Seam");
        assert_eq!(sanitize_instance_name("   "), "Seam");
        assert_eq!(sanitize_instance_name("...___"), "Seam");
        assert_eq!(sanitize_instance_name("@@@"), "Seam");
    }

    #[test]
    fn truncates_to_63_bytes_without_splitting_utf8() {
        let exact = "a".repeat(63);
        assert_eq!(sanitize_instance_name(&exact), exact);

        let too_long = "b".repeat(64);
        assert_eq!(sanitize_instance_name(&too_long), "b".repeat(63));
        assert_eq!(sanitize_instance_name(&too_long).len(), 63);

        // '你' is 3 bytes; 21 of them fit, the 22nd would exceed 63.
        let han = "你".repeat(22);
        let got = sanitize_instance_name(&han);
        assert_eq!(got, "你".repeat(21));
        assert_eq!(got.len(), 63);

        // A trailing multibyte character that would cross the limit is dropped whole.
        let mixed = format!("{}\u{4f60}", "a".repeat(62));
        assert_eq!(sanitize_instance_name(&mixed), "a".repeat(62));
    }

    #[test]
    fn truncation_does_not_leave_surrounding_spaces() {
        let name = format!("  {}  ", "c".repeat(70));
        let got = sanitize_instance_name(&name);
        assert_eq!(got.len(), 63);
        assert_eq!(got, "c".repeat(63));
        assert_eq!(got, got.trim());
    }

    #[test]
    fn hostname_is_a_single_ascii_local_label() {
        assert_eq!(
            hostname_for("Johns MacBook Air"),
            "johns-macbook-air.local."
        );
        assert_eq!(hostname_for("Seam"), "seam.local.");
        assert_eq!(hostname_for("PC-01"), "pc-01.local.");
        assert_eq!(hostname_for("A--B  C"), "a-b-c.local.");
        assert_eq!(hostname_for("电脑"), "seam.local.");
        assert_eq!(hostname_for("!!!"), "seam.local.");
        let host = hostname_for(&"A".repeat(80));
        let label = host.strip_suffix(".local.").expect("local suffix");
        assert!(label.len() <= 63);
        assert!(!label.is_empty());
    }

    #[test]
    fn mdns_sd_accepts_the_service_description() {
        let fp = "cd".repeat(32);
        let desc = service_description("Johns MacBook Air", 47100, &fp);
        build_service_info(&desc).expect("mdns-sd should accept the Seam service");
    }
}
