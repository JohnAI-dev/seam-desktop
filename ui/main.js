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

function renderLink(link) {
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
      return el(
        "li",
        { className: "device" },
        el("div", { className: "info" },
          el("div", { className: "name" }, el("span", { className: `dot${p.connected ? " on" : ""}` }), p.name),
          el("div", { className: "meta", textContent: meta })),
        forget,
      );
    }),
  );
  document.getElementById("linked-empty").hidden = link.phones.length > 0;

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
        ),
        dismiss,
      );
    }),
  );
  document.getElementById("notif-empty").hidden = link.notifications.length > 0;
  showLinkError(link.error);
}

function showLinkError(msg) {
  const e = document.getElementById("link-error");
  e.textContent = msg || "";
  e.hidden = !msg;
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

let knownPhones = null;
async function refreshLink() {
  const link = await invoke("link_status");
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
})();
