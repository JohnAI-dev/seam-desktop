//! Quote an application path so a space cannot split it from its arguments.
//!
//! Windows and Linux autostart entries are command lines. macOS LaunchAgents
//! store one XML string per argument, so the path is escaped for XML instead of
//! wrapped in shell quotes (launchd does not use a shell).

/// Windows `CommandLineToArgvW` quoting. The result always includes the surrounding quotes.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn quote_windows_arg(arg: &str) -> String {
    let mut out = String::from("\"");
    let mut backslashes = 0u32;
    for ch in arg.chars() {
        if ch == '\\' {
            backslashes += 1;
            continue;
        }
        if ch == '"' {
            for _ in 0..(backslashes * 2 + 1) {
                out.push('\\');
            }
            out.push('"');
            backslashes = 0;
            continue;
        }
        for _ in 0..backslashes {
            out.push('\\');
        }
        backslashes = 0;
        out.push(ch);
    }
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
    out
}

/// Value stored in `HKCU\...\Run`: the quoted executable, then each quoted argument.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn windows_command(app_path: &str, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(1 + args.len());
    parts.push(quote_windows_arg(app_path));
    for arg in args {
        parts.push(quote_windows_arg(arg));
    }
    parts.join(" ")
}

/// Quote one token for a desktop entry `Exec=` line.
///
/// The desktop-entry spec unescapes string escapes before it removes Exec quoting,
/// and only then expands field codes. A quote, backtick, or dollar is written with
/// two backslashes, and a backslash with four. A literal `%` is written as `%%`.
pub fn quote_desktop_arg(arg: &str) -> String {
    let mut out = String::from("\"");
    for ch in arg.chars() {
        match ch {
            '"' | '`' | '$' => {
                out.push('\\');
                out.push('\\');
                out.push(ch);
            }
            // Four backslashes: string-unescape yields two, quote-unescape yields one.
            '\\' => out.push_str("\\\\\\\\"),
            '%' => out.push_str("%%"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// `Exec=` value: the quoted executable, then each quoted argument.
pub fn desktop_exec(app_path: &str, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(1 + args.len());
    parts.push(quote_desktop_arg(app_path));
    for arg in args {
        parts.push(quote_desktop_arg(arg));
    }
    parts.join(" ")
}

/// Escape text placed in a plist `<string>` element.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn xml_escape(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// One `ProgramArguments` entry. The path stays a single argument, with XML metacharacters escaped.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn plist_program_argument(arg: &str) -> String {
    format!("<string>{}</string>", xml_escape(arg))
}

#[cfg(test)]
mod tests {
    use super::{
        desktop_exec, plist_program_argument, quote_desktop_arg, quote_windows_arg, windows_command,
    };

    #[test]
    fn windows_path_with_spaces_stays_one_argument() {
        let command = windows_command(
            r"C:\Program Files\Seam\seam-desktop.exe",
            &["--hidden".to_string()],
        );
        assert_eq!(
            command,
            r#""C:\Program Files\Seam\seam-desktop.exe" "--hidden""#
        );
        assert!(command.starts_with('"'));
    }

    #[test]
    fn windows_quotes_and_trailing_backslashes_are_escaped() {
        assert_eq!(quote_windows_arg(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote_windows_arg(r"C:\Seam\\"), r#""C:\Seam\\\\""#);
    }

    #[test]
    fn linux_exec_quotes_the_path_and_arguments() {
        let exec = desktop_exec("/home/ada/My Apps/seam-desktop", &["--hidden".to_string()]);
        assert_eq!(exec, r#""/home/ada/My Apps/seam-desktop" "--hidden""#);
    }

    #[test]
    fn linux_exec_escapes_quotes_and_dollars() {
        let exec = desktop_exec(r#"/tmp/we"ird$app"#, &[]);
        // `"` and `$` are double-escaped: the file contains `\\"` and `\\$`.
        assert_eq!(exec, r#""/tmp/we\\"ird\\$app""#);
    }

    #[test]
    fn linux_exec_double_escapes_backslash_backtick_and_percent() {
        let exec = desktop_exec("/tmp/a\\b`c%d", &[]);
        assert_eq!(exec, r#""/tmp/a\\\\b\\`c%%d""#);
    }

    /// Undo desktop-entry string escapes, then Exec quoting, then `%%` field codes.
    fn read_desktop_arg(exec: &str) -> String {
        let mut text = String::new();
        let mut chars = exec.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.next() {
                    Some('\\') => text.push('\\'),
                    Some('n') => text.push('\n'),
                    Some('r') => text.push('\r'),
                    Some('t') => text.push('\t'),
                    Some('s') => text.push(' '),
                    Some(other) => {
                        text.push('\\');
                        text.push(other);
                    }
                    None => text.push('\\'),
                }
            } else {
                text.push(ch);
            }
        }
        assert!(
            text.starts_with('"') && text.ends_with('"') && text.len() >= 2,
            "expected a quoted Exec token, got {text}"
        );
        let inner = &text[1..text.len() - 1];
        let mut arg = String::new();
        let mut chars = inner.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.next() {
                    Some(escaped @ ('"' | '\\' | '`' | '$')) => arg.push(escaped),
                    Some(other) => {
                        arg.push('\\');
                        arg.push(other);
                    }
                    None => arg.push('\\'),
                }
            } else {
                arg.push(ch);
            }
        }
        arg.replace("%%", "%")
    }

    #[test]
    fn linux_exec_round_trips_through_the_desktop_entry_spec() {
        for arg in [
            "/home/ada/My Apps/seam-desktop",
            "--hidden",
            r#"/tmp/we"ird$app"#,
            "/tmp/a\\b`c%d",
            "line\nbreak\tand\rpercent%%",
        ] {
            assert_eq!(read_desktop_arg(&quote_desktop_arg(arg)), arg);
        }
    }

    #[test]
    fn plist_escapes_the_path_inside_one_string() {
        let entry = plist_program_argument("/Users/ada/Apps & Stuff/Seam.app/Contents/MacOS/Seam");
        assert_eq!(
            entry,
            "<string>/Users/ada/Apps &amp; Stuff/Seam.app/Contents/MacOS/Seam</string>"
        );
        assert!(!entry.contains(" & "));
    }
}
