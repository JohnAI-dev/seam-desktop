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
/// Longest notification reply, in Unicode scalar values (not bytes).
pub const MAX_REPLY_CHARS: usize = 5_000;
/// How long the phone rings after `ring` before stopping on its own.
pub const RING_SECS: u64 = 60;

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
    /// Inline reply is available. Missing on the wire means false.
    #[serde(default)]
    pub replyable: bool,
}

/// State of a phone call, as the phone reports it.
///
/// Unknown states are kept so a newer phone does not break the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    Ringing,
    Active,
    Ended,
    /// A state this build does not know.
    #[serde(other)]
    Unknown,
}

/// What the computer asks the phone to do with a ringing call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallActionKind {
    Decline,
    Silence,
    /// An action this build does not know.
    #[serde(other)]
    Unknown,
}

/// Who to show for a call: the contact name, otherwise the number, otherwise "Unknown caller".
///
/// Blank and whitespace-only fields are treated as missing.
pub fn caller_label(name: &str, number: &str) -> String {
    let name = name.trim();
    if !name.is_empty() {
        return name.to_string();
    }
    let number = number.trim();
    if !number.is_empty() {
        return number.to_string();
    }
    "Unknown caller".to_string()
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
    /// Desktop → phone: cancel this notification on the phone.
    Dismiss {
        id: String,
    },
    /// Either direction: put this text on the receiver's clipboard.
    Clipboard {
        text: String,
    },
    /// Desktop → phone: fill this notification's inline reply field.
    Reply {
        id: String,
        text: String,
    },
    /// Phone → desktop: the inline reply succeeded or failed.
    ReplyResult {
        id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Phone → desktop: a call is ringing, active, or ended.
    Call {
        state: CallState,
        #[serde(default)]
        number: String,
        #[serde(default)]
        name: String,
    },
    /// Desktop → phone: decline the ringing call, or silence its ringtone.
    CallAction {
        action: CallActionKind,
    },
    /// Desktop → phone: ring loudly until `ring_stop`, "Found it" on the phone, or 60 seconds.
    Ring,
    /// Desktop → phone: stop the loud ring.
    RingStop,
    /// Either direction: offer a file. `transfer` is 32 hex chars; `size` is at most 2 GiB.
    FileOffer {
        transfer: String,
        name: String,
        size: u64,
        /// MIME type, when the sender knows it. Missing means unknown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
    },
    /// Receiver will save the file.
    FileAccept {
        transfer: String,
    },
    /// Receiver refuses the offer.
    FileReject {
        transfer: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// One ordered chunk. `data` is standard base64 of at most 256 KiB of raw bytes.
    FileChunk {
        transfer: String,
        seq: u64,
        data: String,
    },
    /// Sender has finished. `sha256` is the hex digest of the whole file.
    FileDone {
        transfer: String,
        sha256: String,
    },
    /// Receiver checked the size and SHA-256.
    FileResult {
        transfer: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Either side gives up. The receiver deletes the partial file.
    FileCancel {
        transfer: String,
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

/// Trim a notification reply and reject empty or over-long text.
///
/// The limit is [`MAX_REPLY_CHARS`] characters after trimming, not bytes.
pub fn normalize_reply_text(text: &str) -> Result<String, &'static str> {
    let text = text.trim();
    if text.is_empty() {
        return Err("reply is empty");
    }
    if text.chars().count() > MAX_REPLY_CHARS {
        return Err("reply is too long");
    }
    Ok(text.to_string())
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
                replyable: false,
            }),
            Message::Battery {
                level: 82,
                charging: true,
            },
            Message::Ping,
            Message::Dismiss { id: "k".into() },
            Message::Clipboard { text: "hei".into() },
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

    #[test]
    fn reply_round_trip_and_replyable_defaults_to_false() {
        let reply = Message::Reply {
            id: "0|com.whatsapp|9".into(),
            text: "Ja, kl 18".into(),
        };
        assert_eq!(
            reply.to_line(),
            r#"{"type":"reply","id":"0|com.whatsapp|9","text":"Ja, kl 18"}"#,
        );
        assert_eq!(Message::from_line(&reply.to_line()).unwrap(), reply);

        let ok = Message::ReplyResult {
            id: "0|com.whatsapp|9".into(),
            ok: true,
            error: None,
        };
        assert_eq!(
            ok.to_line(),
            r#"{"type":"reply_result","id":"0|com.whatsapp|9","ok":true}"#,
        );
        assert_eq!(Message::from_line(&ok.to_line()).unwrap(), ok);
        assert_eq!(
            Message::from_line(r#"{"type":"reply_result","id":"k","ok":true}"#).unwrap(),
            Message::ReplyResult {
                id: "k".into(),
                ok: true,
                error: None,
            }
        );

        let err = Message::ReplyResult {
            id: "k".into(),
            ok: false,
            error: Some("notification is gone".into()),
        };
        assert_eq!(
            err.to_line(),
            r#"{"type":"reply_result","id":"k","ok":false,"error":"notification is gone"}"#,
        );
        assert_eq!(Message::from_line(&err.to_line()).unwrap(), err);
        assert_eq!(
            Message::from_line(r#"{"type":"reply_result","id":"k","ok":false}"#).unwrap(),
            Message::ReplyResult {
                id: "k".into(),
                ok: false,
                error: None,
            }
        );

        match Message::from_line(r#"{"type":"notification","id":"n1","title":"Hi"}"#).unwrap() {
            Message::Notification(n) => {
                assert!(!n.replyable);
                assert_eq!(n.id, "n1");
                assert_eq!(n.title, "Hi");
                assert_eq!(n.app, "");
                assert_eq!(n.time, 0);
            }
            other => panic!("expected notification, got {other:?}"),
        }
        let without = Message::from_line(r#"{"type":"notification","id":"n1"}"#).unwrap();
        assert!(without.to_line().contains("\"replyable\":false"));

        let with_flag = Message::Notification(PhoneNotification {
            id: "n2".into(),
            app: String::new(),
            app_name: String::new(),
            title: String::new(),
            text: "Hei".into(),
            time: 0,
            replyable: true,
        });
        assert_eq!(Message::from_line(&with_flag.to_line()).unwrap(), with_flag);
        assert!(with_flag.to_line().contains("\"replyable\":true"));
        match Message::from_line(
            r#"{"type":"notification","id":"n2","replyable":true,"text":"Hei"}"#,
        )
        .unwrap()
        {
            Message::Notification(n) => {
                assert!(n.replyable);
                assert_eq!(n.text, "Hei");
            }
            other => panic!("expected notification, got {other:?}"),
        }
    }

    #[test]
    fn reply_text_is_trimmed_and_limited_to_5000_chars() {
        assert_eq!(MAX_REPLY_CHARS, 5_000);
        assert_eq!(normalize_reply_text("  hei  ").unwrap(), "hei");
        assert_eq!(normalize_reply_text("\n\tok\t\n").unwrap(), "ok");
        assert_eq!(normalize_reply_text("   ").unwrap_err(), "reply is empty");
        assert_eq!(normalize_reply_text("").unwrap_err(), "reply is empty");

        let exact = "å".repeat(5_000);
        assert_eq!(
            normalize_reply_text(&format!("  {exact}  ")).unwrap(),
            exact
        );
        assert_eq!(
            normalize_reply_text(&format!("{exact}!")).unwrap_err(),
            "reply is too long"
        );
        // The limit is characters, not bytes: '你' is one character and three bytes.
        assert_eq!(
            normalize_reply_text(&"你".repeat(5_000))
                .unwrap()
                .chars()
                .count(),
            5_000
        );
        assert_eq!(
            normalize_reply_text(&"你".repeat(5_001)).unwrap_err(),
            "reply is too long"
        );
    }

    #[test]
    fn call_and_call_action_round_trip() {
        let ringing = Message::Call {
            state: CallState::Ringing,
            number: "+4712345678".into(),
            name: "Anna".into(),
        };
        assert_eq!(
            ringing.to_line(),
            r#"{"type":"call","state":"ringing","number":"+4712345678","name":"Anna"}"#,
        );
        assert_eq!(Message::from_line(&ringing.to_line()).unwrap(), ringing);

        let active = Message::Call {
            state: CallState::Active,
            number: String::new(),
            name: "Anna".into(),
        };
        assert_eq!(
            active.to_line(),
            r#"{"type":"call","state":"active","number":"","name":"Anna"}"#,
        );
        assert_eq!(Message::from_line(&active.to_line()).unwrap(), active);

        let ended = Message::Call {
            state: CallState::Ended,
            number: String::new(),
            name: String::new(),
        };
        assert_eq!(
            ended.to_line(),
            r#"{"type":"call","state":"ended","number":"","name":""}"#,
        );
        assert_eq!(Message::from_line(&ended.to_line()).unwrap(), ended);

        // number and name are optional on the wire; missing means empty.
        assert_eq!(
            Message::from_line(r#"{"type":"call","state":"ringing"}"#).unwrap(),
            Message::Call {
                state: CallState::Ringing,
                number: String::new(),
                name: String::new(),
            }
        );

        let decline = Message::CallAction {
            action: CallActionKind::Decline,
        };
        assert_eq!(
            decline.to_line(),
            r#"{"type":"call_action","action":"decline"}"#,
        );
        assert_eq!(Message::from_line(&decline.to_line()).unwrap(), decline);

        let silence = Message::CallAction {
            action: CallActionKind::Silence,
        };
        assert_eq!(
            silence.to_line(),
            r#"{"type":"call_action","action":"silence"}"#,
        );
        assert_eq!(Message::from_line(&silence.to_line()).unwrap(), silence);

        // A newer phone's state or action must not fail the frame.
        match Message::from_line(r#"{"type":"call","state":"holding","number":"1","name":"A"}"#)
            .unwrap()
        {
            Message::Call {
                state: CallState::Unknown,
                number,
                name,
            } => {
                assert_eq!(number, "1");
                assert_eq!(name, "A");
            }
            other => panic!("expected call, got {other:?}"),
        }
        assert_eq!(
            Message::from_line(r#"{"type":"call_action","action":"answer"}"#).unwrap(),
            Message::CallAction {
                action: CallActionKind::Unknown,
            }
        );
    }

    #[test]
    fn ring_and_ring_stop_round_trip() {
        assert_eq!(RING_SECS, 60);
        assert_eq!(Message::Ring.to_line(), r#"{"type":"ring"}"#);
        assert_eq!(
            Message::from_line(r#"{"type":"ring"}"#).unwrap(),
            Message::Ring
        );
        assert_eq!(Message::RingStop.to_line(), r#"{"type":"ring_stop"}"#);
        assert_eq!(
            Message::from_line(r#"{"type":"ring_stop"}"#).unwrap(),
            Message::RingStop
        );
    }

    #[test]
    fn caller_label_prefers_name_then_number_then_unknown() {
        assert_eq!(caller_label("Anna", "+47123"), "Anna");
        assert_eq!(caller_label("  Anna  ", "+47123"), "Anna");
        assert_eq!(caller_label("", "+47123"), "+47123");
        assert_eq!(caller_label("   ", "  +47 123  "), "+47 123");
        assert_eq!(caller_label("", ""), "Unknown caller");
        assert_eq!(caller_label("  ", " \t"), "Unknown caller");
        assert_eq!(caller_label("\n", ""), "Unknown caller");
    }

    #[test]
    fn file_offer_accept_reject_and_cancel_round_trip() {
        let id = "00112233445566778899aabbccddeeff";
        let offer = Message::FileOffer {
            transfer: id.into(),
            name: "photo.jpg".into(),
            size: 12,
            mime: Some("image/jpeg".into()),
        };
        assert_eq!(
            offer.to_line(),
            r#"{"type":"file_offer","transfer":"00112233445566778899aabbccddeeff","name":"photo.jpg","size":12,"mime":"image/jpeg"}"#,
        );
        assert_eq!(Message::from_line(&offer.to_line()).unwrap(), offer);

        let bare = Message::FileOffer {
            transfer: id.into(),
            name: "photo.jpg".into(),
            size: 12,
            mime: None,
        };
        assert_eq!(
            bare.to_line(),
            r#"{"type":"file_offer","transfer":"00112233445566778899aabbccddeeff","name":"photo.jpg","size":12}"#,
        );
        assert_eq!(Message::from_line(&bare.to_line()).unwrap(), bare);

        let accept = Message::FileAccept {
            transfer: id.into(),
        };
        assert_eq!(
            accept.to_line(),
            r#"{"type":"file_accept","transfer":"00112233445566778899aabbccddeeff"}"#,
        );
        assert_eq!(Message::from_line(&accept.to_line()).unwrap(), accept);

        let reject = Message::FileReject {
            transfer: id.into(),
            reason: Some("no space".into()),
        };
        assert_eq!(
            reject.to_line(),
            r#"{"type":"file_reject","transfer":"00112233445566778899aabbccddeeff","reason":"no space"}"#,
        );
        assert_eq!(Message::from_line(&reject.to_line()).unwrap(), reject);
        assert_eq!(
            Message::from_line(
                r#"{"type":"file_reject","transfer":"00112233445566778899aabbccddeeff"}"#,
            )
            .unwrap(),
            Message::FileReject {
                transfer: id.into(),
                reason: None,
            }
        );

        let cancel = Message::FileCancel {
            transfer: id.into(),
        };
        assert_eq!(
            cancel.to_line(),
            r#"{"type":"file_cancel","transfer":"00112233445566778899aabbccddeeff"}"#,
        );
        assert_eq!(Message::from_line(&cancel.to_line()).unwrap(), cancel);
    }

    #[test]
    fn file_chunk_done_and_result_round_trip() {
        let id = "00112233445566778899aabbccddeeff";
        let chunk = Message::FileChunk {
            transfer: id.into(),
            seq: 0,
            data: "aGVsbG8=".into(),
        };
        assert_eq!(
            chunk.to_line(),
            r#"{"type":"file_chunk","transfer":"00112233445566778899aabbccddeeff","seq":0,"data":"aGVsbG8="}"#,
        );
        assert_eq!(Message::from_line(&chunk.to_line()).unwrap(), chunk);

        let hash = "ab".repeat(32);
        let done = Message::FileDone {
            transfer: id.into(),
            sha256: hash.clone(),
        };
        assert_eq!(
            done.to_line(),
            format!(r#"{{"type":"file_done","transfer":"{id}","sha256":"{hash}"}}"#),
        );
        assert_eq!(Message::from_line(&done.to_line()).unwrap(), done);

        let ok = Message::FileResult {
            transfer: id.into(),
            ok: true,
            error: None,
        };
        assert_eq!(
            ok.to_line(),
            r#"{"type":"file_result","transfer":"00112233445566778899aabbccddeeff","ok":true}"#,
        );
        assert_eq!(Message::from_line(&ok.to_line()).unwrap(), ok);
        assert_eq!(
            Message::from_line(
                r#"{"type":"file_result","transfer":"00112233445566778899aabbccddeeff","ok":true}"#,
            )
            .unwrap(),
            Message::FileResult {
                transfer: id.into(),
                ok: true,
                error: None,
            }
        );

        let err = Message::FileResult {
            transfer: id.into(),
            ok: false,
            error: Some("wrong hash".into()),
        };
        assert_eq!(
            err.to_line(),
            r#"{"type":"file_result","transfer":"00112233445566778899aabbccddeeff","ok":false,"error":"wrong hash"}"#,
        );
        assert_eq!(Message::from_line(&err.to_line()).unwrap(), err);
        assert_eq!(
            Message::from_line(
                r#"{"type":"file_result","transfer":"00112233445566778899aabbccddeeff","ok":false}"#,
            )
            .unwrap(),
            Message::FileResult {
                transfer: id.into(),
                ok: false,
                error: None,
            }
        );
    }
}
