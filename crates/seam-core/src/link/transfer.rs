//! Helpers for receiving a file from a paired phone.
//!
//! See `protocol/PROTOCOL.md`, section "Sending files". Names are reduced to a single
//! path segment so a transfer can never write outside the download folder. [`Receiver`]
//! checks chunk order, size and SHA-256 without keeping the file in memory.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Files are at most 2 GiB (see the protocol).
pub const MAX_FILE_BYTES: u64 = 1u64 << 31;
/// Chunks carry at most 256 KiB of raw data.
pub const MAX_CHUNK_BYTES: usize = 256 * 1024;
/// Saved names are at most this many Unicode scalar values.
pub const MAX_NAME_CHARS: usize = 200;

/// Why a chunk or `file_done` was refused. The transfer must stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferError {
    /// `seq` was not the next expected index (0, 1, 2, ...).
    WrongSeq { expected: u64, got: u64 },
    /// This chunk would make the file longer than the offered `size`.
    TooManyBytes,
    /// `file_done` arrived before `size` bytes.
    Incomplete,
    /// SHA-256 did not match, or was not 64 hex characters.
    WrongHash,
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongSeq { expected, got } => {
                write!(f, "wrong sequence (expected {expected}, got {got})")
            }
            Self::TooManyBytes => f.write_str("too many bytes"),
            Self::Incomplete => f.write_str("incomplete file"),
            Self::WrongHash => f.write_str("wrong hash"),
        }
    }
}

impl std::error::Error for TransferError {}

/// Reduce `name` to a single file name that cannot escape a directory.
///
/// Both `/` and `\` are separators. The final segment is kept; `..`, `.` and leading
/// dots are stripped. An empty result becomes `file`. Longer than [`MAX_NAME_CHARS`]
/// is truncated without dropping the extension (the suffix after the last dot).
pub fn sanitize_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or("");
    let base: String = base
        .trim_start_matches('.')
        .chars()
        .filter(|c| !matches!(*c, '\0' | '/' | '\\'))
        .collect();
    let out = if base.is_empty() {
        "file".to_string()
    } else {
        truncate_name(&base, MAX_NAME_CHARS)
    };
    debug_assert!(!out.is_empty());
    debug_assert!(out.chars().count() <= MAX_NAME_CHARS);
    debug_assert!(!out.starts_with('.'));
    debug_assert!(!out.contains('/') && !out.contains('\\'));
    out
}

fn truncate_name(name: &str, max_chars: usize) -> String {
    if name.chars().count() <= max_chars {
        return name.to_string();
    }
    let (stem, ext) = split_stem_ext(name);
    let ext_chars = ext.chars().count();
    if ext.is_empty() || ext_chars >= max_chars {
        return name.chars().take(max_chars).collect();
    }
    let mut out: String = stem.chars().take(max_chars - ext_chars).collect();
    out.push_str(ext);
    out
}

/// Split on the last dot. The extension includes the dot; a leading dot is not an extension.
fn split_stem_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(idx) if idx > 0 => (&name[..idx], &name[idx..]),
        _ => (name, ""),
    }
}

/// Path of `name` inside `dir`, adding ` (1)`, ` (2)`, ... before the extension if taken.
///
/// `name` is sanitized first, so the result stays inside `dir`.
pub fn unique_destination(dir: &Path, name: &str) -> PathBuf {
    let name = sanitize_name(name);
    let candidate = first_free(dir, &name);
    if is_inside(dir, &candidate) {
        candidate
    } else {
        first_free(dir, "file")
    }
}

fn first_free(dir: &Path, name: &str) -> PathBuf {
    let plain = dir.join(name);
    if !plain.exists() {
        return plain;
    }
    let (stem, ext) = split_stem_ext(name);
    for n in 1..10_000 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!("{stem} ({n}){ext}"))
}

/// True when `path` is lexically inside `dir` and has no `..` component.
pub(crate) fn is_inside(dir: &Path, path: &Path) -> bool {
    path.starts_with(dir)
        && path
            .components()
            .all(|c| !matches!(c, std::path::Component::ParentDir))
}

/// Reject a `file_offer` that does not follow the protocol.
pub(crate) fn check_offer(transfer: &str, size: u64) -> Result<(), &'static str> {
    if transfer.len() != 32 || !transfer.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("invalid transfer id");
    }
    if size > MAX_FILE_BYTES {
        return Err("file is larger than 2 GiB");
    }
    Ok(())
}

/// Decode one `file_chunk` `data` field (standard base64) and enforce the 256 KiB cap.
pub(crate) fn decode_chunk(data: &str) -> Result<Vec<u8>, &'static str> {
    let bytes = STANDARD.decode(data).map_err(|_| "invalid chunk data")?;
    if bytes.len() > MAX_CHUNK_BYTES {
        return Err("chunk is too large");
    }
    Ok(bytes)
}

/// Streaming check of an inbound file: sequence, size, then SHA-256 at `file_done`.
///
/// Bytes are hashed as they are accepted and then discarded. A rejected chunk does not
/// change the expected sequence or the number of bytes accepted.
#[derive(Debug)]
pub struct Receiver {
    size: u64,
    next_seq: u64,
    received: u64,
    hasher: Sha256,
}

impl Receiver {
    /// Expect exactly `size` bytes, starting at `seq` 0.
    pub fn new(size: u64) -> Self {
        Self {
            size,
            next_seq: 0,
            received: 0,
            hasher: Sha256::new(),
        }
    }

    /// Bytes accepted so far. Never greater than `size`.
    pub fn received(&self) -> u64 {
        self.received
    }

    /// Accept the next chunk. `seq` must be 0, then 1, then 2, ...
    pub fn push(&mut self, seq: u64, data: &[u8]) -> Result<(), TransferError> {
        if seq != self.next_seq {
            return Err(TransferError::WrongSeq {
                expected: self.next_seq,
                got: seq,
            });
        }
        let len = data.len() as u64;
        let new_total = self.received.saturating_add(len);
        if new_total > self.size {
            return Err(TransferError::TooManyBytes);
        }
        self.hasher.update(data);
        self.received = new_total;
        self.next_seq += 1;
        Ok(())
    }

    /// Check that every offered byte arrived and matches `sha256_hex` (64 hex chars).
    pub fn finish(&self, sha256_hex: &str) -> Result<(), TransferError> {
        if self.received != self.size {
            return Err(TransferError::Incomplete);
        }
        let actual = hex::encode(self.hasher.clone().finalize());
        if sha256_hex.len() != actual.len() || !sha256_hex.eq_ignore_ascii_case(&actual) {
            return Err(TransferError::WrongHash);
        }
        Ok(())
    }
}

/// One file being received. Chunks are written to a temp file as they arrive.
///
/// Dropping this before [`Inbound::finish`] deletes the partial file.
pub(crate) struct Inbound {
    transfer: String,
    name: String,
    temp: PathBuf,
    file: Option<File>,
    receiver: Receiver,
    committed: bool,
}

impl Inbound {
    /// Create a temp file in `dir` for a validated offer.
    pub(crate) fn open(dir: &Path, transfer: &str, name: &str, size: u64) -> Result<Self, String> {
        check_offer(transfer, size)?;
        fs::create_dir_all(dir)
            .map_err(|e| format!("could not create the download folder: {e}"))?;
        let temp = partial_path(dir, transfer);
        if !is_inside(dir, &temp) {
            return Err("invalid download path".into());
        }
        let file =
            File::create(&temp).map_err(|e| format!("could not create the download file: {e}"))?;
        Ok(Self {
            transfer: transfer.to_string(),
            name: sanitize_name(name),
            temp,
            file: Some(file),
            receiver: Receiver::new(size),
            committed: false,
        })
    }

    pub(crate) fn is_transfer(&self, transfer: &str) -> bool {
        self.transfer == transfer
    }

    /// Write the next chunk. A rejected chunk is not written and does not advance state.
    pub(crate) fn push_chunk(
        &mut self,
        transfer: &str,
        seq: u64,
        data: &str,
    ) -> Result<(), String> {
        if self.transfer != transfer {
            return Err("wrong transfer".into());
        }
        let bytes = decode_chunk(data)?;
        self.receiver.push(seq, &bytes).map_err(|e| e.to_string())?;
        let out = match self.file.as_mut() {
            Some(out) => out,
            None => return Err("could not save the file".into()),
        };
        out.write_all(&bytes)
            .map_err(|e| format!("could not save the file: {e}"))?;
        Ok(())
    }

    /// Check size and SHA-256, then move the temp file into `dir`. On error the partial is deleted.
    pub(crate) fn finish(
        mut self,
        dir: &Path,
        transfer: &str,
        sha256: &str,
    ) -> Result<PathBuf, String> {
        if self.transfer != transfer {
            return Err("wrong transfer".into());
        }
        if let Err(e) = self.receiver.finish(sha256) {
            return Err(e.to_string());
        }
        self.file.take();
        let written = fs::metadata(&self.temp)
            .map_err(|e| format!("could not save the file: {e}"))?
            .len();
        if written != self.receiver.received() {
            return Err("could not save the file".into());
        }
        fs::create_dir_all(dir)
            .map_err(|e| format!("could not create the download folder: {e}"))?;
        let dest = unique_destination(dir, &self.name);
        if !is_inside(dir, &dest) {
            return Err("invalid download path".into());
        }
        fs::rename(&self.temp, &dest).map_err(|e| format!("could not save the file: {e}"))?;
        self.committed = true;
        Ok(dest)
    }
}

impl Drop for Inbound {
    fn drop(&mut self) {
        self.file.take();
        if !self.committed {
            let _ = fs::remove_file(&self.temp);
        }
    }
}

fn partial_path(dir: &Path, transfer: &str) -> PathBuf {
    static NEXT_PARTIAL: AtomicU64 = AtomicU64::new(1);
    let n = NEXT_PARTIAL.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".{transfer}.{n}.part"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use sha2::{Digest, Sha256};

    fn sha256_hex(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "seam-xfer-{label}-{}-{}",
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

    fn part_files(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".part"))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn sanitize_name_strips_path_dots_and_limits_length() {
        assert_eq!(MAX_NAME_CHARS, 200);
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name(r"a\b"), "b");
        assert_eq!(sanitize_name(".hidden"), "hidden");
        assert_eq!(sanitize_name(""), "file");
        assert_eq!(sanitize_name(".."), "file");
        assert_eq!(sanitize_name("."), "file");
        assert_eq!(sanitize_name("..."), "file");
        assert_eq!(sanitize_name("/"), "file");
        assert_eq!(sanitize_name(r"\"), "file");
        assert_eq!(sanitize_name("foo/bar"), "bar");
        assert_eq!(sanitize_name("foo/"), "foo");
        assert_eq!(sanitize_name(r"..\..\etc\passwd"), "passwd");
        assert_eq!(sanitize_name("dir/.hidden"), "hidden");
        assert_eq!(sanitize_name(r"dir\.hidden"), "hidden");
        assert_eq!(sanitize_name(".hidden.txt"), "hidden.txt");
        assert_eq!(sanitize_name("photo.jpg"), "photo.jpg");
        assert_eq!(sanitize_name("file..txt"), "file..txt");

        for name in ["../../etc/passwd", r"a\b", ".hidden", "", "..", "foo/bar"] {
            let got = sanitize_name(name);
            assert!(!got.is_empty(), "{name:?}");
            assert!(!got.starts_with('.'), "{got}");
            assert!(!got.contains('/'), "{got}");
            assert!(!got.contains('\\'), "{got}");
            assert!(got.chars().count() <= MAX_NAME_CHARS, "{got}");
        }

        let long = format!("{}.txt", "n".repeat(250));
        let got = sanitize_name(&long);
        assert_eq!(got.chars().count(), 200);
        assert!(got.ends_with(".txt"));
        assert_eq!(got, format!("{}.txt", "n".repeat(196)));

        let exact = format!("{}.txt", "a".repeat(196));
        assert_eq!(exact.chars().count(), 200);
        assert_eq!(sanitize_name(&exact), exact);
        assert_eq!(sanitize_name(&"b".repeat(250)), "b".repeat(200));

        let han = format!("{}.txt", "你".repeat(210));
        let got = sanitize_name(&han);
        assert_eq!(got.chars().count(), 200);
        assert!(got.ends_with(".txt"));
        assert_eq!(got, format!("{}.txt", "你".repeat(196)));

        let huge_ext = format!("a.{}", "e".repeat(250));
        assert_eq!(sanitize_name(&huge_ext).chars().count(), 200);
    }

    #[test]
    fn unique_destination_adds_numeric_suffix_for_duplicates() {
        let dir = scratch("dup");
        let first = unique_destination(&dir, "photo.jpg");
        assert_eq!(first, dir.join("photo.jpg"));
        std::fs::write(&first, b"a").unwrap();
        assert_eq!(
            unique_destination(&dir, "photo.jpg"),
            dir.join("photo (1).jpg")
        );
        std::fs::write(dir.join("photo (1).jpg"), b"b").unwrap();
        assert_eq!(
            unique_destination(&dir, "photo.jpg"),
            dir.join("photo (2).jpg")
        );

        std::fs::write(dir.join("file"), b"c").unwrap();
        assert_eq!(unique_destination(&dir, "file"), dir.join("file (1)"));
        assert_eq!(unique_destination(&dir, ""), dir.join("file (1)"));

        std::fs::write(dir.join("passwd"), b"d").unwrap();
        assert_eq!(
            unique_destination(&dir, "../../etc/passwd"),
            dir.join("passwd (1)")
        );
        let escaped = unique_destination(&dir, r"a\b");
        assert_eq!(escaped, dir.join("b"));
        assert!(escaped.starts_with(&dir));
        assert!(is_inside(&dir, &escaped));

        std::fs::write(dir.join("archive.tar.gz"), b"e").unwrap();
        assert_eq!(
            unique_destination(&dir, "archive.tar.gz"),
            dir.join("archive.tar (1).gz")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_inside_rejects_parent_segments() {
        let dir = PathBuf::from("/tmp/seam-downloads");
        assert!(is_inside(&dir, &dir.join("passwd")));
        assert!(!is_inside(&dir, &dir.join("..").join("etc")));
        assert!(!is_inside(&dir, &PathBuf::from("/etc/passwd")));
    }

    #[test]
    fn offer_and_chunk_limits() {
        let id = "ab".repeat(16);
        assert_eq!(id.len(), 32);
        assert!(check_offer(&id, 0).is_ok());
        assert!(check_offer(&id, MAX_FILE_BYTES).is_ok());
        assert_eq!(
            check_offer(&id, MAX_FILE_BYTES + 1),
            Err("file is larger than 2 GiB")
        );
        assert_eq!(check_offer("short", 1), Err("invalid transfer id"));
        assert_eq!(check_offer(&"g".repeat(32), 1), Err("invalid transfer id"));

        assert_eq!(decode_chunk("@@@"), Err("invalid chunk data"));
        assert_eq!(decode_chunk("aGVsbG8=").unwrap(), b"hello");
        let big = vec![7u8; MAX_CHUNK_BYTES + 1];
        assert_eq!(
            decode_chunk(&STANDARD.encode(&big)),
            Err("chunk is too large")
        );
        let max = vec![7u8; MAX_CHUNK_BYTES];
        assert_eq!(decode_chunk(&STANDARD.encode(&max)).unwrap(), max);
    }

    #[test]
    fn receiver_rejects_wrong_seq() {
        let mut r = Receiver::new(3);
        assert_eq!(
            r.push(1, b"a"),
            Err(TransferError::WrongSeq {
                expected: 0,
                got: 1
            })
        );
        assert_eq!(r.received(), 0);
        r.push(0, b"a").unwrap();
        assert_eq!(
            r.push(0, b"b"),
            Err(TransferError::WrongSeq {
                expected: 1,
                got: 0
            })
        );
        assert_eq!(
            r.push(2, b"b"),
            Err(TransferError::WrongSeq {
                expected: 1,
                got: 2
            })
        );
        r.push(1, b"bc").unwrap();
        assert_eq!(r.received(), 3);
        r.finish(&sha256_hex(b"abc")).unwrap();
    }

    #[test]
    fn receiver_never_accepts_more_bytes_than_size() {
        let mut r = Receiver::new(4);
        assert_eq!(r.push(0, b"12345"), Err(TransferError::TooManyBytes));
        assert_eq!(r.received(), 0);
        r.push(0, b"123").unwrap();
        assert_eq!(r.received(), 3);
        assert_eq!(r.push(1, b"45"), Err(TransferError::TooManyBytes));
        assert_eq!(r.received(), 3);
        r.push(1, b"4").unwrap();
        assert_eq!(r.received(), 4);
        assert_eq!(r.push(2, b"x"), Err(TransferError::TooManyBytes));
        assert_eq!(r.received(), 4);
        r.finish(&sha256_hex(b"1234")).unwrap();
    }

    #[test]
    fn receiver_checks_sha256_at_file_done() {
        let mut r = Receiver::new(11);
        r.push(0, b"hello ").unwrap();
        r.push(1, b"world").unwrap();
        assert_eq!(r.finish(&"ab".repeat(32)), Err(TransferError::WrongHash));
        assert_eq!(r.finish("nope"), Err(TransferError::WrongHash));
        let good = sha256_hex(b"hello world");
        r.finish(&good).unwrap();
        r.finish(&good.to_ascii_uppercase()).unwrap();

        let mut short = Receiver::new(5);
        short.push(0, b"hey").unwrap();
        assert_eq!(
            short.finish(&sha256_hex(b"hey")),
            Err(TransferError::Incomplete)
        );
    }

    #[test]
    fn empty_file_hash_is_checked() {
        let empty = sha256_hex(b"");
        Receiver::new(0).finish(&empty).unwrap();
        assert_eq!(
            Receiver::new(0).finish(&"00".repeat(32)),
            Err(TransferError::WrongHash)
        );
    }

    #[test]
    fn inbound_file_strips_paths_and_deletes_a_wrong_hash() {
        let dir = scratch("inbound");
        let id = "ab".repeat(16);
        let mut inbound = Inbound::open(&dir, &id, "../../etc/passwd", 5).unwrap();
        assert_eq!(inbound.name, "passwd");
        inbound
            .push_chunk(&id, 0, &STANDARD.encode(b"hello"))
            .unwrap();
        let path = inbound.finish(&dir, &id, &sha256_hex(b"hello")).unwrap();
        assert_eq!(path, dir.join("passwd"));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert!(part_files(&dir).is_empty());

        let id = "cd".repeat(16);
        let mut inbound = Inbound::open(&dir, &id, "evil.bin", 4).unwrap();
        inbound
            .push_chunk(&id, 0, &STANDARD.encode(b"nope"))
            .unwrap();
        let err = inbound.finish(&dir, &id, &"00".repeat(32)).unwrap_err();
        assert!(err.contains("wrong hash"), "{err}");
        assert!(!dir.join("evil.bin").exists());
        assert_eq!(std::fs::read(dir.join("passwd")).unwrap(), b"hello");
        assert!(part_files(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inbound_rejects_wrong_seq_and_extra_bytes_without_writing_them() {
        let dir = scratch("chunks");
        let id = "ef".repeat(16);
        let mut inbound = Inbound::open(&dir, &id, "f.bin", 3).unwrap();
        let err = inbound
            .push_chunk(&id, 1, &STANDARD.encode(b"a"))
            .unwrap_err();
        assert!(err.contains("wrong sequence"), "{err}");
        assert_eq!(std::fs::read(&inbound.temp).unwrap(), b"");
        inbound.push_chunk(&id, 0, &STANDARD.encode(b"12")).unwrap();
        let err = inbound
            .push_chunk(&id, 1, &STANDARD.encode(b"345"))
            .unwrap_err();
        assert!(err.contains("too many"), "{err}");
        assert_eq!(std::fs::read(&inbound.temp).unwrap(), b"12");
        let temp = inbound.temp.clone();
        drop(inbound);
        assert!(!temp.exists());
        assert!(!dir.join("f.bin").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dropping_inbound_deletes_the_partial() {
        let dir = scratch("drop");
        let id = "11".repeat(16);
        let inbound = Inbound::open(&dir, &id, "partial.bin", 4).unwrap();
        let temp = inbound.temp.clone();
        assert!(temp.exists());
        drop(inbound);
        assert!(!temp.exists());
        assert!(!dir.join("partial.bin").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_file_is_saved() {
        let dir = scratch("empty");
        let id = "aa".repeat(16);
        let inbound = Inbound::open(&dir, &id, ".hidden", 0).unwrap();
        assert_eq!(inbound.name, "hidden");
        let path = inbound.finish(&dir, &id, &sha256_hex(b"")).unwrap();
        assert_eq!(path, dir.join("hidden"));
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        assert!(part_files(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
