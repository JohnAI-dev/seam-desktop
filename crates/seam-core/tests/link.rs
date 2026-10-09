//! End-to-end: a Rust "phone" pairs with the real link server over real TLS.

use seam_core::link::client::PhoneClient;
use seam_core::link::{LinkEvent, LinkServer, Message, PairingInfo, PhoneNotification};
use std::path::PathBuf;
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
