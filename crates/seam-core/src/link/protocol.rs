//! Messages, pairing links and proofs of the Seam link protocol (see `protocol/PROTOCOL.md`).

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Protocol version spoken by this build.
pub const PROTOCOL_VERSION: u32 = 1;
/// Largest accepted frame (one JSON line), in bytes.
pub const MAX_FRAME: usize = 1024 * 1024;

type HmacSha256 = Hmac<Sha256>;

/// A notification shown on the phone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneNotification {
    pub id: String,
    #[serde(default)]
    pub app: String,
    #[serde(default)]
    pub app_name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub time: i64,
}

/// One protocol frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Challenge {
        v: u32,
        nonce: String,
    },
    Hello {
        device_id: String,
        name: String,
        proof: String,
    },
    Welcome {
        desktop_name: String,
    },
    Error {
        message: String,
    },
    Notification(PhoneNotification),
    NotificationRemoved {
        id: String,
    },
    Battery {
        level: u8,
        charging: bool,
    },
    Ping,
    Pong,
    /// Any message type this build doesn't know. Ignored, so newer phones still work.
    #[serde(other)]
    Unknown,
}

impl Message {
    /// Encode as one line (without the trailing newline).
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("messages always serialize")
    }

    /// Decode one line.
    pub fn from_line(line: &str) -> Result<Self, String> {
        serde_json::from_str(line.trim()).map_err(|e| format!("bad message: {e}"))
    }
}

/// The phone's proof that it holds the pairing key:
/// `hex(HMAC-SHA256(key, "seam-v1|" + nonce + "|" + device_id))`.
pub fn proof(key: &[u8], nonce: &str, device_id: &str) -> String {
    hex::encode(proof_mac(key, nonce, device_id).finalize().into_bytes())
}

/// Check a proof in constant time.
pub fn verify_proof(key: &[u8], nonce: &str, device_id: &str, proof_hex: &str) -> bool {
    match hex::decode(proof_hex) {
        Ok(bytes) => proof_mac(key, nonce, device_id)
            .verify_slice(&bytes)
            .is_ok(),
        Err(_) => false,
    }
}

fn proof_mac(key: &[u8], nonce: &str, device_id: &str) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(format!("seam-v1|{nonce}|{device_id}").as_bytes());
    mac
}

/// SHA-256 fingerprint (lowercase hex) of a DER certificate.
pub fn fingerprint(cert_der: &[u8]) -> String {
    hex::encode(Sha256::digest(cert_der))
}

/// Everything the phone needs to pair, carried in the QR code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingInfo {
    pub hosts: Vec<String>,
    pub port: u16,
    /// SHA-256 of the desktop's certificate, 64 lowercase hex chars.
    pub fingerprint: String,
    pub key: [u8; 32],
    pub name: String,
}

impl PairingInfo {
    /// The `seam://pair?...` link shown as a QR code.
    pub fn to_uri(&self) -> String {
        format!(
            "seam://pair?v={PROTOCOL_VERSION}&host={}&port={}&fp={}&key={}&name={}",
            self.hosts.join(","),
            self.port,
            self.fingerprint,
            URL_SAFE_NO_PAD.encode(self.key),
            percent_encode(&self.name)
        )
    }

    /// Parse and validate a pairing link.
    pub fn parse(uri: &str) -> Result<Self, String> {
        let query = uri
            .strip_prefix("seam://pair?")
            .ok_or("not a Seam pairing link")?;
        let mut v = None;
        let (mut hosts, mut port, mut fp, mut key, mut name) = (None, None, None, None, None);
        for pair in query.split('&') {
            let (k, val) = pair.split_once('=').unwrap_or((pair, ""));
            let val = percent_decode(val)?;
            match k {
                "v" => v = Some(val),
                "host" => hosts = Some(val),
                "port" => port = Some(val),
                "fp" => fp = Some(val),
                "key" => key = Some(val),
                "name" => name = Some(val),
                _ => {}
            }
        }
        if v.as_deref() != Some("1") {
            return Err("unsupported pairing link version".into());
        }
        let hosts: Vec<String> = hosts
            .ok_or("missing host")?
            .split(',')
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(String::from)
            .collect();
        if hosts.is_empty() {
            return Err("missing host".into());
        }
        let port: u16 = port
            .ok_or("missing port")?
            .parse()
            .map_err(|_| "invalid port")?;
        if port == 0 {
            return Err("invalid port".into());
        }
        let fingerprint = fp.ok_or("missing fingerprint")?.to_ascii_lowercase();
        if fingerprint.len() != 64 || !fingerprint.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("invalid fingerprint".into());
        }
        let key_bytes = URL_SAFE_NO_PAD
            .decode(key.ok_or("missing key")?)
            .map_err(|_| "invalid key")?;
        let key: [u8; 32] = key_bytes.try_into().map_err(|_| "key must be 32 bytes")?;
        Ok(Self {
            hosts,
            port,
            fingerprint,
            key,
            name: name.ok_or("missing name")?,
        })
    }
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn percent_decode(s: &str) -> Result<String, String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = s.get(i + 1..i + 3).ok_or("bad escape in link")?;
                out.push(u8::from_str_radix(hex, 16).map_err(|_| "bad escape in link")?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| "link is not valid UTF-8".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VECTORS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../protocol/vectors.json"
    ));

    fn vectors() -> serde_json::Value {
        serde_json::from_str(VECTORS).unwrap()
    }

    #[test]
    fn proofs_match_shared_vectors() {
        for v in vectors()["proofs"].as_array().unwrap() {
            let key = URL_SAFE_NO_PAD.decode(v["key"].as_str().unwrap()).unwrap();
            let (nonce, id) = (
                v["nonce"].as_str().unwrap(),
                v["device_id"].as_str().unwrap(),
            );
            let expected = v["proof"].as_str().unwrap();
            assert_eq!(proof(&key, nonce, id), expected);
            assert!(verify_proof(&key, nonce, id, expected));
            assert!(!verify_proof(&key, nonce, "someone-else", expected));
            assert!(!verify_proof(&key, nonce, id, "not hex"));
        }
    }

    #[test]
    fn parses_shared_pairing_link() {
        let v = vectors();
        let info = PairingInfo::parse(v["pair_uri"]["uri"].as_str().unwrap()).unwrap();
        let p = &v["pair_uri"]["parsed"];
        assert_eq!(info.hosts, ["192.168.1.10", "10.0.0.5"]);
        assert_eq!(info.port, p["port"].as_u64().unwrap() as u16);
        assert_eq!(info.fingerprint, p["fingerprint"].as_str().unwrap());
        assert_eq!(URL_SAFE_NO_PAD.encode(info.key), p["key"].as_str().unwrap());
        assert_eq!(info.name, "Johns MacBook Air");
        // Round trip.
        assert_eq!(PairingInfo::parse(&info.to_uri()).unwrap(), info);
    }

    #[test]
    fn rejects_shared_invalid_links() {
        for uri in vectors()["invalid_pair_uris"].as_array().unwrap() {
            let uri = uri.as_str().unwrap();
            assert!(PairingInfo::parse(uri).is_err(), "should reject {uri}");
        }
    }

    #[test]
    fn messages_round_trip_and_unknown_types_are_tolerated() {
        let msgs = [
            Message::Challenge {
                v: 1,
                nonce: "ab".into(),
            },
            Message::Notification(PhoneNotification {
                id: "1".into(),
                app: "com.whatsapp".into(),
                app_name: "WhatsApp".into(),
                title: "Anna".into(),
                text: "Hei!".into(),
                time: 1_760_000_000_000,
            }),
            Message::Battery {
                level: 82,
                charging: true,
            },
            Message::Ping,
        ];
        for m in msgs {
            assert_eq!(Message::from_line(&m.to_line()).unwrap(), m);
        }
        assert_eq!(
            Message::from_line(r#"{"type":"from_the_future","x":1}"#).unwrap(),
            Message::Unknown
        );
        assert_eq!(
            Message::from_line(r#"{"type":"ping"}"#).unwrap(),
            Message::Ping
        );
        assert!(Message::from_line("not json").is_err());
    }
}
