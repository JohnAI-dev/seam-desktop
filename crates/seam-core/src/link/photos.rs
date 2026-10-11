//! Recent photos announced by a paired phone.
//!
//! The phone sends `photo_new` (the 12 most recent on connect, oldest first, then new
//! ones as they are saved). The desktop keeps the last [`KEEP_PHOTOS`] per phone,
//! deduped by id, newest first. Requesting one sends `photo_request`; the phone then
//! uses the normal file transfer.

use super::protocol::Message;
use serde::{Deserialize, Serialize};

/// How many recent photos the window keeps for one phone.
pub const KEEP_PHOTOS: usize = 12;
/// Thumbnail JPEG is at most 32 KiB of raw bytes (standard base64 on the wire).
pub const MAX_THUMB_BYTES: usize = 32 * 1024;

/// One photo the phone has announced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentPhoto {
    /// Paired device id of the phone that announced it.
    pub phone: String,
    /// Stable id from the phone. Used to dedupe and to request the full file.
    pub id: String,
    /// File name only, as the phone sent it.
    pub name: String,
    /// Milliseconds since the epoch.
    pub taken: i64,
    /// Standard base64 JPEG thumbnail. Empty when the phone sent none, or it was too large.
    pub thumb: String,
}

/// Keep `incoming` in `photos` (newest first), replacing any earlier row with the same
/// phone and id, and drop anything past [`KEEP_PHOTOS`] for that phone.
///
/// Other phones are left in place. An empty id is ignored.
pub fn remember(photos: &mut Vec<RecentPhoto>, incoming: RecentPhoto) -> bool {
    if incoming.id.is_empty() || incoming.phone.is_empty() {
        return false;
    }
    photos.retain(|photo| photo.phone != incoming.phone || photo.id != incoming.id);
    photos.insert(0, incoming);
    trim_phone(photos, &photos[0].phone.clone());
    true
}

fn trim_phone(photos: &mut Vec<RecentPhoto>, phone: &str) {
    let mut kept = 0usize;
    photos.retain(|photo| {
        if photo.phone != phone {
            return true;
        }
        kept += 1;
        kept <= KEEP_PHOTOS
    });
}

/// Drop every photo announced by `phone`.
pub fn forget_phone(photos: &mut Vec<RecentPhoto>, phone: &str) {
    photos.retain(|photo| photo.phone != phone);
}

/// `photo_request` for a photo this phone announced. Unknown ids are an error.
pub fn request_message(
    photos: &[RecentPhoto],
    phone: &str,
    id: &str,
) -> Result<Message, &'static str> {
    if id.is_empty() {
        return Err("unknown photo");
    }
    if photos
        .iter()
        .any(|photo| photo.phone == phone && photo.id == id)
    {
        Ok(Message::PhotoRequest { id: id.to_string() })
    } else {
        Err("unknown photo")
    }
}

/// Keep a thumbnail only when it is standard base64 of at most [`MAX_THUMB_BYTES`] raw bytes.
///
/// Anything else becomes an empty string so a huge or corrupt thumb cannot be shown.
pub fn usable_thumb(thumb: &str) -> String {
    let thumb = thumb.trim();
    if thumb.is_empty() {
        return String::new();
    }
    match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, thumb) {
        Ok(bytes) if bytes.len() <= MAX_THUMB_BYTES => thumb.to_string(),
        _ => String::new(),
    }
}

/// `data:image/jpeg;base64,<thumb>` for an `<img>`, or `None` when there is no thumb.
pub fn thumb_data_url(thumb: &str) -> Option<String> {
    let thumb = usable_thumb(thumb);
    if thumb.is_empty() {
        None
    } else {
        Some(format!("data:image/jpeg;base64,{thumb}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn photo(phone: &str, id: &str, taken: i64) -> RecentPhoto {
        RecentPhoto {
            phone: phone.into(),
            id: id.into(),
            name: format!("{id}.jpg"),
            taken,
            thumb: String::new(),
        }
    }

    #[test]
    fn dedupes_by_id_and_keeps_the_newest_twelve_per_phone() {
        assert_eq!(KEEP_PHOTOS, 12);
        let mut photos = Vec::new();
        // Oldest first, as the phone sends them on connect.
        for n in 1..=12 {
            assert!(remember(&mut photos, photo("a", &format!("p{n}"), n)));
        }
        assert_eq!(photos.len(), 12);
        assert_eq!(photos[0].id, "p12");
        assert_eq!(photos[11].id, "p1");

        // A repeat moves to the front and does not grow the list.
        let mut again = photo("a", "p3", 99);
        again.name = "renamed.jpg".into();
        again.thumb = "abc".into();
        assert!(remember(&mut photos, again));
        assert_eq!(photos.len(), 12);
        assert_eq!(photos[0].id, "p3");
        assert_eq!(photos[0].name, "renamed.jpg");
        assert_eq!(photos.iter().filter(|p| p.id == "p3").count(), 1);

        // The 13th distinct photo drops the oldest remaining one (p1).
        assert!(remember(&mut photos, photo("a", "p13", 100)));
        assert_eq!(photos.len(), 12);
        assert_eq!(photos[0].id, "p13");
        assert!(photos.iter().all(|p| p.id != "p1"));
        assert!(photos.iter().any(|p| p.id == "p2"));

        // Another phone has its own twelve and does not evict the first.
        for n in 1..=14 {
            assert!(remember(&mut photos, photo("b", &format!("b{n}"), n)));
        }
        let a: Vec<_> = photos
            .iter()
            .filter(|p| p.phone == "a")
            .map(|p| p.id.as_str())
            .collect();
        let b: Vec<_> = photos
            .iter()
            .filter(|p| p.phone == "b")
            .map(|p| p.id.as_str())
            .collect();
        assert_eq!(a.len(), 12);
        assert_eq!(b.len(), 12);
        assert_eq!(b[0], "b14");
        assert!(b.iter().all(|id| *id != "b1" && *id != "b2"));

        forget_phone(&mut photos, "a");
        assert!(photos.iter().all(|p| p.phone == "b"));
        assert_eq!(photos.len(), 12);
        forget_phone(&mut photos, "missing");
        assert_eq!(photos.len(), 12);
    }

    #[test]
    fn empty_ids_are_ignored_and_request_needs_a_known_photo() {
        let mut photos = Vec::new();
        assert!(!remember(&mut photos, photo("a", "", 1)));
        assert!(!remember(
            &mut photos,
            RecentPhoto {
                phone: String::new(),
                id: "x".into(),
                name: String::new(),
                taken: 0,
                thumb: String::new(),
            }
        ));
        assert!(photos.is_empty());
        assert!(remember(&mut photos, photo("a", "p1", 1)));

        assert_eq!(
            request_message(&photos, "a", "p1").unwrap(),
            Message::PhotoRequest { id: "p1".into() }
        );
        assert_eq!(
            request_message(&photos, "a", "p1").unwrap().to_line(),
            r#"{"type":"photo_request","id":"p1"}"#
        );
        assert_eq!(
            request_message(&photos, "a", "nope").unwrap_err(),
            "unknown photo"
        );
        assert_eq!(
            request_message(&photos, "b", "p1").unwrap_err(),
            "unknown photo"
        );
        assert_eq!(
            request_message(&photos, "a", "").unwrap_err(),
            "unknown photo"
        );
    }

    #[test]
    fn thumb_is_kept_only_when_it_is_a_small_jpeg() {
        assert_eq!(usable_thumb(""), "");
        assert_eq!(usable_thumb("   "), "");
        assert_eq!(usable_thumb("@@@"), "");
        let jpeg = base64::engine::general_purpose::STANDARD.encode(b"\xff\xd8\xff tiny");
        assert_eq!(usable_thumb(&jpeg), jpeg);
        assert_eq!(
            thumb_data_url(&jpeg).as_deref(),
            Some(format!("data:image/jpeg;base64,{jpeg}").as_str())
        );
        assert_eq!(thumb_data_url(""), None);
        assert_eq!(thumb_data_url("not base64!!!"), None);

        let huge = vec![7u8; MAX_THUMB_BYTES + 1];
        assert_eq!(
            usable_thumb(&base64::engine::general_purpose::STANDARD.encode(&huge)),
            ""
        );
        let max = vec![7u8; MAX_THUMB_BYTES];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&max);
        assert_eq!(usable_thumb(&encoded), encoded);
    }
}
