//! What the desktop remembers: its TLS identity and the phones paired with it.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A phone that has completed pairing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedDevice {
    pub id: String,
    pub name: String,
    /// Shared 32-byte key, base64url without padding.
    pub key: String,
    /// When pairing happened (ms since epoch).
    pub paired_at: i64,
}

impl PairedDevice {
    pub fn key_bytes(&self) -> Option<Vec<u8>> {
        URL_SAFE_NO_PAD.decode(&self.key).ok()
    }
}

/// The desktop's certificate and private key (both DER).
pub struct Identity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

/// Files in one folder: `cert.der`, `key.der`, `paired.json`.
pub struct Store {
    dir: PathBuf,
    devices: Mutex<Vec<PairedDevice>>,
}

impl Store {
    pub fn open(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let devices = match fs::read(dir.join("paired.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            devices: Mutex::new(devices),
        })
    }

    /// Load the TLS identity, creating a new self-signed one the first time.
    pub fn identity(&self) -> io::Result<Identity> {
        let (cert_path, key_path) = (self.dir.join("cert.der"), self.dir.join("key.der"));
        if let (Ok(cert_der), Ok(key_der)) = (fs::read(&cert_path), fs::read(&key_path)) {
            return Ok(Identity { cert_der, key_der });
        }
        let generated = rcgen::generate_simple_self_signed(vec!["seam.local".to_string()])
            .map_err(io::Error::other)?;
        let identity = Identity {
            cert_der: generated.cert.der().to_vec(),
            key_der: generated.key_pair.serialize_der(),
        };
        write_private(&key_path, &identity.key_der)?;
        fs::write(&cert_path, &identity.cert_der)?;
        Ok(identity)
    }

    pub fn devices(&self) -> Vec<PairedDevice> {
        self.devices.lock().unwrap().clone()
    }

    pub fn device(&self, id: &str) -> Option<PairedDevice> {
        self.devices
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.id == id)
            .cloned()
    }

    /// Add or replace a paired phone and save.
    pub fn upsert(&self, device: PairedDevice) -> io::Result<()> {
        let mut devices = self.devices.lock().unwrap();
        devices.retain(|d| d.id != device.id);
        devices.push(device);
        self.save(&devices)
    }

    /// Forget a phone. Returns whether it was paired.
    pub fn remove(&self, id: &str) -> io::Result<bool> {
        let mut devices = self.devices.lock().unwrap();
        let before = devices.len();
        devices.retain(|d| d.id != id);
        let removed = devices.len() != before;
        self.save(&devices)?;
        Ok(removed)
    }

    fn save(&self, devices: &[PairedDevice]) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(devices).map_err(io::Error::other)?;
        write_private(&self.dir.join("paired.json"), &json)
    }
}

/// Write a file only the current user can read (keys live in it).
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("seam-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn identity_is_created_once_and_reused() {
        let dir = temp_dir("identity");
        let first = Store::open(&dir).unwrap().identity().unwrap();
        let second = Store::open(&dir).unwrap().identity().unwrap();
        assert!(!first.cert_der.is_empty());
        assert_eq!(first.cert_der, second.cert_der);
        assert_eq!(first.key_der, second.key_der);
    }

    #[test]
    fn devices_persist_and_can_be_removed() {
        let dir = temp_dir("devices");
        let store = Store::open(&dir).unwrap();
        let d = PairedDevice {
            id: "phone-1".into(),
            name: "Galaxy S24 Ultra".into(),
            key: URL_SAFE_NO_PAD.encode([7u8; 32]),
            paired_at: 1,
        };
        store.upsert(d.clone()).unwrap();
        let reopened = Store::open(&dir).unwrap();
        assert_eq!(reopened.device("phone-1"), Some(d));
        assert_eq!(
            reopened.device("phone-1").unwrap().key_bytes().unwrap(),
            [7u8; 32]
        );
        assert!(reopened.remove("phone-1").unwrap());
        assert!(Store::open(&dir).unwrap().devices().is_empty());
    }
}
