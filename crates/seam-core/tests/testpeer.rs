//! Runs the `seam-testpeer` binary and pairs the in-process test phone with it.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use seam_core::link::client::PhoneClient;
use seam_core::link::{Message, PairingInfo, PhoneNotification};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn peer_bin() -> &'static str {
    // Cargo 1.99 keeps the hyphen in this variable. Older Cargo rewrote it to an underscore.
    option_env!("CARGO_BIN_EXE_seam-testpeer")
        .or(option_env!("CARGO_BIN_EXE_seam_testpeer"))
        .expect("Cargo should set the seam-testpeer binary path")
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "seam-testpeer-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("free port");
    listener.local_addr().expect("local addr").port()
}

struct Peer {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_key_file(dir: &Path, contents: &str) -> PathBuf {
    let path = dir.join("pairing-key");
    std::fs::create_dir_all(dir).expect("key directory");
    std::fs::write(&path, contents).expect("write key file");
    path
}

impl Peer {
    fn start(port: u16, key: &[u8; 32], cert_dir: &Path) -> Self {
        let key_path = write_key_file(cert_dir, &URL_SAFE_NO_PAD.encode(key));
        let mut child = Command::new(peer_bin())
            .args([
                "--port",
                &port.to_string(),
                "--key-file",
                &key_path.display().to_string(),
                "--cert-dir",
                &cert_dir.display().to_string(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn seam-testpeer");
        let stdout = child.stdout.take().expect("stdout");
        let mut stderr = child.stderr.take().expect("stderr");
        let stdin = child.stdin.take().expect("stdin");
        // Drain stderr so a chatty peer cannot fill the pipe and stall.
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        });
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            lines: rx,
        }
    }

    fn read_line(&self, timeout: Duration) -> String {
        self.lines
            .recv_timeout(timeout)
            .unwrap_or_else(|_| panic!("timed out after {timeout:?} waiting for seam-testpeer"))
    }

    fn pairing_link(&self) -> PairingInfo {
        let line = self.read_line(Duration::from_secs(5));
        assert!(
            line.starts_with("seam://pair?"),
            "first stdout line should be the pairing link, got {line}"
        );
        assert!(
            line.contains("host=10.0.2.2"),
            "pairing link should use the emulator host, got {line}"
        );
        PairingInfo::parse(&line).expect("pairing link should parse")
    }

    fn wait_for(&self, timeout: Duration, pred: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for a matching seam-testpeer line"
            );
            let line = self.read_line(remaining);
            if pred(&line) {
                return line;
            }
        }
    }

    fn write_line(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        writeln!(stdin, "{line}").expect("write stdin");
        stdin.flush().expect("flush stdin");
    }
}

#[tokio::test]
async fn peer_logs_a_notification_and_forwards_stdin() {
    let dir = temp_dir("notify");
    let port = free_port();
    let key = [7u8; 32];
    let mut peer = Peer::start(port, &key, &dir);
    let info = peer.pairing_link();
    assert_eq!(info.port, port);
    assert_eq!(info.key, key);
    assert_eq!(info.hosts, vec!["10.0.2.2".to_string()]);

    let mut phone = PhoneClient::connect(&info, "127.0.0.1", &key, "phone-1", "Pixel")
        .await
        .expect("pairing with seam-testpeer");
    assert_eq!(phone.desktop_name, "Seam Test Peer");

    let title = "Seam test note";
    phone
        .send(&Message::Notification(PhoneNotification {
            id: "n1".into(),
            app: "com.example".into(),
            app_name: "Example".into(),
            title: title.into(),
            text: "hello from the test phone".into(),
            time: 1,
            replyable: false,
        }))
        .await
        .expect("send notification");

    let logged = peer.wait_for(Duration::from_secs(5), |line| {
        line.contains("\"type\":\"notification\"") && line.contains(title)
    });
    assert!(logged.contains(title));

    let dismiss = Message::Dismiss { id: "n1".into() };
    peer.write_line(&dismiss.to_line());
    let got = tokio::time::timeout(Duration::from_secs(5), phone.recv())
        .await
        .expect("timed out waiting for the dismiss frame")
        .expect("phone connection closed before dismiss");
    assert_eq!(got, dismiss);
}

#[test]
fn same_cert_dir_keeps_the_fingerprint() {
    let dir = temp_dir("cert");
    let key = [1u8; 32];
    let first = {
        let peer = Peer::start(free_port(), &key, &dir);
        peer.pairing_link().fingerprint
    };
    let second = {
        let peer = Peer::start(free_port(), &key, &dir);
        peer.pairing_link().fingerprint
    };
    assert_eq!(first.len(), 64);
    assert_eq!(first, second);
}

#[test]
fn bad_key_exits_nonzero_without_a_pairing_link() {
    let dir = temp_dir("bad");
    let key_path = write_key_file(&dir, "not-a-key");
    let mut child = Command::new(peer_bin())
        .args([
            "--port",
            "9",
            "--key-file",
            &key_path.display().to_string(),
            "--cert-dir",
            &dir.display().to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn seam-testpeer");
    let status = wait_for_exit(&mut child, Duration::from_secs(5));
    assert!(
        status.map(|code| !code.success()).unwrap_or(false),
        "a bad key should exit non-zero"
    );
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    assert!(
        !stdout.contains("seam://pair"),
        "startup failure must not print a pairing link, got {stdout}"
    );
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    assert!(
        stderr.contains("seam-testpeer:"),
        "startup error should be reported, got {stderr}"
    );
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return Some(status);
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
