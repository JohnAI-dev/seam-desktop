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
| `notification` | `id` (string, unique per notification), `app` (package name), `app_name`, `title`, `text`, `time` (ms since epoch), `replyable` (bool, optional, default false: the notification has an inline reply field) |
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

## Replying to notifications

When a `notification` has `replyable: true`, the desktop may send
`{"type":"reply","id":"<notification id>","text":"<at most 5 000 characters>"}`.
The phone fills the notification's own inline reply field (Android `RemoteInput`) and
fires it, then answers `{"type":"reply_result","id":"<id>","ok":true}` or
`{"type":"reply_result","id":"<id>","ok":false,"error":"<reason>"}` (e.g. the notification
is gone or has no reply action).

## Sending files

Either side can send a file. Files are at most **2 GiB**; chunks carry at most **256 KiB**
of raw data (base64 in the frame, well under the 1 MiB line limit).

| type | fields |
|---|---|
| `file_offer` | `transfer` (random 32 hex chars), `name` (file name only, no path), `size` (bytes), `mime` (optional) |
| `file_accept` | `transfer` |
| `file_reject` | `transfer`, `reason` (optional) |
| `file_chunk` | `transfer`, `seq` (0, 1, 2, ...), `data` (standard base64) |
| `file_done` | `transfer`, `sha256` (64 hex chars of the whole file) |
| `file_result` | `transfer`, `ok` (bool), `error` (optional) |
| `file_cancel` | `transfer` — either side gives up; the receiver deletes the partial file |

1. Sender: `file_offer`. Receiver answers `file_accept` (it saves to its Downloads folder,
   by default without asking) or `file_reject`.
2. After `file_accept`, the sender sends `file_chunk`s in order, then `file_done`.
3. Receiver writes to a temporary file, checks the size and SHA-256, moves it into place
   (adding ` (1)`, ` (2)` ... if the name exists), and answers `file_result`.

Receivers must strip any path from `name` (`/`, `\`, `..`, leading dots) and must never
write outside their download folder. Wrong `seq`, more bytes than `size`, or a wrong hash
fail the transfer. Other messages (pings, notifications) may be interleaved with chunks.

## Calls

Phone → desktop: `{"type":"call","state":"ringing"|"active"|"ended","number":"<or empty>","name":"<contact name or empty>"}`.

Desktop → phone: `{"type":"call_action","action":"decline"|"silence"}` — `decline` rejects
the ringing call, `silence` stops the ringtone. Answering happens on the phone.

## Find my phone

Desktop → phone: `{"type":"ring"}` — the phone rings loudly (alarm volume, even when on
silent or Do Not Disturb) and shows a full-screen "Found it" button, until that button is
pressed, `{"type":"ring_stop"}` arrives, or 60 seconds pass.

## App icons

Phone → desktop: `{"type":"app_icon","app":"<package name>","png":"<standard base64>"}` — a
square PNG of the app's icon, at most 96×96 px and 16 KiB. The phone sends it once per app per
connection, before the first `notification` from that app. The desktop caches icons by `app`.

## Media

Phone → desktop, whenever the active media session changes (at most once per second):

`{"type":"media","app":"<package>","app_name":"…","title":"…","artist":"…","album":"…","playing":true|false,"position_ms":0,"duration_ms":0,"art":"<optional base64 JPEG, at most 64 KiB>"}`

`{"type":"media_stopped"}` — nothing is playing any more.

Desktop → phone: `{"type":"media_control","action":"play"|"pause"|"toggle"|"next"|"previous"|"volume_up"|"volume_down"}`.
Unknown actions are ignored.

## Opening links

Either side: `{"type":"open_url","url":"<http(s) URL, at most 4 096 characters>"}`. The receiver
never opens it by itself: it shows a notification ("Open link from <device>?") and opens the URL
in the default browser only when the person clicks it. Anything that isn't `http://` or
`https://` is ignored.

## Recent photos

Phone → desktop: `{"type":"photo_new","id":"<string>","name":"<file name>","taken":<ms since epoch>,"thumb":"<base64 JPEG, at most 32 KiB, longest side 256 px>"}`
— a new photo or screenshot was saved on the phone. On connect the phone sends the 12 most
recent ones, oldest first.

Desktop → phone: `{"type":"photo_request","id":"<id>"}` — the phone sends that photo with the
normal file transfer (`file_offer` …), using the photo's file name.

## Do Not Disturb

Phone → desktop: `{"type":"dnd","on":true|false}` — sent on connect and whenever the phone's
Do Not Disturb changes.

Desktop → phone: `{"type":"dnd_set","on":true|false}` — turn the phone's Do Not Disturb on or
off (the phone needs the person's permission once; without it the request is ignored).

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
