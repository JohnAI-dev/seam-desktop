//! End-to-end: a Rust "phone" pairs with the real link server over real TLS.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use seam_core::link::client::PhoneClient;
use seam_core::link::protocol::{CallActionKind, CallState};
use seam_core::link::{LinkEvent, LinkServer, Message, PairingInfo, PhoneNotification};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedReceiver;

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("seam-link-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Start a server on a random local port; returns it, its events and the port.
async fn start(name: &str) -> (LinkServer, UnboundedReceiver<LinkEvent>, u16) {
    let (server, events) = LinkServer::new(&temp_dir(name), "Test Desktop".into()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(server.clone().serve(listener));
    (server, events, port)
}

async fn next_event(events: &mut UnboundedReceiver<LinkEvent>) -> LinkEvent {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("timed out waiting for a link event")
        .expect("event channel closed")
}

fn pair(server: &LinkServer, port: u16) -> PairingInfo {
    server.start_pairing(vec!["127.0.0.1".into()], port)
}

#[tokio::test]
async fn phone_pairs_sends_notification_and_battery_then_reconnects() {
    let (server, mut events, port) = start("happy").await;
    let info = pair(&server, port);
    // The phone only ever sees the link, so go through the URI like the camera would.
    let info = PairingInfo::parse(&info.to_uri()).unwrap();

    let mut phone =
        PhoneClient::connect(&info, "127.0.0.1", &info.key, "phone-1", "Galaxy S24 Ultra")
            .await
            .expect("pairing should succeed");
    assert_eq!(phone.desktop_name, "Test Desktop");
    assert_eq!(
        next_event(&mut events).await,
        LinkEvent::Paired {
            device_id: "phone-1".into(),
            name: "Galaxy S24 Ultra".into()
        }
    );
    assert_eq!(
        next_event(&mut events).await,
        LinkEvent::Connected {
            device_id: "phone-1".into(),
            name: "Galaxy S24 Ultra".into()
        }
    );

    let n = PhoneNotification {
        id: "0|com.whatsapp|1".into(),
        app: "com.whatsapp".into(),
        app_name: "WhatsApp".into(),
        title: "Anna".into(),
        text: "Middag kl 18?".into(),
        time: 1_760_000_000_000,
        replyable: false,
    };
    phone.send(&Message::Notification(n.clone())).await.unwrap();
    assert_eq!(
        next_event(&mut events).await,
        LinkEvent::Notification {
            device_id: "phone-1".into(),
            notification: n
        }
    );
    phone
        .send(&Message::Battery {
            level: 82,
            charging: true,
        })
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        LinkEvent::Battery {
            device_id: "phone-1".into(),
            level: 82,
            charging: true
        }
    );
    // Unknown messages from newer phones are ignored, not fatal.
    phone.send(&Message::Unknown).await.unwrap();
    phone
        .send(&Message::NotificationRemoved {
            id: "0|com.whatsapp|1".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        LinkEvent::NotificationRemoved {
            device_id: "phone-1".into(),
            id: "0|com.whatsapp|1".into()
        }
    );

    phone.close().await;
    assert_eq!(
        next_event(&mut events).await,
        LinkEvent::Disconnected {
            device_id: "phone-1".into()
        }
    );
    assert_eq!(server.paired_devices().len(), 1);

    // Reconnecting later uses the stored key; no new pairing needed.
    let _phone = PhoneClient::connect(&info, "127.0.0.1", &info.key, "phone-1", "Galaxy S24 Ultra")
        .await
        .expect("reconnect with stored key");
    assert!(matches!(
        next_event(&mut events).await,
        LinkEvent::Connected { .. }
    ));
}

#[tokio::test]
async fn pairing_code_works_once() {
    let (server, mut events, port) = start("once").await;
    let info = pair(&server, port);
    PhoneClient::connect(&info, "127.0.0.1", &info.key, "phone-a", "A")
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut events).await,
        LinkEvent::Paired { .. }
    ));

    let err = PhoneClient::connect(&info, "127.0.0.1", &info.key, "phone-b", "B")
        .await
        .err()
        .expect("a used pairing code must not pair a second phone");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(server.paired_devices().len(), 1);
}

#[tokio::test]
async fn wrong_key_is_rejected() {
    let (server, _events, port) = start("wrongkey").await;
    let info = pair(&server, port);
    let err = PhoneClient::connect(&info, "127.0.0.1", &[9u8; 32], "phone-x", "X")
        .await
        .err()
        .expect("wrong key must fail");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(server.paired_devices().is_empty());
}

#[tokio::test]
async fn phone_refuses_a_desktop_with_a_different_certificate() {
    let (server, _events, port) = start("pin").await;
    let mut info = pair(&server, port);
    info.fingerprint = "00".repeat(32);
    assert!(
        PhoneClient::connect(&info, "127.0.0.1", &info.key, "phone-1", "P")
            .await
            .is_err(),
        "certificate pinning must reject an unknown desktop"
    );
    assert!(server.paired_devices().is_empty());
}

#[tokio::test]
async fn forgotten_phone_cannot_reconnect() {
    let (server, mut events, port) = start("forget").await;
    let info = pair(&server, port);
    drop(
        PhoneClient::connect(&info, "127.0.0.1", &info.key, "phone-1", "P")
            .await
            .unwrap(),
    );
    while !matches!(
        next_event(&mut events).await,
        LinkEvent::Disconnected { .. }
    ) {}
    assert!(server.forget("phone-1").unwrap());
    assert!(
        PhoneClient::connect(&info, "127.0.0.1", &info.key, "phone-1", "P")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn desktop_can_send_to_a_connected_phone_and_receive_clipboard() {
    let (server, mut events, port) = start("send").await;
    let info = pair(&server, port);
    let mut phone = PhoneClient::connect(&info, "127.0.0.1", &info.key, "phone-1", "P")
        .await
        .unwrap();
    while !matches!(next_event(&mut events).await, LinkEvent::Connected { .. }) {}

    assert_eq!(server.connected(), vec!["phone-1".to_string()]);
    assert!(server.send_to("phone-1", Message::Dismiss { id: "k1".into() }));
    assert!(!server.send_to("nobody", Message::Ping));
    let got = tokio::time::timeout(Duration::from_secs(5), phone.recv())
        .await
        .unwrap();
    assert_eq!(got, Some(Message::Dismiss { id: "k1".into() }));

    phone
        .send(&Message::Clipboard {
            text: "fra mobilen".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        LinkEvent::Clipboard {
            device_id: "phone-1".into(),
            text: "fra mobilen".into()
        }
    );

    phone.close().await;
    while !matches!(
        next_event(&mut events).await,
        LinkEvent::Disconnected { .. }
    ) {}
    assert!(server.connected().is_empty());
}

#[tokio::test]
async fn desktop_reply_reaches_the_phone_as_the_exact_frame() {
    let (server, mut events, port) = start("reply").await;
    let info = pair(&server, port);
    let mut phone = PhoneClient::connect(&info, "127.0.0.1", &info.key, "phone-1", "Pixel")
        .await
        .unwrap();
    while !matches!(next_event(&mut events).await, LinkEvent::Connected { .. }) {}

    let n = PhoneNotification {
        id: "0|com.whatsapp|9".into(),
        app: "com.whatsapp".into(),
        app_name: "WhatsApp".into(),
        title: "Anna".into(),
        text: "Middag?".into(),
        time: 1_760_000_000_000,
        replyable: true,
    };
    phone.send(&Message::Notification(n.clone())).await.unwrap();
    assert_eq!(
        next_event(&mut events).await,
        LinkEvent::Notification {
            device_id: "phone-1".into(),
            notification: n.clone(),
        }
    );

    let reply = Message::Reply {
        id: n.id.clone(),
        text: "Ja, kl 18".into(),
    };
    assert!(server.send_to("phone-1", reply.clone()));
    let got = tokio::time::timeout(Duration::from_secs(5), phone.recv())
        .await
        .expect("timed out waiting for the reply frame")
        .expect("phone connection closed before the reply");
    assert_eq!(
        got.to_line(),
        r#"{"type":"reply","id":"0|com.whatsapp|9","text":"Ja, kl 18"}"#,
    );
    assert_eq!(got, reply);

    phone
        .send(&Message::ReplyResult {
            id: n.id.clone(),
            ok: true,
            error: None,
        })
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        LinkEvent::ReplyResult {
            device_id: "phone-1".into(),
            id: n.id.clone(),
            ok: true,
            error: None,
        }
    );
    phone
        .send(&Message::ReplyResult {
            id: n.id.clone(),
            ok: false,
            error: Some("notification is gone".into()),
        })
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        LinkEvent::ReplyResult {
            device_id: "phone-1".into(),
            id: n.id,
            ok: false,
            error: Some("notification is gone".into()),
        }
    );
}

#[tokio::test]
async fn phone_sends_a_ringing_call_and_receives_call_action() {
    let (server, mut events, port) = start("call").await;
    let info = pair(&server, port);
    let mut phone = PhoneClient::connect(&info, "127.0.0.1", &info.key, "phone-1", "Pixel")
        .await
        .unwrap();
    while !matches!(next_event(&mut events).await, LinkEvent::Connected { .. }) {}

    phone
        .send(&Message::Call {
            state: CallState::Ringing,
            number: "+4712345678".into(),
            name: "Anna".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        LinkEvent::Call {
            device_id: "phone-1".into(),
            state: CallState::Ringing,
            number: "+4712345678".into(),
            name: "Anna".into(),
        }
    );

    // Decline and Silence are the frames the window buttons send.
    let decline = Message::CallAction {
        action: CallActionKind::Decline,
    };
    assert!(server.send_to("phone-1", decline.clone()));
    let got = tokio::time::timeout(Duration::from_secs(5), phone.recv())
        .await
        .expect("timed out waiting for the call_action frame")
        .expect("phone connection closed before the call_action");
    assert_eq!(
        got.to_line(),
        r#"{"type":"call_action","action":"decline"}"#,
    );
    assert_eq!(got, decline);

    let silence = Message::CallAction {
        action: CallActionKind::Silence,
    };
    assert!(server.send_to("phone-1", silence.clone()));
    let got = tokio::time::timeout(Duration::from_secs(5), phone.recv())
        .await
        .expect("timed out waiting for the silence frame")
        .expect("phone connection closed before silence");
    assert_eq!(
        got.to_line(),
        r#"{"type":"call_action","action":"silence"}"#,
    );
    assert_eq!(got, silence);

    phone
        .send(&Message::Call {
            state: CallState::Active,
            number: "+4712345678".into(),
            name: "Anna".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        LinkEvent::Call {
            device_id: "phone-1".into(),
            state: CallState::Active,
            number: "+4712345678".into(),
            name: "Anna".into(),
        }
    );
    phone
        .send(&Message::Call {
            state: CallState::Ended,
            number: String::new(),
            name: String::new(),
        })
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        LinkEvent::Call {
            device_id: "phone-1".into(),
            state: CallState::Ended,
            number: String::new(),
            name: String::new(),
        }
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

async fn next_msg(phone: &mut PhoneClient) -> Message {
    tokio::time::timeout(Duration::from_secs(5), phone.recv())
        .await
        .expect("timed out waiting for a frame")
        .expect("phone connection closed")
}

#[tokio::test]
async fn phone_sends_a_file_and_a_wrong_hash_leaves_nothing() {
    let (server, mut events, port) = start("file").await;
    let info = pair(&server, port);
    let mut phone = PhoneClient::connect(&info, "127.0.0.1", &info.key, "phone-1", "Pixel")
        .await
        .unwrap();
    while !matches!(next_event(&mut events).await, LinkEvent::Connected { .. }) {}

    let download = server.download_dir().to_path_buf();
    let payload: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let id = "ab".repeat(16);
    phone
        .send(&Message::FileOffer {
            transfer: id.clone(),
            name: "../../etc/passwd".into(),
            size: payload.len() as u64,
            mime: Some("application/octet-stream".into()),
        })
        .await
        .unwrap();
    assert_eq!(
        next_msg(&mut phone).await,
        Message::FileAccept {
            transfer: id.clone()
        }
    );

    let mut seq = 0u64;
    for chunk in payload.chunks(200_000) {
        phone
            .send(&Message::FileChunk {
                transfer: id.clone(),
                seq,
                data: STANDARD.encode(chunk),
            })
            .await
            .unwrap();
        if seq == 0 {
            // Other messages may be interleaved with chunks.
            phone
                .send(&Message::Battery {
                    level: 50,
                    charging: false,
                })
                .await
                .unwrap();
        }
        seq += 1;
    }
    assert!(seq >= 5, "expected several chunks, sent {seq}");
    phone
        .send(&Message::FileDone {
            transfer: id.clone(),
            sha256: sha256_hex(&payload),
        })
        .await
        .unwrap();
    assert_eq!(
        next_msg(&mut phone).await,
        Message::FileResult {
            transfer: id.clone(),
            ok: true,
            error: None,
        }
    );
    assert_eq!(
        next_event(&mut events).await,
        LinkEvent::Battery {
            device_id: "phone-1".into(),
            level: 50,
            charging: false,
        }
    );
    match next_event(&mut events).await {
        LinkEvent::FileReceived { phone, path } => {
            assert_eq!(phone, "phone-1");
            assert_eq!(path.parent(), Some(download.as_path()));
            assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("passwd"));
            assert_eq!(std::fs::read(&path).unwrap(), payload);
        }
        other => panic!("expected FileReceived, got {other:?}"),
    }
    assert!(file_names(&download).iter().all(|n| !n.ends_with(".part")));

    // A second file with the same name must not overwrite the first.
    let id2 = "cd".repeat(16);
    let second = b"v2!";
    phone
        .send(&Message::FileOffer {
            transfer: id2.clone(),
            name: "../../etc/passwd".into(),
            size: second.len() as u64,
            mime: None,
        })
        .await
        .unwrap();
    assert_eq!(
        next_msg(&mut phone).await,
        Message::FileAccept {
            transfer: id2.clone()
        }
    );
    phone
        .send(&Message::FileChunk {
            transfer: id2.clone(),
            seq: 0,
            data: STANDARD.encode(second),
        })
        .await
        .unwrap();
    phone
        .send(&Message::FileDone {
            transfer: id2.clone(),
            sha256: sha256_hex(second),
        })
        .await
        .unwrap();
    assert_eq!(
        next_msg(&mut phone).await,
        Message::FileResult {
            transfer: id2,
            ok: true,
            error: None,
        }
    );
    match next_event(&mut events).await {
        LinkEvent::FileReceived { path, .. } => {
            assert_eq!(
                path.file_name().and_then(|n| n.to_str()),
                Some("passwd (1)")
            );
            assert_eq!(std::fs::read(&path).unwrap(), second);
        }
        other => panic!("expected FileReceived, got {other:?}"),
    }
    assert_eq!(std::fs::read(download.join("passwd")).unwrap(), payload);

    // A wrong hash deletes the partial and leaves nothing new behind.
    let before = file_names(&download);
    let bad_id = "ef".repeat(16);
    let bad = b"nope";
    phone
        .send(&Message::FileOffer {
            transfer: bad_id.clone(),
            name: "evil.bin".into(),
            size: bad.len() as u64,
            mime: None,
        })
        .await
        .unwrap();
    assert!(matches!(
        next_msg(&mut phone).await,
        Message::FileAccept { .. }
    ));
    phone
        .send(&Message::FileChunk {
            transfer: bad_id.clone(),
            seq: 0,
            data: STANDARD.encode(bad),
        })
        .await
        .unwrap();
    phone
        .send(&Message::FileDone {
            transfer: bad_id.clone(),
            sha256: "00".repeat(32),
        })
        .await
        .unwrap();
    match next_msg(&mut phone).await {
        Message::FileResult {
            ok: false,
            transfer,
            ..
        } => assert_eq!(transfer, bad_id),
        other => panic!("expected file_result ok:false, got {other:?}"),
    }
    assert_eq!(file_names(&download), before);
    assert!(!download.join("evil.bin").exists());
    assert!(file_names(&download).iter().all(|n| !n.ends_with(".part")));
    assert_eq!(std::fs::read(download.join("passwd")).unwrap(), payload);

    // file_cancel deletes the partial file.
    let cancel_id = "12".repeat(16);
    phone
        .send(&Message::FileOffer {
            transfer: cancel_id.clone(),
            name: "partial.bin".into(),
            size: 4,
            mime: None,
        })
        .await
        .unwrap();
    assert!(matches!(
        next_msg(&mut phone).await,
        Message::FileAccept { .. }
    ));
    phone
        .send(&Message::FileChunk {
            transfer: cancel_id.clone(),
            seq: 0,
            data: STANDARD.encode(b"part"),
        })
        .await
        .unwrap();
    phone
        .send(&Message::FileCancel {
            transfer: cancel_id,
        })
        .await
        .unwrap();
    phone
        .send(&Message::Battery {
            level: 9,
            charging: true,
        })
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        LinkEvent::Battery {
            device_id: "phone-1".into(),
            level: 9,
            charging: true,
        }
    );
    assert!(!download.join("partial.bin").exists());
    assert!(file_names(&download).iter().all(|n| !n.ends_with(".part")));
    assert_eq!(file_names(&download), before);
    assert_eq!(std::fs::read(download.join("passwd (1)")).unwrap(), second);
}

#[tokio::test]
async fn phone_receives_ring_then_ring_stop() {
    let (server, mut events, port) = start("ring").await;
    let info = pair(&server, port);
    let mut phone = PhoneClient::connect(&info, "127.0.0.1", &info.key, "phone-1", "Pixel")
        .await
        .unwrap();
    while !matches!(next_event(&mut events).await, LinkEvent::Connected { .. }) {}

    assert!(server.send_to("phone-1", Message::Ring));
    let got = next_msg(&mut phone).await;
    assert_eq!(got.to_line(), r#"{"type":"ring"}"#);
    assert_eq!(got, Message::Ring);

    assert!(server.send_to("phone-1", Message::RingStop));
    let got = next_msg(&mut phone).await;
    assert_eq!(got.to_line(), r#"{"type":"ring_stop"}"#);
    assert_eq!(got, Message::RingStop);

    phone
        .send(&Message::Battery {
            level: 11,
            charging: false,
        })
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        LinkEvent::Battery {
            device_id: "phone-1".into(),
            level: 11,
            charging: false,
        }
    );
}

struct SendOutcome {
    ok: bool,
    error: Option<String>,
    progress: Vec<u64>,
    battery: Option<u8>,
}

async fn next_phone_msg(phone: &mut PhoneClient) -> Message {
    loop {
        match next_msg(phone).await {
            Message::Ping => phone.send(&Message::Pong).await.unwrap(),
            other => return other,
        }
    }
}

async fn observe_send(events: &mut UnboundedReceiver<LinkEvent>, transfer: &str) -> SendOutcome {
    let mut progress = Vec::new();
    let mut battery = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("timed out waiting for the send to finish")
            .expect("event channel closed");
        match event {
            LinkEvent::FileSendProgress {
                transfer: id,
                sent,
                size,
                ..
            } if id == transfer => {
                assert!(sent <= size);
                progress.push(sent);
            }
            LinkEvent::FileSendFinished {
                phone,
                transfer: id,
                ok,
                error,
                ..
            } if id == transfer => {
                assert_eq!(phone, "phone-1");
                return SendOutcome {
                    ok,
                    error,
                    progress,
                    battery,
                };
            }
            LinkEvent::Battery { level, .. } => battery = Some(level),
            other => panic!("unexpected event during send: {other:?}"),
        }
    }
}

#[tokio::test]
async fn desktop_sends_a_file_and_a_reject_ends_cleanly() {
    let (server, mut events, port) = start("send-file").await;
    let info = pair(&server, port);
    let mut phone = PhoneClient::connect(&info, "127.0.0.1", &info.key, "phone-1", "Pixel")
        .await
        .unwrap();
    while !matches!(next_event(&mut events).await, LinkEvent::Connected { .. }) {}

    let file_dir = temp_dir("send-file-src");
    let nested = file_dir.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let path = nested.join("photo.jpg");
    let payload: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &payload).unwrap();

    let id = server.send_file("phone-1", &path).unwrap();
    assert_eq!(id.len(), 32);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    match next_phone_msg(&mut phone).await {
        Message::FileOffer {
            transfer,
            name,
            size,
            mime,
        } => {
            assert_eq!(transfer, id);
            assert_eq!(name, "photo.jpg");
            assert!(!name.contains('/'));
            assert!(!name.contains('\\'));
            assert_eq!(size, payload.len() as u64);
            assert_eq!(mime.as_deref(), Some("image/jpeg"));
        }
        other => panic!("expected file_offer, got {other:?}"),
    }
    phone
        .send(&Message::FileAccept {
            transfer: id.clone(),
        })
        .await
        .unwrap();

    let mut got = Vec::new();
    let mut seqs = Vec::new();
    let mut saw_dismiss = false;
    loop {
        match next_phone_msg(&mut phone).await {
            Message::FileChunk {
                transfer,
                seq,
                data,
            } => {
                assert_eq!(transfer, id);
                let bytes = STANDARD
                    .decode(data.as_bytes())
                    .expect("chunk should be standard base64");
                assert!(bytes.len() <= 256 * 1024);
                got.extend(bytes);
                seqs.push(seq);
                if seq == 0 {
                    phone
                        .send(&Message::Battery {
                            level: 33,
                            charging: false,
                        })
                        .await
                        .unwrap();
                    assert!(server.send_to(
                        "phone-1",
                        Message::Dismiss {
                            id: "mid-send".into(),
                        },
                    ));
                }
            }
            Message::FileDone { transfer, sha256 } => {
                assert_eq!(transfer, id);
                assert_eq!(sha256, sha256_hex(&payload));
                assert_eq!(got, payload);
                assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>());
                let n = seqs.len();
                assert!(n >= 4, "expected several chunks, got {n}");
                phone
                    .send(&Message::FileResult {
                        transfer,
                        ok: true,
                        error: None,
                    })
                    .await
                    .unwrap();
                break;
            }
            Message::Dismiss { id: nid } => {
                assert_eq!(nid, "mid-send");
                saw_dismiss = true;
            }
            other => panic!("unexpected frame during send: {other:?}"),
        }
    }
    if !saw_dismiss {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !saw_dismiss && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, phone.recv()).await {
                Ok(Some(Message::Dismiss { id: nid })) => {
                    assert_eq!(nid, "mid-send");
                    saw_dismiss = true;
                }
                Ok(Some(Message::Ping)) => phone.send(&Message::Pong).await.unwrap(),
                other => {
                    panic!("other messages must get through during a send, got {other:?}")
                }
            }
        }
    }
    assert!(
        saw_dismiss,
        "a message queued during the send must be delivered"
    );

    let outcome = observe_send(&mut events, &id).await;
    assert!(outcome.ok, "{:?}", outcome.error);
    assert_eq!(outcome.error, None);
    assert_eq!(outcome.progress.last().copied(), Some(payload.len() as u64));
    if outcome.battery != Some(33) {
        match next_event(&mut events).await {
            LinkEvent::Battery {
                level: 33,
                device_id,
                ..
            } => assert_eq!(device_id, "phone-1"),
            other => panic!("expected the battery sent during the transfer, got {other:?}"),
        }
    }

    let rejected = file_dir.join("nope.txt");
    std::fs::write(&rejected, b"no thanks").unwrap();
    let id2 = server.send_file("phone-1", &rejected).unwrap();
    match next_phone_msg(&mut phone).await {
        Message::FileOffer {
            transfer,
            name,
            size,
            ..
        } => {
            assert_eq!(transfer, id2);
            assert_eq!(name, "nope.txt");
            assert_eq!(size, 9);
        }
        other => panic!("expected offer, got {other:?}"),
    }
    phone
        .send(&Message::FileReject {
            transfer: id2.clone(),
            reason: Some("no space".into()),
        })
        .await
        .unwrap();
    let outcome = observe_send(&mut events, &id2).await;
    assert!(!outcome.ok);
    assert_eq!(outcome.error.as_deref(), Some("no space"));
    match tokio::time::timeout(Duration::from_millis(50), phone.recv()).await {
        Err(_) | Ok(None) => {}
        Ok(Some(Message::Ping)) => phone.send(&Message::Pong).await.unwrap(),
        Ok(Some(other)) => panic!("reject should end the transfer, got {other:?}"),
    }

    let cancel_path = file_dir.join("later.txt");
    std::fs::write(&cancel_path, b"later").unwrap();
    let id3 = server.send_file("phone-1", &cancel_path).unwrap();
    match next_phone_msg(&mut phone).await {
        Message::FileOffer { transfer, .. } => assert_eq!(transfer, id3),
        other => panic!("expected offer, got {other:?}"),
    }
    assert!(server.cancel_send(&id3));
    match next_phone_msg(&mut phone).await {
        Message::FileCancel { transfer } => assert_eq!(transfer, id3),
        other => panic!("expected file_cancel, got {other:?}"),
    }
    let outcome = observe_send(&mut events, &id3).await;
    assert!(!outcome.ok);
    assert_eq!(outcome.error.as_deref(), Some("cancelled"));
    assert!(!server.cancel_send(&id3));

    phone
        .send(&Message::Battery {
            level: 3,
            charging: false,
        })
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        LinkEvent::Battery {
            device_id: "phone-1".into(),
            level: 3,
            charging: false,
        }
    );
    let _ = std::fs::remove_dir_all(&file_dir);
}
