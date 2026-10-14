//! Desktop link peer for end-to-end tests.
//!
//! Runs a [`LinkServer`](seam_core::link::LinkServer) with a certificate stored in
//! `--cert-dir` and a one-time pairing key from `--key`. The first line on stdout is
//! the pairing link (`seam://pair?...` with `host=10.0.2.2`). After that, every
//! message received from the phone is one JSON line on stdout. Stdin is the same
//! protocol, one JSON frame per line, sent to the connected phone.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use seam_core::link::{LinkServer, Message};
use std::env;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// Address an Android emulator uses for the host machine.
const EMULATOR_HOST: &str = "10.0.2.2";

struct Args {
    port: u16,
    key: [u8; 32],
    cert_dir: PathBuf,
}

fn main() -> ExitCode {
    match peer() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("seam-testpeer: {err}");
            ExitCode::from(1)
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn peer() -> Result<(), String> {
    let args = parse_args(env::args().skip(1))?;
    let (server, events) =
        LinkServer::new(&args.cert_dir, "Seam Test Peer".into()).map_err(|err| err.to_string())?;
    // The peer logs protocol frames itself; link events are not part of its output.
    drop(events);

    let (log_tx, mut log_rx) = mpsc::unbounded_channel();
    server.log_received_messages(log_tx);

    // Bind before printing the link so a startup failure does not publish a dead port.
    let listener = TcpListener::bind(("0.0.0.0", args.port))
        .await
        .map_err(|err| format!("could not listen on port {}: {err}", args.port))?;
    let info = server.start_pairing_with_key(vec![EMULATOR_HOST.into()], args.port, args.key);
    println!("{}", info.to_uri());
    io::stdout().flush().map_err(|err| err.to_string())?;

    tokio::spawn(async move {
        while let Some(msg) = log_rx.recv().await {
            println!("{}", msg.to_line());
            let _ = io::stdout().flush();
        }
    });
    let running = server.clone();
    tokio::spawn(async move {
        running.serve(listener).await;
    });

    // Stdin is read on a blocking thread: this crate's Tokio build has no async stdio.
    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in io::BufReader::new(io::stdin()).lines() {
            if stdin_tx.send(line).is_err() {
                break;
            }
        }
    });
    while let Some(line) = stdin_rx.recv().await {
        let line = line.map_err(|err| err.to_string())?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg = match Message::from_line(line) {
            Ok(msg) => msg,
            Err(err) => {
                eprintln!("seam-testpeer: {err}");
                continue;
            }
        };
        let ids = server.connected();
        if ids.is_empty() {
            eprintln!("seam-testpeer: no phone connected");
            continue;
        }
        for id in ids {
            if !server.send_to(&id, msg.clone()) {
                eprintln!("seam-testpeer: could not send to {id}");
            }
        }
    }
    Ok(())
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut args = args.into_iter();
    let mut port = None;
    let mut key = None;
    let mut cert_dir = None;
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--port" => {
                let parsed: u16 = value.parse().map_err(|_| format!("invalid port {value}"))?;
                if parsed == 0 {
                    return Err("port must not be 0".into());
                }
                port = Some(parsed);
            }
            "--key" => key = Some(decode_key(&value)?),
            "--cert-dir" => cert_dir = Some(PathBuf::from(value)),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        port: port.ok_or("missing --port")?,
        key: key.ok_or("missing --key")?,
        cert_dir: cert_dir.ok_or("missing --cert-dir")?,
    })
}

/// Base64url, no padding, exactly 32 bytes. Same encoding as the pairing link.
fn decode_key(text: &str) -> Result<[u8; 32], String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_| "key is not base64url without padding".to_string())?;
    bytes
        .try_into()
        .map_err(|_| "key must be 32 bytes".to_string())
}

#[cfg(test)]
mod tests {
    use super::{decode_key, parse_args, Args};
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use std::path::PathBuf;

    fn parse(list: &[&str]) -> Result<Args, String> {
        parse_args(list.iter().map(|part| (*part).to_string()))
    }

    #[test]
    fn parses_flags() {
        let key = URL_SAFE_NO_PAD.encode([9u8; 32]);
        let parsed =
            parse(&["--cert-dir", "/tmp/certs", "--key", &key, "--port", "47100"]).expect("flags");
        assert_eq!(parsed.port, 47100);
        assert_eq!(parsed.key, [9u8; 32]);
        assert_eq!(parsed.cert_dir, PathBuf::from("/tmp/certs"));
    }

    #[test]
    fn rejects_bad_input() {
        let key = URL_SAFE_NO_PAD.encode([9u8; 32]);
        assert!(parse(&["--port", "1"]).is_err());
        assert!(parse(&["--port", "0", "--key", &key, "--cert-dir", "d"]).is_err());
        assert!(parse(&["--port", "1", "--key", "aa", "--cert-dir", "d"]).is_err());
        assert!(parse(&["--port", "nope", "--key", &key, "--cert-dir", "d"]).is_err());
        assert!(parse(&["--nope", "1"]).is_err());
        assert!(decode_key(&format!("{key}=")).is_err());
    }
}
