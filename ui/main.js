// Seam main window. Talks to the Rust side through Tauri commands.
const { invoke } = window.__TAURI__.core;

const STATE_TEXT = {
  device: "Connected",
  unauthorized: "Accept the prompt on your phone",
  offline: "Offline - reconnect the cable",
};

const INSTALL_HINT = {
  adb: "Install Android platform tools (Arch: android-tools, Debian/Ubuntu: adb, Mac: brew install android-platform-tools)",
  scrcpy: "Install scrcpy (Arch/Debian/Ubuntu: scrcpy, Mac: brew install scrcpy)",
};

function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  Object.assign(node, props);
  for (const c of children) node.append(c);
  return node;
}

function renderDevices(status) {
  const list = document.getElementById("devices");
  list.replaceChildren(
    ...status.devices.map((d) => {
      const name = d.model || d.serial;
      let meta = `${STATE_TEXT[d.state] || d.state} · ${d.wireless ? "Wi-Fi" : "USB"}`;
      if (d.battery) {
        meta += ` · ${d.battery.level}%`;
        if (d.battery.charging) meta += " ⚡";
      }
      const btn = el("button", { textContent: "Mirror" });
      btn.disabled = d.state !== "device" || !status.scrcpy.found;
      btn.addEventListener("click", async () => {
        btn.disabled = true;
        try {
          await invoke("start_mirror", { serial: d.serial, name });
          showError(null);
        } catch (e) {
          showError(String(e));
        } finally {
          setTimeout(() => (btn.disabled = false), 1500);
        }
      });
      return el(
        "li",
        { className: "device" },
        el("div", { className: "info" }, el("div", { className: "name", textContent: name }), el("div", { className: "meta", textContent: meta })),
        btn,
      );
    }),
  );
  document.getElementById("empty").hidden = status.devices.length > 0 || !status.adb.found;
}

function renderTools(status) {
  const rows = ["adb", "scrcpy"].map((key) => {
    const t = status[key];
    const right = t.found
      ? el("span", { className: "ok", textContent: `${t.version || "found"}${t.bundled ? " · built in" : ""}` })
      : el("span", { className: "missing", textContent: "missing", title: INSTALL_HINT[key] });
    return el("li", {}, el("span", { textContent: key }), right);
  });
  document.getElementById("tools").replaceChildren(...rows);
}

function showError(msg) {
  const e = document.getElementById("error");
  e.textContent = msg || "";
  e.hidden = !msg;
}

// How long "Sent" stays up. The timer re-renders when it fires so Reply comes back
// without waiting for an unrelated status poll.
const REPLY_SENT_MS = 3000;
const MAX_REPLY_CHARS = 5000;

let lastLink = null;
const composers = new Map();

function noteKey(phone, id) {
  return JSON.stringify([phone, id]);
}

function noteReplyState(phone, id) {
  const key = noteKey(phone, id);
  let state = composers.get(key);
  if (!state) {
    state = {
      open: false,
      draft: "",
      sending: false,
      error: "",
      sentUntil: 0,
      appliedSeq: 0,
      sentTimer: null,
    };
    composers.set(key, state);
  }
  return state;
}

function findReplyInput(key) {
  for (const input of document.querySelectorAll("input.reply-text")) {
    if (input.dataset.replyFor === key) return input;
  }
  return null;
}

function captureReplyFocus() {
  const active = document.activeElement;
  if (!active || !active.classList || !active.classList.contains("reply-text")) return null;
  return {
    key: active.dataset.replyFor,
    start: active.selectionStart,
    end: active.selectionEnd,
  };
}

function restoreReplyFocus(saved) {
  if (!saved) return;
  const input = findReplyInput(saved.key);
  if (!input) return;
  input.focus();
  if (typeof saved.start === "number") {
    try {
      input.setSelectionRange(saved.start, saved.end);
    } catch (e) {
      /* the input may not accept a selection */
    }
  }
}

function scheduleSentClear(key) {
  const state = composers.get(key);
  if (!state) return;
  clearTimeout(state.sentTimer);
  state.sentTimer = setTimeout(() => {
    const cur = composers.get(key);
    if (!cur) return;
    cur.sentUntil = 0;
    cur.sentTimer = null;
    // Re-render so "Sent" clears and the Reply button comes back on its own.
    if (lastLink) renderLink(lastLink);
    else refreshLink().catch(() => {});
  }, REPLY_SENT_MS);
}

function applyReplyOutcome(phone, id, outcome) {
  if (typeof phone !== "string" || typeof id !== "string") return;
  const state = noteReplyState(phone, id);
  if (outcome.seq && state.appliedSeq === outcome.seq) return;
  if (outcome.seq) state.appliedSeq = outcome.seq;
  state.sending = false;
  if (outcome.ok) {
    state.open = false;
    state.draft = "";
    state.error = "";
    state.sentUntil = Date.now() + REPLY_SENT_MS;
    scheduleSentClear(noteKey(phone, id));
  } else {
    state.sentUntil = 0;
    clearTimeout(state.sentTimer);
    state.sentTimer = null;
    state.error = outcome.error || "could not send the reply";
    state.open = true;
  }
}

function absorbStatusReply(n) {
  const seq = n.reply_seq || 0;
  const state = composers.get(noteKey(n.phone, n.id));
  // A pushed event may already be newer than this status snapshot.
  if (state && seq < state.appliedSeq) return;
  if (!seq) return;
  applyReplyOutcome(n.phone, n.id, {
    ok: n.reply_ok === true,
    error: n.reply_error || "",
    seq,
  });
}

function onPushedReplyResult(payload) {
  applyReplyOutcome(payload.phone, payload.id, {
    ok: payload.ok === true,
    error: payload.error || "",
    seq: payload.seq || 0,
  });
  if (lastLink) renderLink(lastLink);
  else refreshLink().catch((e) => showLinkError(String(e)));
}

function listenForReplyResults() {
  try {
    const listen = window.__TAURI__ && window.__TAURI__.event && window.__TAURI__.event.listen;
    if (typeof listen !== "function") return;
    listen("reply-result", (event) => {
      const payload = event && event.payload;
      if (!payload) return;
      onPushedReplyResult(payload);
    }).catch(() => {});
  } catch (e) {
    // Self-test still has to render if the event API is missing.
  }
}

function listenForIncomingCalls() {
  try {
    const listen = window.__TAURI__ && window.__TAURI__.event && window.__TAURI__.event.listen;
    if (typeof listen !== "function") return;
    listen("incoming-call", () => {
      refreshLink().catch((e) => showLinkError(String(e)));
    }).catch(() => {});
  } catch (e) {
    // Polling still updates the banner.
  }
}

function phoneLabel(link, id) {
  const phones = link.phones || [];
  for (let i = 0; i < phones.length; i++) {
    if (phones[i].id === id && phones[i].name) return phones[i].name;
  }
  return id || "";
}

function sendStatusText(send) {
  if (send.state === "done") return "Sent";
  if (send.state === "failed") return send.error || "could not send the file";
  const size = Number(send.size) || 0;
  const sent = Number(send.sent) || 0;
  if (size <= 0) return "Sending\u2026";
  const pct = Math.min(100, Math.round((sent / size) * 100));
  return "Sending " + pct + "%";
}

function fileSendRow(link, send) {
  const who = phoneLabel(link, send.phone);
  let meta = sendStatusText(send);
  if (who) meta += " \u00b7 " + who;
  const info = el(
    "div",
    { className: "info" },
    el("div", { className: "name", textContent: send.name || "file" }),
    el("div", { className: send.state === "failed" ? "meta fail" : "meta", textContent: meta }),
  );
  const row = el("li", { className: "file-row" }, info);
  if (send.state === "sending" && send.transfer) {
    const cancel = el("button", { type: "button", className: "secondary", textContent: "Cancel" });
    cancel.addEventListener("click", async () => {
      cancel.disabled = true;
      try {
        await invoke("cancel_file_send", { transfer: send.transfer });
        showLinkError(null);
        await refreshLink();
      } catch (e) {
        const msg = String(e);
        if (msg.indexOf("not running") === -1) showLinkError(msg);
        await refreshLink().catch(() => {});
        cancel.disabled = false;
      }
    });
    row.append(cancel);
  }
  return row;
}

function receivedRow(file) {
  const show = el("button", {
    type: "button",
    className: "secondary",
    textContent: "Show in folder",
  });
  show.addEventListener("click", async () => {
    show.disabled = true;
    try {
      await invoke("reveal_file", { path: file.path });
      showLinkError(null);
    } catch (e) {
      showLinkError(String(e));
    } finally {
      show.disabled = false;
    }
  });
  const from = file.phone_name ? "from " + file.phone_name : "";
  return el(
    "li",
    { className: "file-row" },
    el(
      "div",
      { className: "info" },
      el("div", { className: "name", textContent: file.name || "file" }),
      from ? el("div", { className: "meta", textContent: from }) : "",
    ),
    show,
  );
}

function renderFiles(link) {
  const sendsHead = document.getElementById("sends-h");
  const sends = document.getElementById("sends");
  const received = document.getElementById("received");
  const empty = document.getElementById("received-empty");
  const outgoing = link.sends || [];
  if (sendsHead) sendsHead.hidden = outgoing.length === 0;
  if (sends) sends.replaceChildren(...outgoing.map((send) => fileSendRow(link, send)));
  const files = link.received || [];
  if (received) received.replaceChildren(...files.map((file) => receivedRow(file)));
  if (empty) empty.hidden = files.length > 0;
}

let dropAskPaths = null;

function hideDropAsk() {
  dropAskPaths = null;
  const box = document.getElementById("drop-ask");
  if (box) box.hidden = true;
}

async function sendPathsTo(phoneId, phoneName, paths) {
  let started = 0;
  let firstError = "";
  for (let i = 0; i < paths.length; i++) {
    try {
      await invoke("send_file", { phone: phoneId, path: paths[i] });
      started += 1;
    } catch (e) {
      if (!firstError) firstError = String(e);
    }
  }
  await refreshLink().catch((e) => {
    if (!firstError) firstError = String(e);
  });
  if (started) showLinkConfirm("Sending to " + (phoneName || "phone") + ".");
  else showLinkConfirm(null);
  if (firstError) showLinkError(firstError);
  else if (started) showLinkError(null);
}

function showDropAsk(phones, paths) {
  dropAskPaths = paths;
  const box = document.getElementById("drop-ask");
  const list = document.getElementById("drop-ask-phones");
  const text = document.getElementById("drop-ask-text");
  if (!box || !list) {
    showLinkError("no phone is connected");
    return;
  }
  if (text) {
    text.textContent = paths.length === 1
      ? "Send this file to which phone?"
      : "Send these files to which phone?";
  }
  list.replaceChildren(
    ...phones.map((p) => {
      const btn = el("button", {
        type: "button",
        className: "secondary",
        textContent: p.name || p.id,
      });
      btn.addEventListener("click", async () => {
        const chosen = dropAskPaths || paths;
        hideDropAsk();
        await sendPathsTo(p.id, p.name || p.id, chosen);
      });
      return btn;
    }),
  );
  box.hidden = false;
}

async function chooseAndSendFile(phoneId, phoneName) {
  let path;
  try {
    path = await invoke("pick_file");
  } catch (e) {
    showLinkError(String(e));
    return;
  }
  if (!path) return;
  await sendPathsTo(phoneId, phoneName, [path]);
}

async function onFilesDropped(payload) {
  const paths = Array.isArray(payload) ? payload.filter((p) => typeof p === "string" && p) : [];
  if (!paths.length) return;
  try {
    const plan = await invoke("handle_file_drop", { paths });
    if (!plan || plan.action === "error") {
      await refreshLink().catch(() => {});
      showLinkError((plan && plan.message) || "could not send the file");
      return;
    }
    if (plan.action === "ask") {
      showDropAsk(plan.phones || [], plan.paths || paths);
      return;
    }
    await refreshLink().catch(() => {});
    if (plan.action === "sent") {
      showLinkError(null);
      showLinkConfirm("Sending to " + (plan.phone || "phone") + ".");
    }
  } catch (e) {
    await refreshLink().catch(() => {});
    showLinkError(String(e));
  }
}

function listenForFileTransfers() {
  try {
    const listen = window.__TAURI__ && window.__TAURI__.event && window.__TAURI__.event.listen;
    if (typeof listen !== "function") return;
    listen("file-transfer", () => {
      refreshLink().catch((e) => showLinkError(String(e)));
    }).catch(() => {});
    listen("files-dropped", (event) => {
      onFilesDropped(event && event.payload);
    }).catch(() => {});
  } catch (e) {
    // Self-test still has to render if the event API is missing.
  }
}

function openReply(phone, id) {
  const state = noteReplyState(phone, id);
  clearTimeout(state.sentTimer);
  state.sentTimer = null;
  state.sentUntil = 0;
  state.open = true;
  state.error = "";
  if (lastLink) renderLink(lastLink);
  const input = findReplyInput(noteKey(phone, id));
  if (input) input.focus();
}

function cancelReply(phone, id) {
  const state = composers.get(noteKey(phone, id));
  if (!state) return;
  state.open = false;
  state.sending = false;
  state.error = "";
  if (lastLink) renderLink(lastLink);
}

async function submitReply(phone, id) {
  const key = noteKey(phone, id);
  const state = noteReplyState(phone, id);
  if (state.sending) return;
  const input = findReplyInput(key);
  if (input) state.draft = input.value;
  const text = state.draft.trim();
  if (!text) {
    state.error = "reply is empty";
    state.open = true;
    if (lastLink) renderLink(lastLink);
    return;
  }
  if ([...text].length > MAX_REPLY_CHARS) {
    state.error = "reply is too long";
    state.open = true;
    if (lastLink) renderLink(lastLink);
    return;
  }
  state.draft = text;
  state.sending = true;
  state.error = "";
  if (lastLink) renderLink(lastLink);
  try {
    await invoke("reply_notification", { phone, id, text });
    // reply_result is pushed on "reply-result" and stored on link_status.
    // Do not give up after a few seconds of polling: a late result still applies.
  } catch (e) {
    state.sending = false;
    state.error = String(e);
    state.open = true;
    if (lastLink) renderLink(lastLink);
    const again = findReplyInput(key);
    if (again) again.focus();
  }
}

function replyControls(n) {
  const key = noteKey(n.phone, n.id);
  const state = composers.get(key);
  if (state && state.sentUntil > Date.now()) {
    return el("div", { className: "reply-sent", textContent: "Sent" });
  }
  if (state && state.sentUntil) state.sentUntil = 0;
  const onKey = (e) => {
    if (e.key === "Enter" && e.target.classList && e.target.classList.contains("reply-text")) {
      e.preventDefault();
      submitReply(n.phone, n.id);
    } else if (e.key === "Escape") {
      e.preventDefault();
      cancelReply(n.phone, n.id);
    }
  };
  if (state && state.open) {
    const input = el("input", {
      type: "text",
      className: "reply-text",
      value: state.draft,
      placeholder: "Reply",
    });
    input.readOnly = !!state.sending;
    input.autocomplete = "off";
    input.dataset.replyFor = key;
    input.setAttribute("aria-label", "Reply");
    input.addEventListener("input", () => {
      const cur = composers.get(key);
      if (cur) cur.draft = input.value;
    });
    const send = el("button", {
      type: "button",
      className: "send",
      textContent: state.sending ? "Sending\u2026" : "Send",
      disabled: !!state.sending,
    });
    send.addEventListener("click", () => submitReply(n.phone, n.id));
    const row = el("div", { className: "reply" }, input, send);
    row.addEventListener("keydown", onKey);
    if (state.error) {
      return el(
        "div",
        { className: "reply-wrap" },
        row,
        el("div", { className: "reply-error", textContent: state.error }),
      );
    }
    return row;
  }
  const btn = el("button", { type: "button", className: "reply-btn", textContent: "Reply" });
  btn.addEventListener("click", () => openReply(n.phone, n.id));
  if (state && state.error) {
    return el(
      "div",
      { className: "reply-wrap" },
      btn,
      el("div", { className: "reply-error", textContent: state.error }),
    );
  }
  return btn;
}

function renderCall(call) {
  const box = document.getElementById("call");
  if (!box) return;
  const caller = document.getElementById("call-caller");
  if (!call || call.state !== "ringing" || typeof call.phone !== "string" || !call.phone) {
    box.hidden = true;
    delete box.dataset.phone;
    return;
  }
  if (caller) caller.textContent = call.caller || "Unknown caller";
  box.dataset.phone = call.phone;
  box.hidden = false;
}

// Find my phone. The phone rings until Stop, its "Found it" button, or ring_secs.
const RING_FALLBACK_MS = 60000;
const ringingPhones = new Map();

function stopRinging(id) {
  const state = ringingPhones.get(id);
  if (!state) return;
  clearTimeout(state.timer);
  ringingPhones.delete(id);
}

function ringDurationMs() {
  const secs = lastLink && Number(lastLink.ring_secs);
  return secs > 0 ? secs * 1000 : RING_FALLBACK_MS;
}

function beginRinging(id) {
  stopRinging(id);
  const timer = setTimeout(() => {
    if (!ringingPhones.has(id)) return;
    ringingPhones.delete(id);
    invoke("ring_phone", { id, action: "ring_stop" }).catch(() => {});
    if (lastLink) renderLink(lastLink);
  }, ringDurationMs());
  ringingPhones.set(id, { timer });
}

function isRinging(id) {
  return ringingPhones.has(id);
}

function pruneRinging(link) {
  for (const id of [...ringingPhones.keys()]) {
    const phone = link.phones.find((p) => p.id === id);
    if (!phone || !phone.connected) stopRinging(id);
  }
}

function renderLink(link) {
  lastLink = link;
  pruneRinging(link);
  renderCall(link.call);
  const liveNotes = new Set(link.notifications.map((n) => noteKey(n.phone, n.id)));
  for (const [key, state] of composers) {
    if (!liveNotes.has(key)) {
      clearTimeout(state.sentTimer);
      composers.delete(key);
    }
  }
  const list = document.getElementById("linked");
  list.replaceChildren(
    ...link.phones.map((p) => {
      let meta = p.connected ? "Connected over Wi-Fi" : "Not connected";
      if (p.connected && p.battery) meta += ` · ${p.battery[0]}%${p.battery[1] ? " ⚡" : ""}`;
      const forget = el("button", { className: "link", textContent: "Forget" });
      forget.addEventListener("click", async () => {
        if (!confirm(`Forget ${p.name}? You'll need to pair it again.`)) return;
        try { await invoke("forget_phone", { id: p.id }); await refresh(); } catch (e) { showLinkError(String(e)); }
      });
      const buttons = [];
      if (p.connected) {
        const sendFile = el("button", {
          type: "button",
          className: "secondary",
          textContent: "Send file\u2026",
        });
        sendFile.addEventListener("click", async () => {
          sendFile.disabled = true;
          try {
            await chooseAndSendFile(p.id, p.name);
          } finally {
            sendFile.disabled = false;
          }
        });
        const ringing = isRinging(p.id);
        const ring = el("button", {
          type: "button",
          className: ringing ? "secondary ring-stop" : "secondary",
          textContent: ringing ? "Stop" : "Ring",
          title: ringing ? "Stop ringing" : "Ring phone",
        });
        ring.addEventListener("click", async () => {
          const stop = isRinging(p.id);
          ring.disabled = true;
          try {
            await invoke("ring_phone", { id: p.id, action: stop ? "ring_stop" : "ring" });
            if (stop) stopRinging(p.id);
            else beginRinging(p.id);
            showLinkError(null);
            if (lastLink) renderLink(lastLink);
          } catch (e) {
            showLinkError(String(e));
          } finally {
            ring.disabled = false;
          }
        });
        const send = el("button", {
          type: "button",
          className: "secondary",
          textContent: "Send clipboard",
        });
        send.addEventListener("click", async () => {
          send.disabled = true;
          try {
            await invoke("send_clipboard", { id: p.id });
            showLinkError(null);
            showLinkConfirm(`Clipboard sent to ${p.name}.`);
          } catch (e) {
            showLinkConfirm(null);
            showLinkError(String(e));
          } finally {
            send.disabled = false;
          }
        });
        buttons.push(sendFile, ring, send);
      }
      buttons.push(forget);
      return el(
        "li",
        { className: "device" },
        el("div", { className: "info" },
          el("div", { className: "name" }, el("span", { className: `dot${p.connected ? " on" : ""}` }), p.name),
          el("div", { className: "meta", textContent: meta })),
        ...buttons,
      );
    }),
  );
  document.getElementById("linked-empty").hidden = link.phones.length > 0;

  const focusedReply = captureReplyFocus();
  for (const n of link.notifications) absorbStatusReply(n);
  const notes = document.getElementById("notifications");
  notes.replaceChildren(
    ...link.notifications.slice(0, 20).map((n) => {
      const dismiss = el("button", {
        className: "dismiss",
        type: "button",
        textContent: "\u00d7",
        title: "Dismiss",
      });
      dismiss.setAttribute("aria-label", "Dismiss");
      dismiss.addEventListener("click", async () => {
        // link_status flattens NotificationView, so phone and id sit beside
        // app_name/title/text (not under n.notification). See
        // link_status_flattens_phone_and_id_for_the_dismiss_button.
        const phone = n.phone;
        const id = n.id;
        if (typeof phone !== "string" || typeof id !== "string") {
          showLinkError("could not dismiss this notification");
          return;
        }
        dismiss.disabled = true;
        try {
          await invoke("dismiss_notification", { phone, id });
          await refreshLink();
        } catch (e) {
          showLinkError(String(e));
          dismiss.disabled = false;
        }
      });
      return el(
        "li",
        {},
        el(
          "div",
          { className: "body" },
          el("div", { className: "app", textContent: `${n.app_name || n.app} · ${new Date(n.time).toLocaleTimeString()}` }),
          n.title ? el("div", { className: "title", textContent: n.title }) : "",
          n.text ? el("div", { className: "text", textContent: n.text }) : "",
          n.replyable ? replyControls(n) : "",
        ),
        dismiss,
      );
    }),
  );
  restoreReplyFocus(focusedReply);
  document.getElementById("notif-empty").hidden = link.notifications.length > 0;
  renderFiles(link);
  showLinkError(link.error);
  if (link.error) showLinkConfirm(null);
}

function showLinkError(msg) {
  const e = document.getElementById("link-error");
  e.textContent = msg || "";
  e.hidden = !msg;
}

let confirmTimer = null;
function showLinkConfirm(msg) {
  const e = document.getElementById("link-confirm");
  if (!e) return;
  e.textContent = msg || "";
  e.hidden = !msg;
  clearTimeout(confirmTimer);
  if (msg) confirmTimer = setTimeout(() => showLinkConfirm(null), 4000);
}

let pairingTimer = null;
function hidePairing() {
  document.getElementById("pairing").hidden = true;
  document.getElementById("qr").replaceChildren();
  clearTimeout(pairingTimer);
}

document.getElementById("pair-btn").addEventListener("click", async () => {
  try {
    const p = await invoke("start_pairing");
    // The SVG is generated by our own Rust code from the pairing link, never from outside input.
    document.getElementById("qr").innerHTML = p.qr_svg;
    document.getElementById("pair-expiry").textContent = Math.round(p.expires_in_secs / 60);
    document.getElementById("pairing").hidden = false;
    showLinkError(null);
    clearTimeout(pairingTimer);
    pairingTimer = setTimeout(hidePairing, p.expires_in_secs * 1000);
  } catch (e) {
    showLinkError(String(e));
  }
});
document.getElementById("pair-done").addEventListener("click", hidePairing);
const dropAskCancel = document.getElementById("drop-ask-cancel");
if (dropAskCancel) dropAskCancel.addEventListener("click", hideDropAsk);

async function sendCallAction(action) {
  const box = document.getElementById("call");
  const phone = box && box.dataset.phone;
  if (!phone) return;
  const buttons = box.querySelectorAll("button");
  for (const b of buttons) b.disabled = true;
  try {
    await invoke("call_action", { phone, action });
    showLinkError(null);
  } catch (e) {
    showLinkError(String(e));
  } finally {
    for (const b of buttons) b.disabled = false;
  }
}
document.getElementById("call-decline").addEventListener("click", () => sendCallAction("decline"));
document.getElementById("call-silence").addEventListener("click", () => sendCallAction("silence"));

let knownPhones = null;
let linkRefreshEpoch = 0;
async function refreshLink() {
  const epoch = ++linkRefreshEpoch;
  const link = await invoke("link_status");
  // A slow read must not hide a banner a newer refresh already showed.
  if (epoch !== linkRefreshEpoch) return link;
  // Close the QR code automatically when a new phone finishes pairing.
  const ids = link.phones.map((p) => p.id).join(",");
  if (knownPhones !== null && ids !== knownPhones && link.phones.length > knownPhones.split(",").filter(Boolean).length) hidePairing();
  knownPhones = ids;
  renderLink(link);
  return link;
}

async function refresh() {
  refreshLink().catch((e) => showLinkError(String(e)));
  const status = await invoke("status");
  renderDevices(status);
  renderTools(status);
  if (!status.adb.found) showError(INSTALL_HINT.adb);
  else showError(status.error);
  return status;
}

async function refreshUpdate() {
  const u = await invoke("update_status");
  const box = document.getElementById("update");
  if (u.state === "ready") {
    document.getElementById("update-text").textContent = `Seam ${u.version} is ready. Restart Seam to update.`;
    box.hidden = false;
  } else {
    box.hidden = true;
  }
  let v = document.getElementById("version");
  if (!v) {
    v = el("p", { id: "version", className: "version" });
    document.body.append(v);
  }
  v.textContent = `Seam ${u.current}`;
}

document.getElementById("update-btn").addEventListener("click", async (e) => {
  e.target.disabled = true;
  e.target.textContent = "Restarting…";
  try {
    await invoke("restart_to_update");
  } catch (err) {
    showError(String(err));
    e.target.disabled = false;
    e.target.textContent = "Restart";
  }
});

async function waitForLink() {
  // The link server starts in the background; give it a few seconds.
  for (let i = 0; i < 30; i++) {
    const link = await invoke("link_status");
    if (link.running || link.error) return link;
    await new Promise((r) => setTimeout(r, 200));
  }
  return invoke("link_status");
}

(async () => {
  try {
    listenForReplyResults();
    listenForIncomingCalls();
    listenForFileTransfers();
    const status = await refresh();
    const link = await waitForLink();
    const rendered = document.querySelectorAll("#tools li").length === 2 && !!document.getElementById("pair-btn");
    await invoke("frontend_ready", {
      ok: rendered,
      detail: `window rendered; link ${link.running ? `listening on ${link.port}` : `not running (${link.error || "unknown"})`}; adb ${status.adb.found ? "found" : "missing"}${status.adb.bundled ? " (built in)" : ""}, scrcpy ${status.scrcpy.found ? "found" : "missing"}${status.scrcpy.bundled ? " (built in)" : ""}, ${status.devices.length} device(s)`,
    });
  } catch (e) {
    await invoke("frontend_ready", { ok: false, detail: `startup error: ${e}` }).catch(() => {});
  }
  setInterval(() => refresh().catch((e) => showError(String(e))), 2000);
  refreshUpdate().catch(() => {});
  setInterval(() => refreshUpdate().catch(() => {}), 60000);
})();
