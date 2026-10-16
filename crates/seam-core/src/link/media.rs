//! The phone's active media session.
//!
//! See `protocol/PROTOCOL.md`, section "Media". The desktop keeps one session per
//! phone. A new `media` frame replaces it. `media_stopped`, and the phone
//! disconnecting, clear it. While `playing` is true the shown position moves
//! forward from `position_ms` on the desktop's clock.

use super::protocol::{MediaAction, Message};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Album art is at most 64 KiB of raw JPEG bytes (standard base64 on the wire).
pub const MAX_ART_BYTES: usize = 64 * 1024;

/// One `media` frame, before art is checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFrame {
    pub app: String,
    pub app_name: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
    pub position_ms: u64,
    pub duration_ms: u64,
    /// Standard base64 JPEG, when the phone sent any.
    pub art: Option<String>,
}

/// The latest session for one phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaSession {
    pub app: String,
    pub app_name: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
    /// Playback position when the phone sent the frame, in milliseconds.
    pub position_ms: u64,
    /// Track length in milliseconds. `0` means the length is unknown.
    pub duration_ms: u64,
    /// Standard base64 JPEG, or empty when there is no usable art.
    pub art: String,
    /// Unix milliseconds when this snapshot was received.
    pub updated_ms: i64,
}

impl MediaSession {
    /// Build a session, dropping art that is not a JPEG of at most [`MAX_ART_BYTES`].
    #[must_use]
    pub fn from_frame(frame: MediaFrame, updated_ms: i64) -> Self {
        Self {
            app: frame.app,
            app_name: frame.app_name,
            title: frame.title,
            artist: frame.artist,
            album: frame.album,
            playing: frame.playing,
            position_ms: frame.position_ms,
            duration_ms: frame.duration_ms,
            art: usable_art(frame.art.as_deref()),
            updated_ms,
        }
    }

    /// Position to show at `now_ms`.
    ///
    /// Paused sessions stay at `position_ms`. Playing sessions add the time since
    /// `updated_ms`, and do not go past `duration_ms` when that is greater than 0.
    #[must_use]
    pub fn shown_position_ms(&self, now_ms: i64) -> u64 {
        shown_position_ms(
            self.position_ms,
            self.duration_ms,
            self.playing,
            self.updated_ms,
            now_ms,
        )
    }
}

/// Latest session per phone, keyed by device id.
#[derive(Debug, Default, Clone)]
pub struct Sessions {
    by_phone: HashMap<String, MediaSession>,
}

impl Sessions {
    /// Replace this phone's session. An empty device id is ignored.
    pub fn remember(
        &mut self,
        phone: &str,
        frame: MediaFrame,
        updated_ms: i64,
    ) -> Option<MediaSession> {
        if phone.is_empty() {
            return None;
        }
        let session = MediaSession::from_frame(frame, updated_ms);
        self.by_phone.insert(phone.to_string(), session.clone());
        Some(session)
    }

    /// Drop this phone's session. False when it had none.
    pub fn stop(&mut self, phone: &str) -> bool {
        self.by_phone.remove(phone).is_some()
    }

    #[must_use]
    pub fn get(&self, phone: &str) -> Option<&MediaSession> {
        self.by_phone.get(phone)
    }
}

/// Unix time in milliseconds. `0` if the clock is before the epoch.
#[must_use]
pub fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// `play`, `pause`, `toggle`, `next`, `previous`, `volume_up`, or `volume_down`.
pub fn parse_action(action: &str) -> Result<MediaAction, &'static str> {
    match action.trim() {
        "play" => Ok(MediaAction::Play),
        "pause" => Ok(MediaAction::Pause),
        "toggle" => Ok(MediaAction::Toggle),
        "next" => Ok(MediaAction::Next),
        "previous" => Ok(MediaAction::Previous),
        "volume_up" => Ok(MediaAction::VolumeUp),
        "volume_down" => Ok(MediaAction::VolumeDown),
        _ => Err("unknown media action"),
    }
}

/// `media_control` for a known action.
pub fn control_message(action: &str) -> Result<Message, &'static str> {
    Ok(Message::MediaControl {
        action: parse_action(action)?,
    })
}

/// Position shown for these fields. See [`MediaSession::shown_position_ms`].
#[must_use]
pub fn shown_position_ms(
    position_ms: u64,
    duration_ms: u64,
    playing: bool,
    updated_ms: i64,
    now_ms: i64,
) -> u64 {
    let base = if duration_ms > 0 {
        position_ms.min(duration_ms)
    } else {
        position_ms
    };
    if !playing {
        return base;
    }
    // i64::saturating_sub stops at i64::MIN, not zero. A clock behind the snapshot
    // must not look like a huge elapsed time.
    let elapsed = u64::try_from(now_ms.saturating_sub(updated_ms)).unwrap_or(0);
    let pos = base.saturating_add(elapsed);
    if duration_ms > 0 {
        pos.min(duration_ms)
    } else {
        pos
    }
}

/// Keep art only when it is standard base64 of a JPEG of at most [`MAX_ART_BYTES`].
///
/// Anything else becomes empty so a huge or corrupt image is not shown. The rest
/// of the session is kept.
pub fn usable_art(art: Option<&str>) -> String {
    let Some(raw) = art.map(str::trim).filter(|art| !art.is_empty()) else {
        return String::new();
    };
    let compact: String = raw.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    match STANDARD.decode(&compact) {
        Ok(bytes)
            if !bytes.is_empty()
                && bytes.len() <= MAX_ART_BYTES
                && bytes.starts_with(&[0xFF, 0xD8]) =>
        {
            compact
        }
        _ => String::new(),
    }
}

/// `data:image/jpeg;base64,<art>` for an `<img>`, or empty when there is no art.
#[must_use]
pub fn art_data_url(art: &str) -> String {
    let art = usable_art(Some(art));
    if art.is_empty() {
        String::new()
    } else {
        format!("data:image/jpeg;base64,{art}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// Smallest JPEG marker pair: SOI + EOI.
    const TINY_JPEG: &str = "/9j/2Q==";

    fn frame(title: &str, playing: bool, position_ms: u64, art: Option<&str>) -> MediaFrame {
        MediaFrame {
            app: "com.spotify.music".into(),
            app_name: "Spotify".into(),
            title: title.into(),
            artist: "The Weeknd".into(),
            album: "After Hours".into(),
            playing,
            position_ms,
            duration_ms: 200_000,
            art: art.map(str::to_string),
        }
    }

    #[test]
    fn one_session_per_phone_and_stop_clears_only_that_phone() {
        let mut sessions = Sessions::default();
        assert!(sessions
            .remember("", frame("x", true, 0, None), 1)
            .is_none());
        assert!(sessions.get("").is_none());

        let first = sessions
            .remember("phone-a", frame("First", true, 1_000, Some(TINY_JPEG)), 10)
            .unwrap();
        assert_eq!(first.title, "First");
        assert_eq!(first.art, TINY_JPEG);
        assert_eq!(sessions.get("phone-a").unwrap().title, "First");

        let second = sessions
            .remember("phone-a", frame("Second", false, 4_000, None), 20)
            .unwrap();
        assert!(!second.playing);
        assert!(second.art.is_empty());
        assert_eq!(sessions.get("phone-a").unwrap().title, "Second");
        assert_eq!(sessions.get("phone-a").unwrap().updated_ms, 20);

        sessions
            .remember("phone-b", frame("Other", true, 0, None), 30)
            .unwrap();
        assert!(sessions.stop("phone-a"));
        assert!(!sessions.stop("phone-a"));
        assert!(sessions.get("phone-a").is_none());
        assert_eq!(sessions.get("phone-b").unwrap().title, "Other");
    }

    #[test]
    fn art_must_be_a_jpeg_within_64_kib() {
        assert_eq!(MAX_ART_BYTES, 64 * 1024);
        assert_eq!(usable_art(None), "");
        assert_eq!(usable_art(Some("")), "");
        assert_eq!(usable_art(Some("   ")), "");
        assert_eq!(usable_art(Some("not base64")), "");
        assert_eq!(usable_art(Some("YQ==")), "");
        assert_eq!(usable_art(Some(TINY_JPEG)), TINY_JPEG);
        assert_eq!(usable_art(Some(&format!("  {TINY_JPEG}\n"))), TINY_JPEG);

        let mut big = vec![0xFF, 0xD8];
        big.resize(MAX_ART_BYTES + 1, 0x00);
        let big = STANDARD.encode(big);
        assert_eq!(usable_art(Some(&big)), "");

        let mut exact = vec![0xFF, 0xD8];
        exact.resize(MAX_ART_BYTES, 0x11);
        let exact = STANDARD.encode(exact);
        assert_eq!(usable_art(Some(&exact)), exact);
        assert_eq!(
            art_data_url(TINY_JPEG),
            format!("data:image/jpeg;base64,{TINY_JPEG}")
        );
        assert_eq!(art_data_url("YQ=="), "");
        assert_eq!(art_data_url(""), "");
    }

    #[test]
    fn shown_position_advances_only_while_playing_and_stops_at_the_end() {
        assert_eq!(shown_position_ms(1_000, 5_000, false, 0, 1_500), 1_000);
        assert_eq!(shown_position_ms(1_000, 5_000, true, 0, 1_500), 2_500);
        assert_eq!(shown_position_ms(1_000, 5_000, true, 0, 9_000), 5_000);
        assert_eq!(shown_position_ms(1_000, 0, true, 0, 1_500), 2_500);
        assert_eq!(shown_position_ms(1_000, 5_000, true, 2_000, 1_500), 1_000);
        assert_eq!(shown_position_ms(9_000, 5_000, false, 0, 0), 5_000);

        let session = MediaSession::from_frame(frame("Song", true, 1_000, None), 0);
        assert_eq!(session.shown_position_ms(1_500), 2_500);
        let paused = MediaSession::from_frame(frame("Song", false, 1_000, None), 0);
        assert_eq!(paused.shown_position_ms(9_000), 1_000);
    }

    #[test]
    fn control_actions_are_the_protocol_set() {
        for (name, kind) in [
            ("play", MediaAction::Play),
            ("pause", MediaAction::Pause),
            ("toggle", MediaAction::Toggle),
            ("next", MediaAction::Next),
            ("previous", MediaAction::Previous),
            ("volume_up", MediaAction::VolumeUp),
            ("volume_down", MediaAction::VolumeDown),
        ] {
            assert_eq!(parse_action(name).unwrap(), kind);
            assert_eq!(parse_action(&format!("  {name}  ")).unwrap(), kind);
            assert_eq!(
                control_message(name).unwrap().to_line(),
                format!(r#"{{"type":"media_control","action":"{name}"}}"#)
            );
        }
        assert_eq!(parse_action("seek").unwrap_err(), "unknown media action");
        assert_eq!(parse_action("").unwrap_err(), "unknown media action");
        assert_eq!(
            control_message("toggle").unwrap(),
            Message::MediaControl {
                action: MediaAction::Toggle,
            }
        );
    }
}
