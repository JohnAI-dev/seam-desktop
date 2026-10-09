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
      const meta = `${STATE_TEXT[d.state] || d.state} · ${d.wireless ? "Wi-Fi" : "USB"}`;
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
      ? el("span", { className: "ok", textContent: t.version || "found" })
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

async function refresh() {
  const status = await invoke("status");
  renderDevices(status);
  renderTools(status);
  if (!status.adb.found) showError(INSTALL_HINT.adb);
  else showError(status.error);
  return status;
}

(async () => {
  try {
    const status = await refresh();
    const rendered = document.querySelectorAll("#tools li").length === 2;
    await invoke("frontend_ready", {
      ok: rendered,
      detail: `window rendered; adb ${status.adb.found ? "found" : "missing"}, scrcpy ${status.scrcpy.found ? "found" : "missing"}, ${status.devices.length} device(s)`,
    });
  } catch (e) {
    await invoke("frontend_ready", { ok: false, detail: `startup error: ${e}` }).catch(() => {});
  }
  setInterval(() => refresh().catch((e) => showError(String(e))), 3000);
})();
