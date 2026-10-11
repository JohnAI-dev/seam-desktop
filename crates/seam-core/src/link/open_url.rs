//! `open_url` validation. See `protocol/PROTOCOL.md`, section "Opening links".
//!
//! Only `http://` and `https://` URLs of at most [`MAX_URL_CHARS`] characters are
//! accepted. Anything else is ignored by the receiver. Opening is a separate step
//! the person confirms; this module never launches a browser.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use url::Url;

/// Longest accepted `open_url`, in Unicode scalar values (not bytes).
pub const MAX_URL_CHARS: usize = 4096;

/// A URL that may be sent or shown. `url` is the trimmed original; `host` is the
/// parsed host name (no userinfo, no port) for the confirmation notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidUrl {
    pub url: String,
    pub host: String,
}

/// A link a phone asked this computer to open. Stored until the person clicks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncomingLink {
    pub id: String,
    pub phone: String,
    pub phone_name: String,
    /// Trimmed URL that passed [`validate_open_url`].
    pub url: String,
    /// Host name shown in the notification and the Links list.
    pub host: String,
}

type IncomingListener = Arc<dyn Fn(IncomingLink) + Send + Sync>;

static LISTENER: Mutex<Option<IncomingListener>> = Mutex::new(None);

/// Title of the system notification. The body is the host name, not the full URL.
pub fn notification_title(phone_name: &str) -> String {
    let name = phone_name.trim();
    let name = if name.is_empty() { "phone" } else { name };
    format!("Open link from {name}?")
}

/// Accept only `http://` or `https://` with a host, at most [`MAX_URL_CHARS`] characters.
///
/// Rejects `javascript:`, `file:`, `data:`, other schemes, a missing host, and
/// anything longer than the limit. The check is on characters after trimming, not bytes.
pub fn validate_open_url(raw: &str) -> Result<ValidUrl, &'static str> {
    let url = raw.trim();
    if url.is_empty() {
        return Err("not an http(s) url");
    }
    if url.chars().count() > MAX_URL_CHARS {
        return Err("url is too long");
    }
    if !is_http_scheme(url) {
        return Err("not an http(s) url");
    }
    // WHATWG skips extra slashes, so `http:///etc/passwd` would become host `etc`.
    // An empty authority in the text the sender wrote is not a host.
    if !has_written_authority(url) {
        return Err("missing host");
    }
    let parsed = match Url::parse(url) {
        Ok(parsed) => parsed,
        Err(_) => return Err("missing host"),
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err("not an http(s) url");
    }
    match parsed.host_str() {
        Some(host) if !host.is_empty() => Ok(ValidUrl {
            url: url.to_string(),
            host: display_host(host),
        }),
        _ => Err("missing host"),
    }
}

/// Parser-normalized http(s) URL safe to hand to the system shell. Fails closed.
pub fn url_to_open(raw: &str) -> Result<String, &'static str> {
    let valid = validate_open_url(raw)?;
    let parsed = Url::parse(&valid.url).map_err(|_| "not an http(s) url")?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err("not an http(s) url");
    }
    if parsed.host_str().filter(|host| !host.is_empty()).is_none() {
        return Err("missing host");
    }
    let open_as = parsed.to_string();
    if !is_http_scheme(&open_as) {
        return Err("not an http(s) url");
    }
    Ok(open_as)
}

/// Called when a phone's `open_url` has been stored. The desktop shows a notification.
/// Replaces any previous listener. Tests leave this unset.
pub fn on_incoming_link(listener: impl Fn(IncomingLink) + Send + Sync + 'static) {
    *LISTENER.lock().unwrap() = Some(Arc::new(listener));
}

/// Notify the desktop off the connection task so a slow notification cannot stall it.
pub(crate) fn emit_incoming(link: IncomingLink) {
    let listener = LISTENER.lock().unwrap().clone();
    if let Some(listener) = listener {
        std::thread::spawn(move || listener(link));
    }
}

/// True when `http://` or `https://` is followed by a non-empty authority.
fn has_written_authority(url: &str) -> bool {
    let bytes = url.as_bytes();
    let rest = if bytes.len() >= 8 && bytes[..8].eq_ignore_ascii_case(b"https://") {
        &url[8..]
    } else if bytes.len() >= 7 && bytes[..7].eq_ignore_ascii_case(b"http://") {
        &url[7..]
    } else {
        return false;
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    !rest[..end].is_empty()
}

/// Host for the confirmation text: no userinfo, no port, no IPv6 brackets.
fn display_host(host: &str) -> String {
    match host.strip_prefix('[') {
        Some(inner) => inner.strip_suffix(']').unwrap_or(inner).to_string(),
        None => host.to_string(),
    }
}

fn is_http_scheme(url: &str) -> bool {
    let bytes = url.as_bytes();
    let https = bytes.len() >= 8 && bytes[..8].eq_ignore_ascii_case(b"https://");
    let http = bytes.len() >= 7 && bytes[..7].eq_ignore_ascii_case(b"http://");
    https || http
}

#[cfg(test)]
mod tests {
    use super::*;

    fn https_len(chars: usize) -> String {
        let prefix = "https://example.com/";
        assert!(chars >= prefix.chars().count());
        format!("{prefix}{}", "a".repeat(chars - prefix.chars().count()))
    }

    #[test]
    fn rejects_javascript_file_data_too_long_and_missing_host() {
        assert_eq!(MAX_URL_CHARS, 4096);
        assert_eq!(
            validate_open_url("javascript:alert(1)").unwrap_err(),
            "not an http(s) url"
        );
        assert_eq!(
            validate_open_url("JAVASCRIPT:alert(document.cookie)").unwrap_err(),
            "not an http(s) url"
        );
        assert_eq!(
            validate_open_url("  javascript:alert(1)").unwrap_err(),
            "not an http(s) url"
        );
        assert_eq!(
            validate_open_url("file:///etc/passwd").unwrap_err(),
            "not an http(s) url"
        );
        assert_eq!(
            validate_open_url("file://localhost/etc/passwd").unwrap_err(),
            "not an http(s) url"
        );
        assert_eq!(
            validate_open_url("data:text/html,hi").unwrap_err(),
            "not an http(s) url"
        );
        assert_eq!(
            validate_open_url("data:text/html,<script>alert(1)</script>").unwrap_err(),
            "not an http(s) url"
        );
        assert_eq!(validate_open_url("http://").unwrap_err(), "missing host");
        assert_eq!(validate_open_url("https://").unwrap_err(), "missing host");
        assert_eq!(validate_open_url("http:///").unwrap_err(), "missing host");
        assert_eq!(
            validate_open_url("http:///etc/passwd").unwrap_err(),
            "missing host"
        );
        assert_eq!(
            validate_open_url("https:///no-host").unwrap_err(),
            "missing host"
        );
        assert_eq!(validate_open_url("").unwrap_err(), "not an http(s) url");
        assert_eq!(validate_open_url("   ").unwrap_err(), "not an http(s) url");
        assert_eq!(
            validate_open_url("example.com").unwrap_err(),
            "not an http(s) url"
        );
        assert_eq!(
            validate_open_url("ftp://example.com").unwrap_err(),
            "not an http(s) url"
        );
        assert_eq!(
            validate_open_url("seam://pair?v=1").unwrap_err(),
            "not an http(s) url"
        );

        let too_long = https_len(4097);
        assert_eq!(too_long.chars().count(), 4097);
        assert_eq!(validate_open_url(&too_long).unwrap_err(), "url is too long");
        assert_eq!(url_to_open(&too_long).unwrap_err(), "url is too long");
        assert!(url_to_open("javascript:alert(1)").is_err());
        assert!(url_to_open("file:///etc/passwd").is_err());
        assert!(url_to_open("data:text/html,hi").is_err());
        assert!(url_to_open("http://").is_err());
    }

    #[test]
    fn accepts_http_and_https_and_extracts_the_host() {
        let exact = https_len(4096);
        assert_eq!(exact.chars().count(), 4096);
        let ok = validate_open_url(&exact).unwrap();
        assert_eq!(ok.host, "example.com");
        assert_eq!(ok.url, exact);
        assert!(url_to_open(&exact)
            .unwrap()
            .starts_with("https://example.com/"));

        let trimmed = validate_open_url("  https://example.com/a?b=1  ").unwrap();
        assert_eq!(trimmed.url, "https://example.com/a?b=1");
        assert_eq!(trimmed.host, "example.com");
        assert_eq!(
            url_to_open("https://example.com/a?b=1").unwrap(),
            "https://example.com/a?b=1"
        );

        assert_eq!(
            validate_open_url("http://example.com:8443/x").unwrap().host,
            "example.com"
        );
        assert_eq!(
            validate_open_url("https://192.168.1.20/x").unwrap().host,
            "192.168.1.20"
        );
        assert_eq!(
            validate_open_url("http://[2001:db8::1]/").unwrap().host,
            "2001:db8::1"
        );
        // The notification shows the host, never the password.
        assert_eq!(
            validate_open_url("https://user:secret@example.com/private")
                .unwrap()
                .host,
            "example.com"
        );
        assert!(validate_open_url("HTTPS://Example.COM/Path")
            .unwrap()
            .host
            .eq_ignore_ascii_case("example.com"));

        // The limit is characters, not bytes: '你' is one character and three bytes.
        let han = format!("https://example.com/{}", "你".repeat(2000));
        assert!(han.len() > MAX_URL_CHARS);
        assert!(han.chars().count() <= MAX_URL_CHARS);
        assert_eq!(validate_open_url(&han).unwrap().host, "example.com");
    }

    #[test]
    fn notification_asks_which_phone_and_the_body_is_the_host() {
        assert_eq!(notification_title("Pixel"), "Open link from Pixel?");
        assert_eq!(notification_title("  Pixel  "), "Open link from Pixel?");
        assert_eq!(notification_title(""), "Open link from phone?");
        assert_eq!(notification_title("   "), "Open link from phone?");
        let link = IncomingLink {
            id: "1".into(),
            phone: "phone-1".into(),
            phone_name: "Pixel".into(),
            url: "https://example.com/a".into(),
            host: "example.com".into(),
        };
        assert_eq!(
            notification_title(&link.phone_name),
            "Open link from Pixel?"
        );
        assert_eq!(link.host, "example.com");
    }
}
