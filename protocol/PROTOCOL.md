# Seam link protocol, version 1

How the Seam phone app and the Seam desktop app talk, without USB debugging.
This file and `vectors.json` are kept identical in `seam-desktop` and `seam-android`;
both repos' tests check their code against the same vectors.

## Security model

- All traffic is **TLS 1.3**. The desktop uses a self-signed certificate; the phone
  accepts it **only** if its SHA-256 fingerprint matches the one in the pairing QR code
  (certificate pinning), so nobody on the network can impersonate the desktop.
- The phone proves it is paired with an **HMAC-SHA256 challenge** using a 32-byte key
  that only the two devices know. The key never travels over the network after pairing.
- Pairing keys from a QR code expire after **10 minutes** and work once.

## Pairing

The desktop shows a QR code containing:

```
seam://pair?v=1&host=<ip>[,<ip>...]&port=<port>&fp=<64 hex chars>&key=<base64url, 32 bytes, no padding>&name=<url-encoded desktop name>
```

All parameters are required. `v` must be `1`, `fp` must be 64 lowercase-or-uppercase hex
characters, `key` must decode to exactly 32 bytes, `port` must be 1-65535, and `host`
must contain at least one address. Anything else is rejected.

The phone's camera opens the `seam://` link in the Seam app, which stores the
host list, port, fingerprint, key and name, then connects.

## Connection

Frames are **one JSON object per line** (UTF-8, `\n`-terminated, at most 1 MiB per line).

1. Phone connects with TLS to one of the hosts on the port, pinning the fingerprint.
2. Desktop → phone: `{"type":"challenge","v":1,"nonce":"<32 hex chars>"}`
3. Phone → desktop:
   `{"type":"hello","device_id":"<stable id>","name":"<phone name>","proof":"<hex>"}`
   where `proof = hex(HMAC-SHA256(key, "seam-v1|" + nonce + "|" + device_id))`.
4. Desktop checks the proof against the key stored for `device_id`, or against the
   pending pairing key (which then becomes that device's key).
   - OK: `{"type":"welcome","desktop_name":"<name>"}`
   - Not OK: `{"type":"error","message":"<reason>"}`, then the connection closes.

## Messages after `welcome`

Phone → desktop:

| type | fields |
|---|---|
| `notification` | `id` (string, unique per notification), `app` (package name), `app_name`, `title`, `text`, `time` (ms since epoch) |
| `notification_removed` | `id` |
| `battery` | `level` (0-100), `charging` (bool) |
| `pong` | — |

Desktop → phone:

| type | fields |
|---|---|
| `ping` | — (sent every 30 s; the phone answers `pong`) |
| `dismiss` | `id` — the person dismissed this notification on the computer; the phone should cancel it |

Both directions:

| type | fields |
|---|---|
| `clipboard` | `text` (at most 100 000 characters) — the sender's clipboard changed or the person chose "send clipboard"; the receiver puts it on its clipboard |

## Finding the computer after its address changes

The desktop advertises itself on the local network with mDNS/DNS-SD:

- service type `_seam._tcp`, port = the link port
- TXT record `fp=<64 hex chars>` = its certificate fingerprint

When none of the stored hosts answer, the phone browses for `_seam._tcp`, picks the service
whose `fp` equals the paired fingerprint, connects to its address, and updates the stored
hosts. The fingerprint check (and TLS pinning) means a different computer can never be
picked by mistake.

Unknown message types must be ignored by both sides, so new features can be added
without breaking older versions.
