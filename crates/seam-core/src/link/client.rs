//! A phone-side client in Rust. The real phone app is Kotlin; this one lets the
//! desktop's tests (and future tools) act as a phone, following the same protocol.

use super::protocol::{self, Message, PairingInfo};
use super::server::spawn_reader;
use std::io;
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::CryptoProvider;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{DigitallySignedStruct, SignatureScheme};
use tokio_rustls::TlsConnector;

/// Accepts the server only if its certificate matches the fingerprint from the QR code.
#[derive(Debug)]
pub struct PinnedCert {
    fingerprint: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if protocol::fingerprint(end_entity.as_ref()) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "certificate does not match pairing".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// A connected, authenticated phone session.
pub struct PhoneClient {
    writer: WriteHalf<TlsStream<TcpStream>>,
    frames: mpsc::Receiver<Result<Message, String>>,
    pub desktop_name: String,
}

impl PhoneClient {
    /// Connect to `host` from the pairing info and authenticate with `key`.
    pub async fn connect(
        info: &PairingInfo,
        host: &str,
        key: &[u8],
        device_id: &str,
        name: &str,
    ) -> io::Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(io::Error::other)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCert {
                fingerprint: info.fingerprint.clone(),
                provider,
            }))
            .with_no_client_auth();
        let tcp = TcpStream::connect((host, info.port)).await?;
        let server_name = ServerName::try_from("seam.local").expect("valid name");
        let tls = TlsConnector::from(Arc::new(config))
            .connect(server_name, tcp)
            .await?;
        let (read_half, writer) = tokio::io::split(tls);
        let mut client = Self {
            writer,
            frames: spawn_reader(read_half),
            desktop_name: String::new(),
        };
        let nonce = match client.recv().await {
            Some(Message::Challenge { nonce, .. }) => nonce,
            other => return Err(protocol_error(format!("expected challenge, got {other:?}"))),
        };
        client
            .send(&Message::Hello {
                device_id: device_id.to_string(),
                name: name.to_string(),
                proof: protocol::proof(key, &nonce, device_id),
            })
            .await?;
        match client.recv().await {
            Some(Message::Welcome { desktop_name }) => {
                client.desktop_name = desktop_name;
                Ok(client)
            }
            Some(Message::Error { message }) => {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, message))
            }
            other => Err(protocol_error(format!("expected welcome, got {other:?}"))),
        }
    }

    pub async fn send(&mut self, msg: &Message) -> io::Result<()> {
        let mut line = msg.to_line();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await
    }

    /// Next message, or `None` when the connection closed.
    pub async fn recv(&mut self) -> Option<Message> {
        match self.frames.recv().await {
            Some(Ok(m)) => Some(m),
            _ => None,
        }
    }
}

fn protocol_error(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
