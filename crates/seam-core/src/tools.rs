//! Locating the external programs Seam drives (adb and scrcpy).

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Folder holding the copies of adb and scrcpy that ship inside the app, once set.
static BUNDLED_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Tell Seam where the tools shipped inside the app live. Called once at startup.
pub fn set_bundled_dir(dir: PathBuf) {
    let _ = BUNDLED_DIR.set(dir);
}

/// The folder of tools shipped inside the app, if any.
pub fn bundled_dir() -> Option<&'static Path> {
    BUNDLED_DIR.get().map(PathBuf::as_path)
}

/// Whether `path` is one of the tools shipped inside the app.
pub fn is_bundled(path: &Path) -> bool {
    bundled_dir().is_some_and(|d| path.starts_with(d))
}

/// An external program Seam needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Adb,
    Scrcpy,
}

impl Tool {
    /// Executable name, without platform suffix.
    pub fn name(self) -> &'static str {
        match self {
            Tool::Adb => "adb",
            Tool::Scrcpy => "scrcpy",
        }
    }

    /// Environment variable that overrides where the tool is found.
    pub fn env_override(self) -> &'static str {
        match self {
            Tool::Adb => "SEAM_ADB",
            Tool::Scrcpy => "SEAM_SCRCPY",
        }
    }

    /// Arguments that make the tool print its version.
    fn version_args(self) -> &'static [&'static str] {
        match self {
            Tool::Adb => &["version"],
            Tool::Scrcpy => &["--version"],
        }
    }
}

fn executable_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Directories searched after `PATH`: common install locations that are often missing
/// from the `PATH` of GUI apps (Homebrew on macOS, the Android SDK, /usr/local).
fn extra_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/opt/homebrew/bin"),
    ];
    if let Some(home) = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE")) {
        let home = PathBuf::from(home);
        dirs.push(home.join("Android/Sdk/platform-tools"));
        dirs.push(home.join("Library/Android/sdk/platform-tools"));
        dirs.push(home.join(".local/bin"));
    }
    dirs
}

/// Find `tool`: the override variable wins, then the copy shipped inside the app,
/// then `PATH`, then common install locations.
pub fn find(tool: Tool) -> Option<PathBuf> {
    if let Some(p) = env::var_os(tool.env_override()) {
        let p = PathBuf::from(p);
        return is_executable(&p).then_some(p);
    }
    let search_path = env::var_os("PATH").unwrap_or_default();
    find_in(
        &executable_name(tool.name()),
        bundled_dir()
            .map(Path::to_path_buf)
            .into_iter()
            .chain(env::split_paths(&search_path))
            .chain(extra_dirs()),
    )
}

/// Find an executable called `file_name` in the first of `dirs` that has it.
pub fn find_in(file_name: &str, dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    dirs.into_iter()
        .map(|d| d.join(file_name))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// First line of the tool's version output, if it runs.
pub fn version(tool: Tool, path: &Path) -> Option<String> {
    let out = Command::new(path).args(tool.version_args()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Name of the file recording which tool version a folder holds.
pub const VERSION_FILE: &str = "VERSION";

/// Make the tools shipped inside the app runnable and return the folder to use.
///
/// On Windows they run straight from the app. On Mac and Linux they are copied to
/// `cache` (once per tools version) and marked executable, because the app's own
/// folder can be read-only (AppImage) or quarantined by macOS Gatekeeper.
pub fn prepare_bundled(src: &Path, cache: &Path) -> io::Result<PathBuf> {
    if cfg!(windows) {
        return Ok(src.to_path_buf());
    }
    let wanted = fs::read_to_string(src.join(VERSION_FILE))?;
    let have = fs::read_to_string(cache.join(VERSION_FILE)).unwrap_or_default();
    if have != wanted || !is_executable(&cache.join("scrcpy")) {
        let _ = fs::remove_dir_all(cache);
        fs::create_dir_all(cache)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                fs::copy(entry.path(), cache.join(entry.file_name()))?;
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in ["adb", "scrcpy"] {
                let p = cache.join(name);
                if p.exists() {
                    fs::set_permissions(&p, fs::Permissions::from_mode(0o755))?;
                }
            }
        }
        if cfg!(target_os = "macos") {
            // Downloaded apps carry a quarantine flag that would block the tools.
            let _ = Command::new("xattr")
                .args(["-dr", "com.apple.quarantine"])
                .arg(cache)
                .status();
        }
    }
    Ok(cache.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let d = env::temp_dir().join(format!("seam-tools-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    fn make_executable(p: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(p, "#!/bin/sh\necho 'Android Debug Bridge version 1.0.41'\n").unwrap();
        fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn finds_first_matching_dir_and_reads_version() {
        let empty = temp_dir("empty");
        let full = temp_dir("full");
        let adb = full.join("adb");
        make_executable(&adb);
        let found = find_in("adb", vec![empty, full]).expect("adb should be found");
        assert_eq!(found, adb);
        assert_eq!(
            version(Tool::Adb, &found).as_deref(),
            Some("Android Debug Bridge version 1.0.41")
        );
    }

    #[cfg(unix)]
    #[test]
    fn ignores_non_executable_files() {
        let d = temp_dir("noexec");
        fs::write(d.join("adb"), "not a program").unwrap();
        assert_eq!(find_in("adb", vec![d]), None);
    }

    #[cfg(unix)]
    #[test]
    fn prepare_bundled_copies_and_makes_tools_executable() {
        let src = temp_dir("bundle-src");
        let cache = temp_dir("bundle-cache").join("tools");
        fs::write(src.join("adb"), "#!/bin/sh\n").unwrap();
        fs::write(src.join("scrcpy"), "#!/bin/sh\n").unwrap();
        fs::write(src.join("scrcpy-server"), "server").unwrap();
        fs::write(src.join(VERSION_FILE), "5.0.1").unwrap();

        let dir = prepare_bundled(&src, &cache).unwrap();
        assert_eq!(dir, cache);
        assert!(is_executable(&dir.join("adb")));
        assert!(is_executable(&dir.join("scrcpy")));
        assert!(dir.join("scrcpy-server").exists());

        // A new tools version replaces the old copy.
        fs::write(src.join(VERSION_FILE), "5.1.0").unwrap();
        prepare_bundled(&src, &cache).unwrap();
        assert_eq!(
            fs::read_to_string(cache.join(VERSION_FILE)).unwrap(),
            "5.1.0"
        );
    }

    #[test]
    fn missing_tool_is_none() {
        assert_eq!(
            find_in("definitely-not-a-real-tool", vec![temp_dir("none")]),
            None
        );
    }
}
