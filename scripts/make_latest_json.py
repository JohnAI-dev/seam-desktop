#!/usr/bin/env python3
"""Build latest.json for Tauri's updater from the signed update files in a folder.

Usage: make_latest_json.py <dist-dir> <version> <tag> <owner/repo>
Writes <dist-dir>/latest.json. Platforms whose update file or signature is missing are
left out (their users simply don't update this time). Exits 1 if no platform is present.
"""
import datetime
import json
import sys
from pathlib import Path

# Tauri platform key -> suffix of the update file it downloads.
PLATFORMS = {
    "darwin-aarch64": ".app.tar.gz",
    "linux-x86_64": ".AppImage",
    "windows-x86_64": "-setup.exe",
}


def main() -> None:
    dist, version, tag, repo = Path(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
    files = [p for p in dist.rglob("*") if p.is_file()]
    platforms = {}
    for key, suffix in PLATFORMS.items():
        for f in files:
            sig = f.with_name(f.name + ".sig")
            if f.name.endswith(suffix) and sig.exists():
                platforms[key] = {
                    "signature": sig.read_text().strip(),
                    "url": f"https://github.com/{repo}/releases/download/{tag}/{f.name}",
                }
                break
    if not platforms:
        print("no signed update files found; not writing latest.json")
        sys.exit(1)
    manifest = {
        "version": version,
        "notes": f"Seam {version}",
        "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": platforms,
    }
    (dist / "latest.json").write_text(json.dumps(manifest, indent=2))
    print(f"latest.json for {version}: {', '.join(platforms)}")


if __name__ == "__main__":
    main()
