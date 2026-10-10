//! The desktop side of the link: a TLS server phones connect to. Files a phone
//! sends are streamed into the download folder. Files sent to a phone are read
//! from disk and written one chunk at a time so the connection stays responsive.

use super::protocol::{self, Message, PairingInfo, PhoneNotification, MAX_FRAME};
use super::store::{PairedDevice, Store};
use super::transfer;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};
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
/// Connections that have not finished the handshake (through `welcome`).
const MAX_UNAUTHENTICATED: usize = 16;
/// Largest frame accepted before `welcome`. The hello frame is small.
const PRE_AUTH_MAX_FRAME: usize = 4 * 1024;
/// How long to wait for `file_accept` or `file_reject`.
const FILE_ACCEPT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long to wait for `file_result` after `file_done`.
const FILE_RESULT_TIMEOUT: Duration = Duration::from_secs(180);
/// One queued file frame, so a send cannot fill memory or hold the writer.
const FILE_QUEUE: usize = 1;

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
    /// Bytes of an outbound file have been handed to the phone's connection.
    FileSendProgress {
        phone: String,
        transfer: String,
        name: String,
        sent: u64,
        size: u64,
    },
    /// An outbound file transfer ended. `ok` means the phone checked the hash.
    FileSendFinished {
        phone: String,
        transfer: String,
        name: String,
        ok: bool,
        error: Option<String>,
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
    /// Paced file frames for each connected phone. Capacity is one chunk.
    file_outs: Mutex<HashMap<String, mpsc::Sender<Message>>>,
    /// Phone replies for outbound transfers, keyed by transfer id.
    outbound: Mutex<HashMap<String, OutboundSlot>>,
    /// Folder received files are moved into. Temp files live here too.
    download_dir: PathBuf,
    /// Connections that have not completed the handshake.
    unauth: Mutex<usize>,
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
            file_outs: Mutex::new(HashMap::new()),
            outbound: Mutex::new(HashMap::new()),
            download_dir,
            unauth: Mutex::new(0),
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

    /// Start sending the file at `path` to a connected phone.
    ///
    /// Returns the transfer id (32 hex characters) once the send has been handed
    /// off. Chunks are written one at a time so pings and other messages keep
    /// flowing. Progress and the outcome arrive as [`LinkEvent::FileSendProgress`]
    /// and [`LinkEvent::FileSendFinished`].
    ///
    /// Fails immediately if the phone is not connected, `path` is not a readable
    /// file, or the file is larger than 2 GiB.
    pub fn send_file(&self, phone_id: &str, path: &Path) -> Result<String, String> {
        if self.file_sender(phone_id).is_none() {
            return Err("this phone is not connected".into());
        }
        let prepared = prepare_send(path)?;
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| "the phone link is not running".to_string())?;
        // Register before the offer is sent, so a fast reply cannot be missed.
        let (transfer, control) = self.allocate_transfer(phone_id);
        let job = SendJob {
            server: self.clone(),
            phone: phone_id.to_string(),
            transfer: transfer.clone(),
            name: prepared.name,
            size: prepared.size,
            mime: prepared.mime,
            file: prepared.file,
            control,
            reported: false,
        };
        runtime.spawn(async move {
            job.run().await;
        });
        Ok(transfer)
    }

    /// Give up on an outbound transfer from [`Self::send_file`].
    ///
    /// Sends `file_cancel` if it is still running. Returns false if the transfer
    /// is unknown or already finished.
    pub fn cancel_send(&self, transfer: &str) -> bool {
        let tx = self
            .inner
            .outbound
            .lock()
            .unwrap()
            .get(transfer)
            .map(|slot| slot.tx.clone());
        tx.is_some_and(|tx| tx.send(PhoneFileReply::LocalCancel).is_ok())
    }

    fn file_sender(&self, phone: &str) -> Option<mpsc::Sender<Message>> {
        self.inner.file_outs.lock().unwrap().get(phone).cloned()
    }

    fn allocate_transfer(&self, phone: &str) -> (String, mpsc::UnboundedReceiver<PhoneFileReply>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut outbound = self.inner.outbound.lock().unwrap();
        let transfer = loop {
            let id = new_transfer_id();
            if !outbound.contains_key(&id) {
                break id;
            }
        };
        outbound.insert(
            transfer.clone(),
            OutboundSlot {
                phone: phone.to_string(),
                tx,
            },
        );
        (transfer, rx)
    }

    fn remove_outbound(&self, transfer: &str) {
        self.inner.outbound.lock().unwrap().remove(transfer);
    }

    fn fail_outbound(&self, phone: &str) {
        let mut outbound = self.inner.outbound.lock().unwrap();
        let ids: Vec<String> = outbound
            .iter()
            .filter(|(_, slot)| slot.phone == phone)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(slot) = outbound.remove(&id) {
                let _ = slot.tx.send(PhoneFileReply::Disconnected);
            }
        }
    }

    fn notify_outbound(&self, transfer: &str, reply: PhoneFileReply) -> bool {
        let tx = self
            .inner
            .outbound
            .lock()
            .unwrap()
            .get(transfer)
            .map(|slot| slot.tx.clone());
        match tx {
            Some(tx) => tx.send(reply).is_ok(),
            None => false,
        }
    }

    /// Route a phone reply for an outbound transfer. Other messages are returned.
    fn route_outbound(&self, msg: Message) -> Option<Message> {
        match msg {
            Message::FileAccept { transfer } => {
                self.notify_outbound(&transfer, PhoneFileReply::Accept);
                None
            }
            Message::FileReject { transfer, reason } => {
                self.notify_outbound(&transfer, PhoneFileReply::Reject { reason });
                None
            }
            Message::FileResult {
                transfer,
                ok,
                error,
            } => {
                self.notify_outbound(&transfer, PhoneFileReply::Result { ok, error });
                None
            }
            Message::FileCancel { transfer } => {
                if self.notify_outbound(&transfer, PhoneFileReply::RemoteCancel) {
                    None
                } else {
                    Some(Message::FileCancel { transfer })
                }
            }
            other => Some(other),
        }
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
    ///
    /// At most 16 connections that have not finished the handshake are kept.
    /// Extra sockets are closed immediately.
    pub async fn serve(self, listener: TcpListener) {
        loop {
            let Ok((tcp, addr)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            };
            let Some(admission) = self.try_admit() else {
                drop(tcp);
                continue;
            };
            let server = self.clone();
            tokio::spawn(async move {
                if let Err(e) = server.handle(tcp, admission).await {
                    eprintln!("Seam link: connection from {addr} ended: {e}");
                }
            });
        }
    }

    /// Reserve a slot for a connection that has not finished the handshake.
    fn try_admit(&self) -> Option<UnauthGuard> {
        let mut n = self.inner.unauth.lock().unwrap();
        if *n >= MAX_UNAUTHENTICATED {
            return None;
        }
        *n += 1;
        Some(UnauthGuard {
            server: self.clone(),
            held: true,
        })
    }

    async fn handle(&self, tcp: TcpStream, mut admission: UnauthGuard) -> io::Result<()> {
        let _ = tcp.set_nodelay(true);
        let tls = tokio::time::timeout(HANDSHAKE_TIMEOUT, self.inner.acceptor.accept(tcp))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timed out"))??;
        let (read_half, mut writer) = tokio::io::split(tls);
        // Before welcome, only a small frame is accepted. The hello is tiny; a large
        // line is closed here so it never reaches the 1 MiB session reader.
        let mut pre_auth = BufReader::new(read_half);

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

        let hello_line = match tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            read_limited_line(&mut pre_auth, PRE_AUTH_MAX_FRAME),
        )
        .await
        {
            Ok(Ok(line)) => line,
            Ok(Err(e)) => {
                let _ = writer.shutdown().await;
                return Err(e);
            }
            Err(_) => {
                let _ = writer.shutdown().await;
                return Err(io::Error::new(io::ErrorKind::TimedOut, "no hello"));
            }
        };
        let (device_id, name, proof) = match Message::from_line(&hello_line) {
            Ok(Message::Hello {
                device_id,
                name,
                proof,
            }) => (device_id, name, proof),
            _ => return reject(&mut writer, "expected hello").await,
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

        // Authenticated: this connection no longer counts toward the pre-auth limit.
        // Later frames use the normal 1 MiB reader.
        admission.release();
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

        let (mut frames, reader) = spawn_reader(pre_auth);
        // The reader task owns half of the connection; stop it when we're done so the
        // connection really closes.
        let _abort_reader = AbortOnDrop(reader);

        let (out_tx, mut outbox) = mpsc::unbounded_channel();
        let (file_tx, mut file_out) = mpsc::channel(FILE_QUEUE);
        self.inner
            .outboxes
            .lock()
            .unwrap()
            .insert(device_id.clone(), out_tx);
        self.inner
            .file_outs
            .lock()
            .unwrap()
            .insert(device_id.clone(), file_tx);
        let result = self
            .session(
                &device_id,
                &mut frames,
                &mut outbox,
                &mut file_out,
                &mut writer,
            )
            .await;
        self.inner.outboxes.lock().unwrap().remove(&device_id);
        self.inner.file_outs.lock().unwrap().remove(&device_id);
        drop(file_out);
        self.fail_outbound(&device_id);
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
        file_out: &mut mpsc::Receiver<Message>,
        writer: &mut W,
    ) -> io::Result<()> {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await;
        let mut missed_pongs = 0u8;
        // At most one inbound file. Dropping it deletes a partial download.
        let mut inbound: Option<transfer::Inbound> = None;
        loop {
            // File frames are last so a transfer cannot starve pings or other messages.
            // One chunk is written per turn; the writer is never held for the whole file.
            tokio::select! {
                biased;
                _ = ping.tick() => {
                    missed_pongs += 1;
                    if missed_pongs > 3 {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "phone stopped answering"));
                    }
                    send(writer, &Message::Ping).await?;
                }
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
                    let Some(msg) = self.route_outbound(msg) else {
                        continue;
                    };
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
                Some(msg) = file_out.recv() => {
                    send(writer, &msg).await?;
                }
            }
        }
    }
}

#[derive(Debug)]
enum PhoneFileReply {
    Accept,
    Reject { reason: Option<String> },
    Result { ok: bool, error: Option<String> },
    RemoteCancel,
    LocalCancel,
    Disconnected,
    TimedOut,
}

struct OutboundSlot {
    phone: String,
    tx: mpsc::UnboundedSender<PhoneFileReply>,
}

struct PreparedSend {
    file: fs::File,
    name: String,
    size: u64,
    mime: Option<String>,
}

fn prepare_send(path: &Path) -> Result<PreparedSend, String> {
    let file = fs::File::open(path).map_err(|e| format!("could not open the file: {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("could not read the file: {e}"))?;
    if !meta.is_file() {
        return Err("not a file".into());
    }
    let size = meta.len();
    transfer::check_file_size(size).map_err(|reason| reason.to_string())?;
    let name = transfer::offer_name(path);
    Ok(PreparedSend {
        mime: transfer::guess_mime(&name).map(str::to_string),
        name,
        size,
        file,
    })
}

fn new_transfer_id() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn reject_error(reason: Option<String>) -> String {
    match reason {
        Some(text) if !text.is_empty() => text,
        _ => "rejected".into(),
    }
}

fn result_error(error: Option<String>) -> String {
    match error {
        Some(text) if !text.is_empty() => text,
        _ => "phone reported a failure".into(),
    }
}

async fn send_cancel(file_tx: &mpsc::Sender<Message>, transfer: &str) -> bool {
    file_tx
        .send(Message::FileCancel {
            transfer: transfer.to_string(),
        })
        .await
        .is_ok()
}

struct SendJob {
    server: LinkServer,
    phone: String,
    transfer: String,
    name: String,
    size: u64,
    mime: Option<String>,
    file: fs::File,
    control: mpsc::UnboundedReceiver<PhoneFileReply>,
    reported: bool,
}

impl SendJob {
    async fn run(mut self) {
        let Some(file_tx) = self.server.file_sender(&self.phone) else {
            self.finish(false, Some("phone disconnected".into()));
            return;
        };
        self.emit_progress(0);
        let offer = Message::FileOffer {
            transfer: self.transfer.clone(),
            name: self.name.clone(),
            size: self.size,
            mime: self.mime.clone(),
        };
        if file_tx.send(offer).await.is_err() {
            self.finish(false, Some("phone disconnected".into()));
            return;
        }
        match self.wait_reply(FILE_ACCEPT_TIMEOUT, false).await {
            PhoneFileReply::Accept => {}
            other => {
                self.stop(other, &file_tx, true).await;
                return;
            }
        }
        // A result or cancel queued behind accept must not start the bytes.
        if let Some(reply) = self.poll_stop() {
            self.stop(reply, &file_tx, true).await;
            return;
        }
        if !self.stream(&file_tx).await {
            return;
        }
        match self.wait_reply(FILE_RESULT_TIMEOUT, true).await {
            PhoneFileReply::Result { ok: true, .. } => self.finish(true, None),
            PhoneFileReply::Result { ok: false, error } => {
                self.finish(false, Some(result_error(error)));
            }
            other => self.stop(other, &file_tx, false).await,
        }
    }

    /// Returns false when the transfer was already finished.
    async fn stream(&mut self, file_tx: &mpsc::Sender<Message>) -> bool {
        let mut hasher = Sha256::new();
        let mut seq = 0u64;
        let mut sent = 0u64;
        while sent < self.size {
            if let Some(reply) = self.poll_stop() {
                self.stop(reply, file_tx, true).await;
                return false;
            }
            let max = (self.size - sent).min(transfer::MAX_CHUNK_BYTES as u64) as usize;
            let chunk = match transfer::read_next_chunk(&mut self.file, max) {
                Ok(Some(chunk)) => chunk,
                Ok(None) => {
                    let _ = send_cancel(file_tx, &self.transfer).await;
                    self.finish(false, Some("could not read the file".into()));
                    return false;
                }
                Err(e) => {
                    let _ = send_cancel(file_tx, &self.transfer).await;
                    self.finish(false, Some(format!("could not read the file: {e}")));
                    return false;
                }
            };
            hasher.update(&chunk);
            let msg = Message::FileChunk {
                transfer: self.transfer.clone(),
                seq,
                data: STANDARD.encode(&chunk),
            };
            if file_tx.send(msg).await.is_err() {
                self.finish(false, Some("phone disconnected".into()));
                return false;
            }
            sent += chunk.len() as u64;
            seq += 1;
            self.emit_progress(sent);
        }
        if let Some(reply) = self.poll_stop() {
            self.stop(reply, file_tx, true).await;
            return false;
        }
        let done = Message::FileDone {
            transfer: self.transfer.clone(),
            sha256: hex::encode(hasher.finalize()),
        };
        if file_tx.send(done).await.is_err() {
            self.finish(false, Some("phone disconnected".into()));
            return false;
        }
        true
    }

    async fn wait_reply(&mut self, limit: Duration, ignore_accept: bool) -> PhoneFileReply {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return PhoneFileReply::TimedOut;
            }
            match tokio::time::timeout(left, self.control.recv()).await {
                Ok(Some(PhoneFileReply::Accept)) if ignore_accept => continue,
                Ok(Some(reply)) => return reply,
                Ok(None) => return PhoneFileReply::Disconnected,
                Err(_) => return PhoneFileReply::TimedOut,
            }
        }
    }

    fn poll_stop(&mut self) -> Option<PhoneFileReply> {
        loop {
            match self.control.try_recv() {
                Ok(PhoneFileReply::Accept) => continue,
                Ok(other) => return Some(other),
                Err(mpsc::error::TryRecvError::Empty) => return None,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Some(PhoneFileReply::Disconnected);
                }
            }
        }
    }

    async fn stop(
        &mut self,
        reply: PhoneFileReply,
        file_tx: &mpsc::Sender<Message>,
        before_done: bool,
    ) {
        match reply {
            PhoneFileReply::Reject { reason } => {
                self.finish(false, Some(reject_error(reason)));
            }
            PhoneFileReply::Result { .. } if before_done => {
                let _ = send_cancel(file_tx, &self.transfer).await;
                self.finish(false, Some("unexpected file_result".into()));
            }
            PhoneFileReply::Result { ok: true, .. } => self.finish(true, None),
            PhoneFileReply::Result { ok: false, error } => {
                self.finish(false, Some(result_error(error)));
            }
            PhoneFileReply::RemoteCancel => self.finish(false, Some("cancelled".into())),
            PhoneFileReply::LocalCancel => {
                let _ = send_cancel(file_tx, &self.transfer).await;
                self.finish(false, Some("cancelled".into()));
            }
            PhoneFileReply::TimedOut => {
                let _ = send_cancel(file_tx, &self.transfer).await;
                self.finish(false, Some("timed out waiting for the phone".into()));
            }
            PhoneFileReply::Disconnected => {
                self.finish(false, Some("phone disconnected".into()));
            }
            PhoneFileReply::Accept => {
                self.finish(false, Some("unexpected file_accept".into()));
            }
        }
    }

    fn emit_progress(&self, sent: u64) {
        let _ = self.server.inner.events.send(LinkEvent::FileSendProgress {
            phone: self.phone.clone(),
            transfer: self.transfer.clone(),
            name: self.name.clone(),
            sent,
            size: self.size,
        });
    }

    fn finish(&mut self, ok: bool, error: Option<String>) {
        if self.reported {
            return;
        }
        self.reported = true;
        self.server.remove_outbound(&self.transfer);
        let _ = self.server.inner.events.send(LinkEvent::FileSendFinished {
            phone: self.phone.clone(),
            transfer: self.transfer.clone(),
            name: self.name.clone(),
            ok,
            error: if ok { None } else { error },
        });
    }
}

impl Drop for SendJob {
    fn drop(&mut self) {
        if !self.reported {
            self.finish(false, Some("phone disconnected".into()));
        }
    }
}

/// Holds one pre-auth connection slot until the handshake finishes or the task ends.
struct UnauthGuard {
    server: LinkServer,
    held: bool,
}

impl UnauthGuard {
    fn release(&mut self) {
        if !self.held {
            return;
        }
        self.held = false;
        let mut n = self.server.inner.unauth.lock().unwrap();
        *n = n.saturating_sub(1);
    }
}

impl Drop for UnauthGuard {
    fn drop(&mut self) {
        self.release();
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

/// Read one non-empty line, refusing anything longer than `limit` bytes.
async fn read_limited_line<R>(reader: &mut R, limit: usize) -> io::Result<String>
where
    R: AsyncRead + Unpin,
{
    loop {
        let mut buf = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            let n = reader.read(&mut byte).await?;
            if n == 0 {
                break;
            }
            buf.push(byte[0]);
            if buf.len() > limit {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame too large",
                ));
            }
            if byte[0] == b'\n' {
                break;
            }
        }
        if buf.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed",
            ));
        }
        let line = String::from_utf8_lossy(&buf);
        if line.trim().is_empty() {
            continue;
        }
        return Ok(line.into_owned());
    }
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
    use super::{LinkEvent, LinkServer, PhoneFileReply};
    use crate::link::client::PhoneClient;
    use crate::link::store::PairedDevice;
    use crate::link::transfer::{Inbound, MAX_CHUNK_BYTES, MAX_FILE_BYTES};
    use crate::link::Message;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use sha2::{Digest, Sha256};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::sync::mpsc;

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

    fn connect_phone(server: &LinkServer, phone: &str) -> mpsc::Receiver<Message> {
        let (tx, rx) = mpsc::channel(super::FILE_QUEUE);
        server
            .inner
            .file_outs
            .lock()
            .unwrap()
            .insert(phone.to_string(), tx);
        rx
    }

    fn pair_phone(server: &LinkServer, id: &str) {
        server
            .inner
            .store
            .upsert(PairedDevice {
                id: id.to_string(),
                name: "P".into(),
                key: "k".into(),
                paired_at: 1,
            })
            .unwrap();
    }

    async fn next_out(rx: &mut mpsc::Receiver<Message>) -> Message {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for an outbound frame")
            .expect("file queue closed")
    }

    async fn finished_event(events: &mut mpsc::UnboundedReceiver<LinkEvent>) -> LinkEvent {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let event = tokio::time::timeout_at(deadline, events.recv())
                .await
                .expect("timed out waiting for the send to finish")
                .expect("events closed");
            if !matches!(event, LinkEvent::FileSendProgress { .. }) {
                return event;
            }
        }
    }

    async fn read_frame_line(reader: &mut (impl tokio::io::AsyncRead + Unpin)) -> String {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut line = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                let n = reader.read(&mut byte).await.expect("read");
                assert_ne!(n, 0, "connection closed");
                if byte[0] == b'\n' {
                    return String::from_utf8(line).unwrap();
                }
                line.push(byte[0]);
            }
        })
        .await
        .expect("timed out reading a frame")
    }

    #[tokio::test]
    async fn send_file_rejects_bad_paths_and_a_missing_phone() {
        let dir = scratch("bad");
        let (server, _events) = LinkServer::new(&dir, "Desk".into()).unwrap();
        let path = dir.join("note.txt");
        std::fs::write(&path, b"hi").unwrap();
        assert_eq!(
            server.send_file("phone-1", &path).unwrap_err(),
            "this phone is not connected"
        );
        let _rx = connect_phone(&server, "phone-1");
        let missing = server
            .send_file("phone-1", &dir.join("nope.bin"))
            .unwrap_err();
        assert!(missing.contains("could not open the file"), "{missing}");
        let sub = dir.join("subdir");
        std::fs::create_dir(&sub).unwrap();
        assert_eq!(server.send_file("phone-1", &sub).unwrap_err(), "not a file");
        assert!(!server.cancel_send(&"ab".repeat(16)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn outbound_queue_streams_a_file_and_reports_progress() {
        let dir = scratch("stream");
        let (server, mut events) = LinkServer::new(&dir, "Desk".into()).unwrap();
        let mut rx = connect_phone(&server, "phone-1");
        let nested = dir.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let path = nested.join("photo.jpg");
        let payload = vec![9u8; MAX_CHUNK_BYTES + 3];
        std::fs::write(&path, &payload).unwrap();

        let id = server.send_file("phone-1", &path).unwrap();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        match next_out(&mut rx).await {
            Message::FileOffer {
                transfer,
                name,
                size,
                mime,
            } => {
                assert_eq!(transfer, id);
                assert_eq!(name, "photo.jpg");
                assert_eq!(size, payload.len() as u64);
                assert_eq!(mime.as_deref(), Some("image/jpeg"));
            }
            other => panic!("expected offer, got {other:?}"),
        }
        server.notify_outbound(&id, PhoneFileReply::Accept);

        let mut got = Vec::new();
        let mut seq = 0u64;
        loop {
            match next_out(&mut rx).await {
                Message::FileChunk {
                    transfer,
                    seq: got_seq,
                    data,
                } => {
                    assert_eq!(transfer, id);
                    assert_eq!(got_seq, seq);
                    let bytes = STANDARD.decode(data.as_bytes()).unwrap();
                    assert!(bytes.len() <= MAX_CHUNK_BYTES);
                    got.extend(bytes);
                    seq += 1;
                }
                Message::FileDone { transfer, sha256 } => {
                    assert_eq!(transfer, id);
                    assert_eq!(got, payload);
                    assert_eq!(sha256, hex::encode(Sha256::digest(&payload)));
                    assert!(seq >= 2, "file should be split into chunks, got {seq}");
                    break;
                }
                other => panic!("unexpected frame: {other:?}"),
            }
        }

        let mut progress = Vec::new();
        while let Ok(event) = events.try_recv() {
            match event {
                LinkEvent::FileSendProgress {
                    phone,
                    transfer,
                    name,
                    sent,
                    size,
                } => {
                    assert_eq!(phone, "phone-1");
                    assert_eq!(transfer, id);
                    assert_eq!(name, "photo.jpg");
                    assert_eq!(size, payload.len() as u64);
                    assert!(sent <= size);
                    progress.push(sent);
                }
                other => panic!("unexpected event before result: {other:?}"),
            }
        }
        assert_eq!(progress.first().copied(), Some(0));
        assert_eq!(progress.last().copied(), Some(payload.len() as u64));

        server.notify_outbound(
            &id,
            PhoneFileReply::Result {
                ok: true,
                error: None,
            },
        );
        match finished_event(&mut events).await {
            LinkEvent::FileSendFinished {
                phone,
                transfer,
                name,
                ok: true,
                error: None,
            } => {
                assert_eq!(phone, "phone-1");
                assert_eq!(transfer, id);
                assert_eq!(name, "photo.jpg");
            }
            other => panic!("expected a successful finish, got {other:?}"),
        }
        assert!(!server.cancel_send(&id));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn empty_outbound_file_is_hashed_and_a_failed_result_is_reported() {
        let dir = scratch("empty-out");
        let (server, mut events) = LinkServer::new(&dir, "Desk".into()).unwrap();
        let mut rx = connect_phone(&server, "phone-1");
        let path = dir.join("empty.dat");
        std::fs::write(&path, b"").unwrap();
        let id = server.send_file("phone-1", &path).unwrap();
        match next_out(&mut rx).await {
            Message::FileOffer {
                transfer,
                name,
                size,
                mime,
            } => {
                assert_eq!(transfer, id);
                assert_eq!(name, "empty.dat");
                assert_eq!(size, 0);
                assert_eq!(mime, None);
            }
            other => panic!("expected offer, got {other:?}"),
        }
        server.notify_outbound(&id, PhoneFileReply::Accept);
        match next_out(&mut rx).await {
            Message::FileDone { transfer, sha256 } => {
                assert_eq!(transfer, id);
                assert_eq!(sha256, hex::encode(Sha256::digest(b"")));
            }
            other => panic!("expected file_done, got {other:?}"),
        }
        server.notify_outbound(
            &id,
            PhoneFileReply::Result {
                ok: false,
                error: Some("wrong hash".into()),
            },
        );
        match finished_event(&mut events).await {
            LinkEvent::FileSendFinished {
                ok: false,
                error,
                transfer,
                ..
            } => {
                assert_eq!(transfer, id);
                assert_eq!(error.as_deref(), Some("wrong hash"));
            }
            other => panic!("expected a failed result, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn outbound_queue_reject_and_cancel_end_the_transfer() {
        let dir = scratch("rej");
        let (server, mut events) = LinkServer::new(&dir, "Desk".into()).unwrap();
        let mut rx = connect_phone(&server, "phone-1");
        let path = dir.join("nope.txt");
        std::fs::write(&path, b"no thanks").unwrap();

        let id = server.send_file("phone-1", &path).unwrap();
        match next_out(&mut rx).await {
            Message::FileOffer { transfer, .. } => assert_eq!(transfer, id),
            other => panic!("expected offer, got {other:?}"),
        }
        server.notify_outbound(
            &id,
            PhoneFileReply::Reject {
                reason: Some("no space".into()),
            },
        );
        match finished_event(&mut events).await {
            LinkEvent::FileSendFinished {
                transfer,
                ok: false,
                error,
                ..
            } => {
                assert_eq!(transfer, id);
                assert_eq!(error.as_deref(), Some("no space"));
            }
            other => panic!("expected reject, got {other:?}"),
        }
        assert!(rx.try_recv().is_err());
        assert!(!server.cancel_send(&id));

        let path = dir.join("later.txt");
        std::fs::write(&path, b"later").unwrap();
        let id = server.send_file("phone-1", &path).unwrap();
        match next_out(&mut rx).await {
            Message::FileOffer { transfer, .. } => assert_eq!(transfer, id),
            other => panic!("expected offer, got {other:?}"),
        }
        assert!(server.cancel_send(&id));
        match next_out(&mut rx).await {
            Message::FileCancel { transfer } => assert_eq!(transfer, id),
            other => panic!("expected file_cancel, got {other:?}"),
        }
        match finished_event(&mut events).await {
            LinkEvent::FileSendFinished {
                transfer,
                ok: false,
                error,
                ..
            } => {
                assert_eq!(transfer, id);
                assert_eq!(error.as_deref(), Some("cancelled"));
            }
            other => panic!("expected cancel, got {other:?}"),
        }
        assert!(rx.try_recv().is_err());
        assert!(!server.cancel_send(&id));

        let id = server.send_file("phone-1", &path).unwrap();
        match next_out(&mut rx).await {
            Message::FileOffer { transfer, .. } => assert_eq!(transfer, id),
            other => panic!("expected offer, got {other:?}"),
        }
        server.notify_outbound(&id, PhoneFileReply::RemoteCancel);
        match finished_event(&mut events).await {
            LinkEvent::FileSendFinished {
                ok: false,
                error,
                transfer,
                ..
            } => {
                assert_eq!(transfer, id);
                assert_eq!(error.as_deref(), Some("cancelled"));
            }
            other => panic!("expected remote cancel, got {other:?}"),
        }
        assert!(rx.try_recv().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn file_result_before_file_done_fails_and_cancels() {
        let dir = scratch("early");
        let (server, mut events) = LinkServer::new(&dir, "Desk".into()).unwrap();
        let mut rx = connect_phone(&server, "phone-1");
        let path = dir.join("early.bin");
        std::fs::write(&path, b"early-bytes").unwrap();
        let id = server.send_file("phone-1", &path).unwrap();
        match next_out(&mut rx).await {
            Message::FileOffer { transfer, .. } => assert_eq!(transfer, id),
            other => panic!("expected offer, got {other:?}"),
        }
        server.notify_outbound(
            &id,
            PhoneFileReply::Result {
                ok: true,
                error: None,
            },
        );
        match next_out(&mut rx).await {
            Message::FileCancel { transfer } => assert_eq!(transfer, id),
            other => panic!("expected file_cancel, got {other:?}"),
        }
        match finished_event(&mut events).await {
            LinkEvent::FileSendFinished {
                transfer,
                ok: false,
                error,
                ..
            } => {
                assert_eq!(transfer, id);
                assert_eq!(error.as_deref(), Some("unexpected file_result"));
            }
            other => panic!("expected failure, got {other:?}"),
        }
        assert!(rx.try_recv().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn queue_drop_reports_a_handed_off_send_once() {
        let dir = scratch("dropq");
        let (server, mut events) = LinkServer::new(&dir, "Desk".into()).unwrap();
        let rx = connect_phone(&server, "phone-1");
        let path = dir.join("drop.bin");
        std::fs::write(&path, b"drop").unwrap();
        let id = server.send_file("phone-1", &path).unwrap();
        drop(rx);
        server.fail_outbound("phone-1");
        match finished_event(&mut events).await {
            LinkEvent::FileSendFinished {
                transfer,
                ok: false,
                error,
                ..
            } => {
                assert_eq!(transfer, id);
                assert_eq!(error.as_deref(), Some("phone disconnected"));
            }
            other => panic!("expected disconnect, got {other:?}"),
        }
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(!matches!(
            events.try_recv(),
            Ok(LinkEvent::FileSendFinished { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn session_handles_other_messages_before_queued_file_chunks() {
        let dir = scratch("fair");
        let (server, mut events) = LinkServer::new(&dir, "Desk".into()).unwrap();
        pair_phone(&server, "phone-1");

        let (frame_tx, mut frames) = mpsc::channel(4);
        frame_tx
            .try_send(Ok(Message::Battery {
                level: 7,
                charging: true,
            }))
            .unwrap();
        let (out_tx, mut outbox) = mpsc::unbounded_channel();
        out_tx.send(Message::Dismiss { id: "mid".into() }).unwrap();
        let (file_tx, mut file_rx) = mpsc::channel(1);
        file_tx
            .try_send(Message::FileChunk {
                transfer: "ab".repeat(16),
                seq: 0,
                data: STANDARD.encode(b"hello"),
            })
            .unwrap();

        let (mut reader, mut writer) = tokio::io::duplex(4096);
        let server2 = server.clone();
        let join = tokio::spawn(async move {
            server2
                .session(
                    "phone-1",
                    &mut frames,
                    &mut outbox,
                    &mut file_rx,
                    &mut writer,
                )
                .await
        });

        let first = read_frame_line(&mut reader).await;
        assert!(
            first.contains("dismiss"),
            "other messages must be written before file chunks, got {first}"
        );
        match events.try_recv() {
            Ok(LinkEvent::Battery {
                device_id,
                level: 7,
                charging: true,
            }) => assert_eq!(device_id, "phone-1"),
            other => panic!("expected battery before the next chunk, got {other:?}"),
        }
        let second = read_frame_line(&mut reader).await;
        assert!(
            second.contains("file_chunk"),
            "expected the file chunk after other messages, got {second}"
        );

        join.abort();
        drop(frame_tx);
        drop(out_tx);
        drop(file_tx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn at_most_sixteen_unauthenticated_slots() {
        let dir = scratch("slots");
        let (server, _events) = LinkServer::new(&dir, "Desk".into()).unwrap();
        let mut slots = Vec::new();
        for _ in 0..super::MAX_UNAUTHENTICATED {
            slots.push(server.try_admit().expect("slot"));
        }
        assert!(server.try_admit().is_none());
        assert_eq!(
            *server.inner.unauth.lock().unwrap(),
            super::MAX_UNAUTHENTICATED
        );
        slots.pop().unwrap().release();
        let extra = server.try_admit().expect("a freed slot can be reused");
        assert_eq!(
            *server.inner.unauth.lock().unwrap(),
            super::MAX_UNAUTHENTICATED
        );
        drop(extra);
        assert_eq!(
            *server.inner.unauth.lock().unwrap(),
            super::MAX_UNAUTHENTICATED - 1
        );
        drop(slots);
        assert_eq!(*server.inner.unauth.lock().unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn pre_auth_reader_rejects_lines_over_4_kib() {
        let limit = super::PRE_AUTH_MAX_FRAME;
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let mut reader = tokio::io::BufReader::new(server);
        let mut ok_line = vec![b'a'; limit - 1];
        ok_line.push(b'\n');
        client.write_all(&ok_line).await.unwrap();
        let got = super::read_limited_line(&mut reader, limit).await.unwrap();
        assert_eq!(got.trim().len(), limit - 1);

        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let mut reader = tokio::io::BufReader::new(server);
        let mut big = vec![b'b'; 10 * 1024];
        big.push(b'\n');
        tokio::spawn(async move {
            let _ = client.write_all(&big).await;
        });
        let err = super::read_limited_line(&mut reader, limit)
            .await
            .expect_err("10 KiB line");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("frame too large"));
    }

    async fn wait_unauth(server: &LinkServer, want: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let have = *server.inner.unauth.lock().unwrap();
            if have == want {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out waiting for {want} unauthenticated connections, have {have}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn e2e_unauth_limit_and_pre_auth_line() {
        let dir = scratch("unauth");
        let (server, _events) = LinkServer::new(&dir, "Desk".into()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(server.clone().serve(listener));

        let mut held = Vec::new();
        for _ in 0..super::MAX_UNAUTHENTICATED {
            held.push(
                TcpStream::connect(("127.0.0.1", port))
                    .await
                    .expect("connect"),
            );
        }
        wait_unauth(&server, super::MAX_UNAUTHENTICATED).await;

        let mut extra = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut byte = [0u8; 1];
        let n = tokio::time::timeout(Duration::from_secs(2), extra.read(&mut byte))
            .await
            .expect("17th connection should be closed immediately")
            .expect("read");
        assert_eq!(n, 0, "extra unauthenticated connection must be closed");
        assert_eq!(
            *server.inner.unauth.lock().unwrap(),
            super::MAX_UNAUTHENTICATED
        );

        drop(held);
        drop(extra);
        wait_unauth(&server, 0).await;

        let info = server.start_pairing(vec!["127.0.0.1".into()], port);
        let phone = tokio::time::timeout(
            Duration::from_secs(5),
            PhoneClient::connect(&info, "127.0.0.1", &info.key, "phone-1", "Pixel"),
        )
        .await
        .expect("timed out waiting for a real phone to connect")
        .expect("server should accept a real phone once raw connections close");
        assert_eq!(phone.desktop_name, "Desk");
        phone.close().await;

        let mut tls = crate::link::client::connect_tls(&info, "127.0.0.1")
            .await
            .expect("pinned tls");
        let mut saw_newline = false;
        let challenge_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !saw_newline && tokio::time::Instant::now() < challenge_deadline {
            let mut b = [0u8; 1];
            let n = tokio::time::timeout(Duration::from_secs(5), tls.read(&mut b))
                .await
                .expect("timed out reading the challenge")
                .expect("challenge");
            assert_ne!(n, 0, "closed before challenge");
            if b[0] == b'\n' {
                saw_newline = true;
            }
        }
        assert!(saw_newline, "expected the challenge line");

        let mut big = vec![b'x'; 10 * 1024];
        big.push(b'\n');
        let write_closed = tls.write_all(&big).await.is_err();
        if !write_closed {
            let _ = tls.flush().await;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            let mut closed = false;
            while tokio::time::Instant::now() < deadline {
                let mut buf = [0u8; 256];
                match tls.read(&mut buf).await {
                    Ok(0) | Err(_) => {
                        closed = true;
                        break;
                    }
                    Ok(_) => continue,
                }
            }
            assert!(closed, "a 10 KiB pre-auth line should close the connection");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
