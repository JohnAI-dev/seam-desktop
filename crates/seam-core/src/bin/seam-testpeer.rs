//! Desktop link peer for end-to-end tests.
//!
//! Runs a [`LinkServer`](seam_core::link::LinkServer) with a certificate stored in
//! `--cert-dir` and a one-time pairing key read from `--key-file` (so the key is not
//! visible in the process list). The key file must not be readable by group or others.
//! The pairing link (`seam://pair?...&key=...` with `host=10.0.2.2`) is written to
//! `--uri-file` with mode 0600 and is never printed. After that, every authenticated
//! message from the phone is one JSON line on stdout. Stdin is the same protocol, one
//! JSON frame per line, sent to the connected phone.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use seam_core::link::{LinkServer, Message};
use std::env;
use std::io::{self, BufRead, Write};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::ExitCode;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// Address an Android emulator uses for the host machine's loopback.
const EMULATOR_HOST: &str = "10.0.2.2";

struct Args {
    port: u16,
    key_path: PathBuf,
    state_dir: PathBuf,
    uri_path: PathBuf,
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
    let material = read_key_file(&args.key_path)?;
    let (server, events) = LinkServer::new(&args.state_dir, "Seam Test Peer".into())
        .map_err(|_| "could not open the certificate directory".to_string())?;
    // The peer logs protocol frames itself; link events are not part of its output.
    drop(events);

    let (log_tx, mut log_rx) = mpsc::unbounded_channel();
    server.log_received_messages(log_tx);

    // Bind before printing the link so a startup failure does not publish a dead port.
    // The emulator reaches this loopback listener through 10.0.2.2.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, args.port))
        .await
        .map_err(|_| format!("could not listen on port {}", args.port))?;
    let info = server.start_pairing_with_key(vec![EMULATOR_HOST.into()], args.port, material);
    // The pairing link contains the one-time key. Write it to a 0600 file and do not
    // print it: logs, CI output, and `ps` must not carry the key.
    write_uri_file(&args.uri_path, &info.to_uri())?;

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
        let line = line.map_err(|_| "could not read stdin".to_string())?;
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
    let mut key_path = None;
    let mut state_dir = None;
    let mut uri_path = None;
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
            "--key-file" => key_path = Some(PathBuf::from(value)),
            "--cert-dir" => state_dir = Some(PathBuf::from(value)),
            "--uri-file" => uri_path = Some(PathBuf::from(value)),
            "--key" => {
                return Err("--key is not accepted; pass --key-file so the key is not visible in the process list".into());
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        port: port.ok_or("missing --port")?,
        key_path: key_path.ok_or("missing --key-file")?,
        state_dir: state_dir.ok_or("missing --cert-dir")?,
        uri_path: uri_path.ok_or("missing --uri-file")?,
    })
}

/// Base64url, no padding, exactly 32 bytes. Same encoding as the pairing link.
fn decode_key(text: &str) -> Result<[u8; 32], String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(text.trim())
        .map_err(|_| "key is not base64url without padding".to_string())?;
    bytes
        .try_into()
        .map_err(|_| "key must be 32 bytes".to_string())
}

fn read_key_file(path: &std::path::Path) -> Result<[u8; 32], String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(|_| "could not read the key file".to_string())?
            .permissions()
            .mode();
        // Group and other permission bits. Owner-only (0600) is the required mode.
        if mode & 0o077 != 0 {
            return Err("key file must not be readable by group or others".into());
        }
    }
    let text =
        std::fs::read_to_string(path).map_err(|_| "could not read the key file".to_string())?;
    decode_key(&text)
}

/// Write the pairing link so only the current user can read it.
///
/// The link contains the one-time key, so this never prints it.
fn write_uri_file(path: &std::path::Path, uri: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .map_err(|_| "could not write the pairing link".to_string())?;
        writeln!(file, "{uri}").map_err(|_| "could not write the pairing link".to_string())?;
        file.flush()
            .map_err(|_| "could not write the pairing link".to_string())?;
    }
    #[cfg(unix)]
    restrict_to_owner(&tmp)?;
    std::fs::rename(&tmp, path).map_err(|_| "could not write the pairing link".to_string())?;
    #[cfg(unix)]
    restrict_to_owner(path)?;
    Ok(())
}

#[cfg(unix)]
fn restrict_to_owner(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|_| "could not restrict the pairing link".to_string())?;
    let mode = std::fs::metadata(path)
        .map_err(|_| "could not restrict the pairing link".to_string())?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        let _ = std::fs::remove_file(path);
        return Err("could not restrict the pairing link".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{decode_key, parse_args, Args};
    #[cfg(unix)]
    use super::{read_key_file, write_uri_file};
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use std::path::PathBuf;

    fn parse(list: &[&str]) -> Result<Args, String> {
        parse_args(list.iter().map(|part| (*part).to_string()))
    }

    #[test]
    fn parses_flags() {
        let parsed = parse(&[
            "--cert-dir",
            "/tmp/certs",
            "--key-file",
            "/tmp/key",
            "--uri-file",
            "/tmp/uri",
            "--port",
            "47100",
        ])
        .expect("flags");
        assert_eq!(parsed.port, 47100);
        assert_eq!(parsed.key_path, PathBuf::from("/tmp/key"));
        assert_eq!(parsed.state_dir, PathBuf::from("/tmp/certs"));
        assert_eq!(parsed.uri_path, PathBuf::from("/tmp/uri"));
    }

    #[test]
    fn rejects_bad_input() {
        let key = URL_SAFE_NO_PAD.encode([9u8; 32]);
        assert!(parse(&["--port", "1"]).is_err());
        assert!(parse(&[
            "--port",
            "0",
            "--key-file",
            "k",
            "--cert-dir",
            "d",
            "--uri-file",
            "u"
        ])
        .is_err());
        assert!(parse(&[
            "--port",
            "nope",
            "--key-file",
            "k",
            "--cert-dir",
            "d",
            "--uri-file",
            "u"
        ])
        .is_err());
        assert!(parse(&[
            "--port",
            "1",
            "--key",
            &key,
            "--cert-dir",
            "d",
            "--uri-file",
            "u"
        ])
        .is_err());
        assert!(parse(&["--port", "1", "--key-file", "k", "--cert-dir", "d"]).is_err());
        assert!(parse(&["--nope", "1"]).is_err());
        assert!(decode_key("aa").is_err());
        assert!(decode_key(&format!("{key}=")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn key_file_must_be_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "seam-testpeer-keymode-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("key dir");
        let path = dir.join("key");
        let key = URL_SAFE_NO_PAD.encode([3u8; 32]);
        std::fs::write(&path, &key).expect("write key");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("mode");
        let err = read_key_file(&path).expect_err("group-readable key");
        assert!(
            err.contains("group or others"),
            "loose key file should be rejected, got {err}"
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
        assert_eq!(read_key_file(&path).expect("owner-only key"), [3u8; 32]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn uri_file_is_owner_only_and_holds_the_link() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "seam-testpeer-urimode-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("uri dir");
        let path = dir.join("pairing.uri");
        let uri = "seam://pair?host=10.0.2.2&key=abc";
        write_uri_file(&path, uri).expect("write uri");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "uri mode {mode:o}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read uri").trim(),
            uri
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
