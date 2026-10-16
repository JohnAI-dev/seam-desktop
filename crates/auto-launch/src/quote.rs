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
/// See the Desktop Entry spec: reserved characters are escaped inside double quotes.
pub fn quote_desktop_arg(arg: &str) -> String {
    let mut out = String::from("\"");
    for ch in arg.chars() {
        match ch {
            '"' | '\\' | '`' | '$' => {
                out.push('\\');
                out.push(ch);
            }
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
    use super::{desktop_exec, plist_program_argument, quote_windows_arg, windows_command};

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
        assert_eq!(exec, r#""/tmp/we\"ird\$app""#);
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
