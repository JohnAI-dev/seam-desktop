#!/usr/bin/env python3
"""Download scrcpy (which includes adb) for one platform into src-tauri/resources/tools/,
so they ship inside Seam and users don't install anything else.

Usage: python3 scripts/fetch_tools.py linux-x86_64 | macos-aarch64 | macos-x86_64 | win64

Version and SHA-256 checksums are pinned below; a download that doesn't match is rejected.
To upgrade: change VERSION and copy the new hashes from the release's SHA256SUMS.txt.
"""
import hashlib
import io
import shutil
import sys
import tarfile
import urllib.request
import zipfile
from pathlib import Path

VERSION = "5.0.1"
SHA256 = {
    "linux-x86_64": "9f969d30cc574816077edecda65719c1c70b0aee1df1ba5fa00779ac07b8fd6e",
    "macos-aarch64": "33611e51977a8289e2e124b220f727895181ef2eb9060e0987a4dc2db9cf0d6b",
    "macos-x86_64": "31a5467a9e907f162b093267cbbab1276b15c6e709c2ea8159566a8d165beb50",
    "win64": "b12a2c4ee8be317422451fc7dcf8ee20a71b5ea7ef9ad73ddd825a25316227a5",
}
# Files not needed at runtime.
SKIP = {"scrcpy.1", "open_a_terminal_here.bat", "scrcpy-noconsole.vbs"}

OUT = Path(__file__).resolve().parent.parent / "src-tauri" / "resources" / "tools"


def main() -> None:
    if len(sys.argv) != 2 or sys.argv[1] not in SHA256:
        sys.exit(f"usage: fetch_tools.py {{{'|'.join(SHA256)}}}")
    platform = sys.argv[1]
    stamp = f"{VERSION} {platform}"
    if (OUT / "VERSION").exists() and (OUT / "VERSION").read_text() == stamp:
        print(f"tools already present: scrcpy {stamp}")
        return

    ext = "zip" if platform.startswith("win") else "tar.gz"
    name = f"scrcpy-{platform}-v{VERSION}.{ext}"
    url = f"https://github.com/Genymobile/scrcpy/releases/download/v{VERSION}/{name}"
    print(f"downloading {url}")
    with urllib.request.urlopen(url, timeout=300) as r:
        data = r.read()
    digest = hashlib.sha256(data).hexdigest()
    if digest != SHA256[platform]:
        sys.exit(f"checksum mismatch for {name}: got {digest}, expected {SHA256[platform]}")

    shutil.rmtree(OUT, ignore_errors=True)
    OUT.mkdir(parents=True)
    if ext == "zip":
        with zipfile.ZipFile(io.BytesIO(data)) as z:
            members = [(Path(i.filename).name, z.read(i)) for i in z.infolist() if not i.is_dir()]
    else:
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as t:
            members = [(Path(m.name).name, t.extractfile(m).read()) for m in t.getmembers() if m.isfile()]
    for fname, content in members:
        if fname in SKIP:
            continue
        target = OUT / fname
        target.write_bytes(content)
        if fname in ("adb", "scrcpy"):
            target.chmod(0o755)
    (OUT / "VERSION").write_text(stamp)
    # Keep the placeholder so the folder exists in git checkouts.
    (OUT / ".gitkeep").touch()
    print(f"tools ready in {OUT}: " + ", ".join(sorted(p.name for p in OUT.iterdir())))


if __name__ == "__main__":
    main()
