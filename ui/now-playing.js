// Now playing. One card per connected phone. Art is a data URL; elements are
// built with DOM APIs. The progress bar advances locally while playing, using
// the same rules as seam_core::link::media::shown_position_ms.
(function () {
  "use strict";

  var statusSeq = 0;

  function invoke() {
    try {
      var core = window.__TAURI__ && window.__TAURI__.core;
      if (core && typeof core.invoke === "function") {
        return core.invoke.bind(core);
      }
    } catch (e) {
      return null;
    }
    return null;
  }

  function listen(name, handler) {
    try {
      var eventApi = window.__TAURI__ && window.__TAURI__.event;
      if (eventApi && typeof eventApi.listen === "function") {
        eventApi.listen(name, handler);
      }
    } catch (e) {
      // The card still refreshes on a timer.
    }
  }

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text) node.textContent = text;
    return node;
  }

  function setError(text) {
    var node = document.getElementById("now-playing-error");
    if (!node) return;
    node.textContent = text || "";
    node.hidden = !text;
  }

  function errorText(err) {
    if (!err) return "could not control playback";
    if (typeof err === "string") return err;
    if (err.message) return String(err.message);
    return String(err);
  }

  // Paused sessions stay put. Playing sessions add time since updatedMs and
  // stop at durationMs when that is greater than 0.
  function shownPositionMs(positionMs, durationMs, playing, updatedMs, nowMs) {
    positionMs = Number(positionMs) || 0;
    durationMs = Number(durationMs) || 0;
    updatedMs = Number(updatedMs) || 0;
    nowMs = Number(nowMs) || 0;
    if (positionMs < 0) positionMs = 0;
    if (durationMs < 0) durationMs = 0;
    var base = durationMs > 0 ? Math.min(positionMs, durationMs) : positionMs;
    if (!playing) return base;
    var elapsed = nowMs - updatedMs;
    if (!(elapsed > 0)) elapsed = 0;
    var pos = base + elapsed;
    if (durationMs > 0) return Math.min(pos, durationMs);
    return pos;
  }

  function formatTime(ms) {
    var total = Math.floor(Math.max(0, Number(ms) || 0) / 1000);
    var h = Math.floor(total / 3600);
    var m = Math.floor((total % 3600) / 60);
    var s = total % 60;
    var ss = s < 10 ? "0" + s : String(s);
    if (h > 0) {
      var mm = m < 10 ? "0" + m : String(m);
      return h + ":" + mm + ":" + ss;
    }
    return m + ":" + ss;
  }

  function snapshot(item) {
    return [
      item.phone_name || "",
      item.app || "",
      item.app_name || "",
      item.title || "",
      item.artist || "",
      item.album || "",
      item.playing ? "1" : "0",
      item.position_ms || 0,
      item.duration_ms || 0,
      item.updated_ms || 0,
      item.art || "",
    ].join("\n");
  }

  function findCard(list, phone) {
    var child = list.firstChild;
    while (child) {
      if (child.dataset && child.dataset.phone === phone) return child;
      child = child.nextSibling;
    }
    return null;
  }

  function applyProgress(card, now) {
    var shown = shownPositionMs(
      card.dataset.position,
      card.dataset.duration,
      card.dataset.playing === "1",
      card.dataset.updated,
      now,
    );
    var duration = Number(card.dataset.duration) || 0;
    var bar = card.querySelector("progress");
    var time = card.querySelector(".time");
    var label = duration > 0 ? formatTime(shown) + " / " + formatTime(duration) : formatTime(shown);
    if (bar) {
      if (duration > 0) {
        bar.max = duration;
        bar.value = shown;
      } else if (card.dataset.playing === "1") {
        bar.removeAttribute("value");
      } else {
        bar.max = 1;
        bar.value = 0;
      }
      bar.setAttribute("aria-valuetext", label);
    }
    if (time) time.textContent = label;
  }

  function sendControl(phone, action, button) {
    var call = invoke();
    if (!call) return;
    button.disabled = true;
    call("media_control", { phone: phone, action: action })
      .then(function () {
        setError("");
      })
      .catch(function (err) {
        setError(errorText(err));
      })
      .then(function () {
        button.disabled = false;
      });
  }

  function controlButton(phone, action, label, aria) {
    var button = el("button", "secondary", label);
    button.type = "button";
    button.dataset.action = action;
    button.setAttribute("aria-label", aria || label);
    button.addEventListener("click", function () {
      sendControl(phone, action, button);
    });
    return button;
  }

  function displayTitle(item) {
    if (item.title) return item.title;
    if (item.app_name) return item.app_name;
    return "Untitled";
  }

  function artistLine(item) {
    if (item.artist && item.album) return item.artist + " · " + item.album;
    return item.artist || item.album || "";
  }

  function appLine(item) {
    var app = item.app_name || item.app || "Phone";
    if (item.phone_name) return app + " · " + item.phone_name;
    return app;
  }

  function fillCard(card, item) {
    card.dataset.phone = item.phone;
    card.dataset.position = String(item.position_ms || 0);
    card.dataset.duration = String(item.duration_ms || 0);
    card.dataset.updated = String(item.updated_ms || 0);
    card.dataset.playing = item.playing ? "1" : "0";
    card.dataset.snapshot = snapshot(item);

    var artSlot = card.querySelector(".art-slot");
    if (artSlot) {
      artSlot.replaceChildren();
      if (item.art) {
        var img = el("img", "art");
        img.alt = displayTitle(item);
        img.src = item.art;
        artSlot.append(img);
      } else {
        artSlot.append(el("div", "art-fallback", "\u266a"));
      }
    }
    var app = card.querySelector(".app");
    if (app) app.textContent = appLine(item);
    var title = card.querySelector(".title");
    if (title) title.textContent = displayTitle(item);
    var artist = card.querySelector(".artist");
    if (artist) {
      var line = artistLine(item);
      artist.textContent = line;
      artist.hidden = !line;
    }
    var play = card.querySelector('[data-action="toggle"]');
    if (play) {
      var playLabel = item.playing ? "Pause" : "Play";
      play.textContent = playLabel;
      play.setAttribute("aria-label", playLabel);
    }
    applyProgress(card, Date.now());
  }

  function buildCard(item) {
    var card = el("article", "now-playing-card");
    card.append(el("div", "art-slot"));
    var body = el("div", "body");
    body.append(el("div", "app"));
    body.append(el("div", "title"));
    body.append(el("div", "artist"));
    var bar = document.createElement("progress");
    bar.max = 1;
    bar.value = 0;
    bar.setAttribute("aria-label", "Playback progress");
    body.append(bar);
    body.append(el("div", "time"));
    var controls = el("div", "controls");
    controls.append(controlButton(item.phone, "previous", "Previous"));
    controls.append(controlButton(item.phone, "toggle", "Play"));
    controls.append(controlButton(item.phone, "next", "Next"));
    controls.append(controlButton(item.phone, "volume_down", "\u2212", "Volume down"));
    controls.append(controlButton(item.phone, "volume_up", "+", "Volume up"));
    body.append(controls);
    card.append(body);
    fillCard(card, item);
    return card;
  }

  function render(link) {
    var list = document.getElementById("now-playing");
    if (!list) return;
    var items = (link && link.media) || [];
    var seen = {};
    var ordered = [];
    for (var i = 0; i < items.length; i++) {
      var item = items[i];
      if (!item || typeof item.phone !== "string" || !item.phone || seen[item.phone]) continue;
      seen[item.phone] = true;
      ordered.push(item);
      var card = findCard(list, item.phone);
      if (!card) {
        card = buildCard(item);
        list.append(card);
      } else if (card.dataset.snapshot !== snapshot(item)) {
        fillCard(card, item);
      }
    }
    var child = list.firstChild;
    while (child) {
      var next = child.nextSibling;
      if (!child.dataset || !seen[child.dataset.phone]) child.remove();
      child = next;
    }
    var cursor = list.firstChild;
    for (var n = 0; n < ordered.length; n++) {
      var node = findCard(list, ordered[n].phone);
      if (!node) continue;
      if (cursor !== node) list.insertBefore(node, cursor);
      else cursor = cursor.nextSibling;
    }
    var section = document.getElementById("now-playing-section");
    if (section) section.hidden = ordered.length === 0;
    var empty = document.getElementById("media-empty");
    if (empty) empty.hidden = ordered.length > 0;
    if (!ordered.length) setError("");
  }

  function refresh() {
    var call = invoke();
    if (!call) return;
    var seq = ++statusSeq;
    call("link_status")
      .then(function (status) {
        if (seq !== statusSeq) return;
        render(status);
      })
      .catch(function () {});
  }

  function tick() {
    var now = Date.now();
    var list = document.getElementById("now-playing");
    if (!list) return;
    var child = list.firstChild;
    while (child) {
      if (child.dataset && child.dataset.phone) applyProgress(child, now);
      child = child.nextSibling;
    }
  }

  function start() {
    refresh();
    window.setInterval(refresh, 2000);
    window.setInterval(tick, 250);
    listen("media", refresh);
  }

  try {
    if (document.readyState === "loading") {
      document.addEventListener("DOMContentLoaded", start);
    } else {
      start();
    }
  } catch (e) {
    // Never block the rest of the window.
  }
})();
