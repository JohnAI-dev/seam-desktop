#!/usr/bin/env bash
# One-time: create the key that signs Seam desktop updates, and wire it up everywhere.
# Run on prod1 (needs GIT_TOKEN with repo access):
#   curl -fsSL https://raw.githubusercontent.com/JohnAI-dev/seam-desktop/main/scripts/setup-updater-key.sh | bash
#
# - private key  -> GitHub secret TAURI_SIGNING_PRIVATE_KEY (only the release build can use it)
#                -> backup copy on the cold backup disk, if mounted
# - public key   -> committed into src-tauri/tauri.conf.json (apps use it to verify updates)
set -euo pipefail
REPO=JohnAI-dev/seam-desktop
KEY="$HOME/seam-updater.key"
BACKUP_DIR=/run/media/john/coldbackup
export GH_TOKEN="${GIT_TOKEN:?GIT_TOKEN must be set}"

command -v npx >/dev/null || sudo pacman -S --needed --noconfirm nodejs npm
command -v gh >/dev/null || sudo pacman -S --needed --noconfirm github-cli

if [ ! -f "$KEY" ]; then
  echo "== creating the update signing key"
  npx --yes @tauri-apps/cli@2 signer generate --ci -w "$KEY" >/dev/null
fi
chmod 600 "$KEY"

echo "== storing it as a GitHub secret"
gh secret set TAURI_SIGNING_PRIVATE_KEY -R "$REPO" < "$KEY"

if [ -d "$BACKUP_DIR" ]; then
  install -m 600 "$KEY" "$BACKUP_DIR/seam-updater.key"
  install -m 644 "$KEY.pub" "$BACKUP_DIR/seam-updater.key.pub"
  echo "== backed up to $BACKUP_DIR"
else
  echo "!! $BACKUP_DIR is not mounted: copy $KEY somewhere safe yourself"
fi

echo "== putting the public key into the app"
PUBKEY=$(cat "$KEY.pub") REPO="$REPO" python3 - <<'PY'
import base64, json, os, urllib.request
repo, token, pub = os.environ["REPO"], os.environ["GH_TOKEN"], os.environ["PUBKEY"].strip()
url = f"https://api.github.com/repos/{repo}/contents/src-tauri/tauri.conf.json"
hdr = {"Authorization": f"Bearer {token}", "Accept": "application/vnd.github+json"}
cur = json.load(urllib.request.urlopen(urllib.request.Request(url, headers=hdr)))
conf = json.loads(base64.b64decode(cur["content"]))
if conf["plugins"]["updater"].get("pubkey") == pub:
    print("public key already in the app")
else:
    conf["plugins"]["updater"]["pubkey"] = pub
    body = json.dumps({
        "message": "Updater: add the public key that verifies Seam updates",
        "content": base64.b64encode((json.dumps(conf, indent=2) + "\n").encode()).decode(),
        "sha": cur["sha"],
    }).encode()
    urllib.request.urlopen(urllib.request.Request(url, data=body, headers=hdr, method="PUT"))
    print("public key committed; the next release will update itself")
PY

rm -f "$KEY" "$KEY.pub"
echo "Done. (The key now lives only in GitHub and on the backup disk.)"
