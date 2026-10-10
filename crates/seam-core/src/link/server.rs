//! The desktop side of the link: a TLS server phones connect to. Files a phone
//! sends are streamed into the download folder.

use super::protocol::{self, Message, PairingInfo, PhoneNotification, MAX_FRAME};
use super::store::{PairedDevice, Store};
use super::transfer;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::TlsAcceptor;

/// How long a QR pairing code stays valid.
pub const PAIRING_TTL: Duration = Duration::from_secs(10 * 60);
/// How often the desktop pings a connected phone.
pub const PING_INTERVAL: Duration = Duration::from_secs(30);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Something that happened on the link, for the app to show.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkEvent {
    Paired {
        device_id: String,
        name: String,
    },
    Connected {
        device_id: String,
        name: String,
    },
    Disconnected {
        device_id: String,
    },
    Notification {
        device_id: String,
        notification: PhoneNotification,
    },
    NotificationRemoved {
        device_id: String,
        id: String,
    },
    Battery {
        device_id: String,
        level: u8,
        charging: bool,
    },
    Clipboard {
        device_id: String,
        text: String,
    },
    /// Phone answered an inline notification reply.
    ReplyResult {
        device_id: String,
        id: String,
        ok: bool,
        error: Option<String>,
    },
    /// Phone reported a call. `ringing` shows the banner; `active` and `ended` hide it.
    Call {
        device_id: String,
        state: protocol::CallState,
        number: String,
        name: String,
    },
    /// A file from the phone was saved under the download folder.
    FileReceived {
        /// Paired device id of the phone that sent the file.
        phone: String,
        /// Final path of the saved file.
        path: PathBuf,
    },
}

struct Pending {
    key: [u8; 32],
    created: Instant,
}

struct Inner {
    store: Store,
    desktop_name: String,
    fingerprint: String,
    acceptor: TlsAcceptor,
    pending: Mutex<Option<Pending>>,
    events: mpsc::UnboundedSender<LinkEvent>,
    /// Outgoing queues of the phones connected right now.
    outboxes: Mutex<HashMap<String, mpsc::UnboundedSender<Message>>>,
    /// Folder received files are moved into. Temp files live here too.
    download_dir: PathBuf,
}

/// The link server. Cheap to clone; all clones share state.
#[derive(Clone)]
pub struct LinkServer {
    inner: Arc<Inner>,
}

impl LinkServer {
    /// Open (or create) the desktop's identity and paired phones in `dir`.
    pub fn new(
        dir: &Path,
        desktop_name: String,
    ) -> io::Result<(Self, mpsc::UnboundedReceiver<LinkEvent>)> {
        let store = Store::open(dir)?;
        let identity = store.identity()?;
        let fingerprint = protocol::fingerprint(&identity.cert_der);
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(io::Error::other)?
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(identity.cert_der)],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_der)),
            )
            .map_err(io::Error::other)?;
        let download_dir = dir.join("downloads");
        fs::create_dir_all(&download_dir)?;
        let (events, rx) = mpsc::unbounded_channel();
        let inner = Inner {
            store,
            desktop_name,
            fingerprint,
            acceptor: TlsAcceptor::from(Arc::new(config)),
            pending: Mutex::new(None),
            events,
            outboxes: Mutex::new(HashMap::new()),
            download_dir,
        };
        Ok((
            Self {
                inner: Arc::new(inner),
            },
            rx,
        ))
    }

    /// SHA-256 fingerprint of this desktop's certificate.
    pub fn fingerprint(&self) -> &str {
        &self.inner.fingerprint
    }

    /// Folder where a file received from a phone is saved.
    pub fn download_dir(&self) -> &Path {
        &self.inner.download_dir
    }

    pub fn desktop_name(&self) -> &str {
        &self.inner.desktop_name
    }

    pub fn paired_devices(&self) -> Vec<PairedDevice> {
        self.inner.store.devices()
    }

    /// Send a message to a connected phone. Returns false if it isn't connected.
    pub fn send_to(&self, device_id: &str, msg: Message) -> bool {
        self.inner
            .outboxes
            .lock()
            .unwrap()
            .get(device_id)
            .is_some_and(|tx| tx.send(msg).is_ok())
    }

    /// Ids of the phones connected right now.
    pub fn connected(&self) -> Vec<String> {
        self.inner
            .outboxes
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    pub fn forget(&self, device_id: &str) -> io::Result<bool> {
        self.inner.store.remove(device_id)
    }

    /// Start pairing: create a fresh one-time key and return the QR contents.
    pub fn start_pairing(&self, hosts: Vec<String>, port: u16) -> PairingInfo {
        let mut key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut key);
        *self.inner.pending.lock().unwrap() = Some(Pending {
            key,
            created: Instant::now(),
        });
        PairingInfo {
            hosts,
            port,
            fingerprint: self.inner.fingerprint.clone(),
            key,
            name: self.inner.desktop_name.clone(),
        }
    }

    /// Bind the first free port in `ports` on all interfaces.
    pub async fn bind(ports: std::ops::RangeInclusive<u16>) -> io::Result<TcpListener> {
        let mut last = io::Error::new(io::ErrorKind::AddrInUse, "no free port");
        for port in ports {
            match TcpListener::bind(("0.0.0.0", port)).await {
                Ok(l) => return Ok(l),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Accept phones forever.
    pub async fn serve(self, listener: TcpListener) {
        loop {
            let Ok((tcp, addr)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            };
            let server = self.clone();
            tokio::spawn(async move {
                if let Err(e) = server.handle(tcp).await {
                    eprintln!("Seam link: connection from {addr} ended: {e}");
                }
            });
        }
    }

    async fn handle(&self, tcp: TcpStream) -> io::Result<()> {
        let _ = tcp.set_nodelay(true);
        let tls = tokio::time::timeout(HANDSHAKE_TIMEOUT, self.inner.acceptor.accept(tcp))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timed out"))??;
        let (read_half, mut writer) = tokio::io::split(tls);
        let (mut frames, reader) = spawn_reader(read_half);
        // The reader task owns half of the connection; stop it when we're done so the
        // connection really closes.
        let _abort_reader = AbortOnDrop(reader);

        let mut nonce = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut nonce);
        let nonce = hex::encode(nonce);
        send(
            &mut writer,
            &Message::Challenge {
                v: protocol::PROTOCOL_VERSION,
                nonce: nonce.clone(),
            },
        )
        .await?;

        let hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, frames.recv())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no hello"))?;
        let Some(Ok(Message::Hello {
            device_id,
            name,
            proof,
        })) = hello
        else {
            return reject(&mut writer, "expected hello").await;
        };

        match self.authenticate(&device_id, &name, &nonce, &proof) {
            Ok(newly_paired) => {
                if newly_paired {
                    let _ = self.inner.events.send(LinkEvent::Paired {
                        device_id: device_id.clone(),
                        name: name.clone(),
                    });
                }
            }
            Err(reason) => return reject(&mut writer, reason).await,
        }

        send(
            &mut writer,
            &Message::Welcome {
                desktop_name: self.inner.desktop_name.clone(),
            },
        )
        .await?;
        let _ = self.inner.events.send(LinkEvent::Connected {
            device_id: device_id.clone(),
            name,
        });

        let (out_tx, mut outbox) = mpsc::unbounded_channel();
        self.inner
            .outboxes
            .lock()
            .unwrap()
            .insert(device_id.clone(), out_tx);
        let result = self
            .session(&device_id, &mut frames, &mut outbox, &mut writer)
            .await;
        self.inner.outboxes.lock().unwrap().remove(&device_id);
        let _ = self
            .inner
            .events
            .send(LinkEvent::Disconnected { device_id });
        result
    }

    /// Check the phone's proof. Returns whether this connection completed a new pairing.
    fn authenticate(
        &self,
        device_id: &str,
        name: &str,
        nonce: &str,
        proof: &str,
    ) -> Result<bool, &'static str> {
        if device_id.is_empty() || device_id.len() > 128 {
            return Err("invalid device id");
        }
        if let Some(known) = self.inner.store.device(device_id) {
            let key = known.key_bytes().ok_or("stored key is corrupt")?;
            if protocol::verify_proof(&key, nonce, device_id, proof) {
                return Ok(false);
            }
        }
        let mut pending = self.inner.pending.lock().unwrap();
        let valid = pending.as_ref().is_some_and(|p| {
            p.created.elapsed() < PAIRING_TTL
                && protocol::verify_proof(&p.key, nonce, device_id, proof)
        });
        if !valid {
            return Err("not paired: scan a new pairing code on the computer");
        }
        let key = pending.take().expect("checked above").key;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or_default();
        self.inner
            .store
            .upsert(PairedDevice {
                id: device_id.to_string(),
                name: name.chars().take(100).collect(),
                key: URL_SAFE_NO_PAD.encode(key),
                paired_at: now,
            })
            .map_err(|_| "could not save pairing")?;
        Ok(true)
    }

    /// Handle one inbound file frame. Other messages are returned so the session can emit them.
    async fn handle_file<W: tokio::io::AsyncWrite + Unpin>(
        &self,
        device_id: &str,
        inbound: &mut Option<transfer::Inbound>,
        msg: Message,
        writer: &mut W,
    ) -> io::Result<Option<Message>> {
        match msg {
            Message::FileOffer {
                transfer,
                name,
                size,
                ..
            } => {
                if let Err(reason) = transfer::check_offer(&transfer, size) {
                    send(
                        writer,
                        &Message::FileReject {
                            transfer,
                            reason: Some(reason.to_string()),
                        },
                    )
                    .await?;
                } else {
                    // Drop a partial first so its cleanup cannot remove the new temp file.
                    inbound.take();
                    match transfer::Inbound::open(&self.inner.download_dir, &transfer, &name, size)
                    {
                        Ok(file) => {
                            *inbound = Some(file);
                            send(writer, &Message::FileAccept { transfer }).await?;
                        }
                        Err(reason) => {
                            send(
                                writer,
                                &Message::FileReject {
                                    transfer,
                                    reason: Some(reason),
                                },
                            )
                            .await?;
                        }
                    }
                }
                Ok(None)
            }
            Message::FileChunk {
                transfer,
                seq,
                data,
            } => {
                let failed = match inbound.as_mut() {
                    Some(file) => file.push_chunk(&transfer, seq, &data).err(),
                    None => Some("no transfer".to_string()),
                };
                if let Some(reason) = failed {
                    inbound.take();
                    send(
                        writer,
                        &Message::FileResult {
                            transfer,
                            ok: false,
                            error: Some(reason),
                        },
                    )
                    .await?;
                }
                Ok(None)
            }
            Message::FileDone { transfer, sha256 } => {
                let result = match inbound.take() {
                    Some(file) => file.finish(&self.inner.download_dir, &transfer, &sha256),
                    None => Err("no transfer".to_string()),
                };
                match result {
                    Ok(path) => {
                        send(
                            writer,
                            &Message::FileResult {
                                transfer,
                                ok: true,
                                error: None,
                            },
                        )
                        .await?;
                        let _ = self.inner.events.send(LinkEvent::FileReceived {
                            phone: device_id.to_string(),
                            path,
                        });
                    }
                    Err(reason) => {
                        send(
                            writer,
                            &Message::FileResult {
                                transfer,
                                ok: false,
                                error: Some(reason),
                            },
                        )
                        .await?;
                    }
                }
                Ok(None)
            }
            Message::FileCancel { transfer } => {
                if inbound
                    .as_ref()
                    .is_some_and(|file| file.is_transfer(&transfer))
                {
                    inbound.take();
                }
                Ok(None)
            }
            other => Ok(Some(other)),
        }
    }

    async fn session<W: tokio::io::AsyncWrite + Unpin>(
        &self,
        device_id: &str,
        frames: &mut mpsc::Receiver<Result<Message, String>>,
        outbox: &mut mpsc::UnboundedReceiver<Message>,
        writer: &mut W,
    ) -> io::Result<()> {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await;
        let mut missed_pongs = 0u8;
        // At most one inbound file. Dropping it deletes a partial download.
        let mut inbound: Option<transfer::Inbound> = None;
        loop {
            tokio::select! {
                frame = frames.recv() => {
                    let msg = match frame {
                        None => return Ok(()),
                        Some(Err(e)) => return Err(io::Error::new(io::ErrorKind::InvalidData, e)),
                        Some(Ok(m)) => m,
                    };
                    // A forgotten phone is disconnected on its next message.
                    if self.inner.store.device(device_id).is_none() {
                        return reject(writer, "this phone was removed on the computer").await;
                    }
                    let Some(msg) = self
                        .handle_file(device_id, &mut inbound, msg, writer)
                        .await?
                    else {
                        continue;
                    };
                    let id = device_id.to_string();
                    let event = match msg {
                        Message::Notification(n) => Some(LinkEvent::Notification { device_id: id, notification: n }),
                        Message::NotificationRemoved { id: nid } => Some(LinkEvent::NotificationRemoved { device_id: id, id: nid }),
                        Message::Battery { level, charging } => Some(LinkEvent::Battery { device_id: id, level: level.min(100), charging }),
                        Message::Clipboard { text } => Some(LinkEvent::Clipboard { device_id: id, text }),
                        Message::ReplyResult { id: nid, ok, error } => Some(LinkEvent::ReplyResult {
                            device_id: id,
                            id: nid,
                            ok,
                            error,
                        }),
                        Message::Call { state, number, name } => Some(LinkEvent::Call {
                            device_id: id,
                            state,
                            number,
                            name,
                        }),
                        Message::Pong => { missed_pongs = 0; None }
                        Message::Ping => { send(writer, &Message::Pong).await?; None }
                        _ => None,
                    };
                    if let Some(e) = event {
                        let _ = self.inner.events.send(e);
                    }
                }
                Some(msg) = outbox.recv() => {
                    send(writer, &msg).await?;
                }
                _ = ping.tick() => {
                    missed_pongs += 1;
                    if missed_pongs > 3 {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "phone stopped answering"));
                    }
                    send(writer, &Message::Ping).await?;
                }
            }
        }
    }
}

/// Aborts a task when dropped.
pub(crate) struct AbortOnDrop(pub(crate) tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn send<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, msg: &Message) -> io::Result<()> {
    let mut line = msg.to_line();
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.flush().await
}

async fn reject<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, reason: &str) -> io::Result<()> {
    let _ = send(
        w,
        &Message::Error {
            message: reason.to_string(),
        },
    )
    .await;
    let _ = w.shutdown().await;
    Err(io::Error::new(io::ErrorKind::PermissionDenied, reason))
}

/// Read newline-delimited frames on a separate task, so reading never has to be
/// cancelled mid-line. Lines longer than `MAX_FRAME` end the connection.
pub(crate) fn spawn_reader<R: AsyncRead + Unpin + Send + 'static>(
    read_half: R,
) -> (
    mpsc::Receiver<Result<Message, String>>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::channel(64);
    let task = tokio::spawn(async move {
        let mut reader = BufReader::new(read_half);
        loop {
            let mut buf = Vec::new();
            let n = match (&mut reader)
                .take(MAX_FRAME as u64 + 1)
                .read_until(b'\n', &mut buf)
                .await
            {
                Ok(n) => n,
                Err(e) => {
                    let _ = tx.send(Err(e.to_string())).await;
                    return;
                }
            };
            if n == 0 {
                return;
            }
            if buf.len() > MAX_FRAME {
                let _ = tx.send(Err("frame too large".into())).await;
                return;
            }
            let line = String::from_utf8_lossy(&buf);
            if line.trim().is_empty() {
                continue;
            }
            if tx.send(Message::from_line(&line)).await.is_err() {
                return;
            }
        }
    });
    (rx, task)
}

/// The computer's address on the local network, as phones would reach it.
pub fn local_ip() -> Option<String> {
    // Connecting a UDP socket sends nothing; it just picks the outgoing interface.
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(a) if !a.ip().is_loopback() && !a.ip().is_unspecified() => {
            Some(a.ip().to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{LinkEvent, LinkServer};
    use crate::link::transfer::{Inbound, MAX_FILE_BYTES};
    use crate::link::Message;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use sha2::{Digest, Sha256};

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "seam-file-srv-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn partials(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".part"))
            .collect();
        names.sort();
        names
    }

    async fn drive(
        server: &LinkServer,
        inbound: &mut Option<Inbound>,
        msg: Message,
    ) -> Option<Message> {
        let mut writer = Vec::new();
        let returned = server
            .handle_file("phone-1", inbound, msg, &mut writer)
            .await
            .unwrap();
        assert!(returned.is_none(), "file messages are consumed");
        let text = String::from_utf8(writer).unwrap();
        if text.trim().is_empty() {
            None
        } else {
            Some(Message::from_line(&text).unwrap())
        }
    }

    #[tokio::test]
    async fn handle_file_saves_on_success_and_deletes_on_failure() {
        let dir = scratch("recv");
        let (server, mut events) = LinkServer::new(&dir, "Desk".into()).unwrap();
        let download = server.download_dir().to_path_buf();
        let mut inbound = None;
        let id = "ab".repeat(16);

        let huge = drive(
            &server,
            &mut inbound,
            Message::FileOffer {
                transfer: id.clone(),
                name: "huge.bin".into(),
                size: MAX_FILE_BYTES + 1,
                mime: None,
            },
        )
        .await;
        assert!(matches!(huge, Some(Message::FileReject { .. })));
        assert!(inbound.is_none());

        let accept = drive(
            &server,
            &mut inbound,
            Message::FileOffer {
                transfer: id.clone(),
                name: "../../etc/passwd".into(),
                size: 5,
                mime: None,
            },
        )
        .await;
        assert_eq!(
            accept,
            Some(Message::FileAccept {
                transfer: id.clone()
            })
        );
        assert!(drive(
            &server,
            &mut inbound,
            Message::FileChunk {
                transfer: id.clone(),
                seq: 0,
                data: STANDARD.encode(b"hello"),
            },
        )
        .await
        .is_none());
        let hash = hex::encode(Sha256::digest(b"hello"));
        let done = drive(
            &server,
            &mut inbound,
            Message::FileDone {
                transfer: id.clone(),
                sha256: hash,
            },
        )
        .await;
        assert_eq!(
            done,
            Some(Message::FileResult {
                transfer: id.clone(),
                ok: true,
                error: None,
            })
        );
        match events.try_recv().unwrap() {
            LinkEvent::FileReceived { phone, path } => {
                assert_eq!(phone, "phone-1");
                assert_eq!(path, download.join("passwd"));
                assert_eq!(std::fs::read(&path).unwrap(), b"hello");
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(partials(&download).is_empty());
        assert!(inbound.is_none());

        let bad = "cd".repeat(16);
        assert!(matches!(
            drive(
                &server,
                &mut inbound,
                Message::FileOffer {
                    transfer: bad.clone(),
                    name: "evil.bin".into(),
                    size: 4,
                    mime: None,
                },
            )
            .await,
            Some(Message::FileAccept { .. })
        ));
        drive(
            &server,
            &mut inbound,
            Message::FileChunk {
                transfer: bad.clone(),
                seq: 0,
                data: STANDARD.encode(b"nope"),
            },
        )
        .await;
        let failed = drive(
            &server,
            &mut inbound,
            Message::FileDone {
                transfer: bad.clone(),
                sha256: "00".repeat(32),
            },
        )
        .await;
        assert!(matches!(
            failed,
            Some(Message::FileResult { ok: false, .. })
        ));
        assert!(!download.join("evil.bin").exists());
        assert_eq!(std::fs::read(download.join("passwd")).unwrap(), b"hello");
        assert!(partials(&download).is_empty());
        assert!(events.try_recv().is_err());

        let cancel_id = "ef".repeat(16);
        drive(
            &server,
            &mut inbound,
            Message::FileOffer {
                transfer: cancel_id.clone(),
                name: "partial.bin".into(),
                size: 4,
                mime: None,
            },
        )
        .await;
        drive(
            &server,
            &mut inbound,
            Message::FileChunk {
                transfer: cancel_id.clone(),
                seq: 0,
                data: STANDARD.encode(b"part"),
            },
        )
        .await;
        assert!(partials(&download).iter().any(|n| n.contains(&cancel_id)));
        assert!(drive(
            &server,
            &mut inbound,
            Message::FileCancel {
                transfer: cancel_id.clone(),
            },
        )
        .await
        .is_none());
        assert!(inbound.is_none());
        assert!(partials(&download).is_empty());
        assert!(!download.join("partial.bin").exists());

        let seq_id = "12".repeat(16);
        drive(
            &server,
            &mut inbound,
            Message::FileOffer {
                transfer: seq_id.clone(),
                name: "seq.bin".into(),
                size: 2,
                mime: None,
            },
        )
        .await;
        let wrong_seq = drive(
            &server,
            &mut inbound,
            Message::FileChunk {
                transfer: seq_id.clone(),
                seq: 1,
                data: STANDARD.encode(b"ab"),
            },
        )
        .await;
        assert!(matches!(
            wrong_seq,
            Some(Message::FileResult { ok: false, .. })
        ));
        assert!(inbound.is_none());
        assert!(!download.join("seq.bin").exists());
        assert!(partials(&download).is_empty());

        let big_id = "34".repeat(16);
        drive(
            &server,
            &mut inbound,
            Message::FileOffer {
                transfer: big_id.clone(),
                name: "big.bin".into(),
                size: 2,
                mime: None,
            },
        )
        .await;
        let too_big = drive(
            &server,
            &mut inbound,
            Message::FileChunk {
                transfer: big_id,
                seq: 0,
                data: STANDARD.encode(b"abcd"),
            },
        )
        .await;
        assert!(matches!(
            too_big,
            Some(Message::FileResult { ok: false, .. })
        ));
        assert!(!download.join("big.bin").exists());
        assert!(partials(&download).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
