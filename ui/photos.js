// Recent photos strip. Thumbnails are data URLs; elements are built with DOM APIs.
(function () {
  "use strict";

  var lastRequested = null;
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
      // The strip still refreshes on a timer.
    }
  }

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) {
      node.className = className;
    }
    if (text) {
      node.textContent = text;
    }
    return node;
  }

  function setStatus(text) {
    var status = document.getElementById("photos-status");
    if (!status) {
      return;
    }
    status.textContent = text || "";
    status.hidden = !text;
  }

  function errorText(err) {
    if (!err) {
      return "could not save the photo";
    }
    if (typeof err === "string") {
      return err;
    }
    if (err.message) {
      return String(err.message);
    }
    return String(err);
  }

  function photoKey(photo) {
    return String(photo.phone || "") + "\n" + String(photo.id || "");
  }

  function sameRequest(photo) {
    return !!(
      lastRequested &&
      photo &&
      lastRequested.id === photo.id &&
      lastRequested.phone === photo.phone
    );
  }

  function photoFromButton(btn) {
    var saves = Number(btn.dataset.saves || "0");
    if (!saves) {
      saves = 0;
    }
    return {
      phone: btn.dataset.phone || "",
      id: btn.dataset.id || "",
      name: btn.dataset.name || "",
      saves: saves,
      pending: btn.dataset.pending === "1"
    };
  }

  function requestPhoto(photo, btn) {
    var call = invoke();
    if (!call || !photo || !photo.id) {
      return;
    }
    var name = photo.name || "photo";
    lastRequested = {
      phone: photo.phone,
      id: photo.id,
      saves: photo.saves || 0,
      acked: false
    };
    btn.disabled = true;
    setStatus("Saving " + name + "...");
    call("request_photo", { phone: photo.phone, id: photo.id })
      .then(function () {
        if (!sameRequest(photo)) {
          return;
        }
        lastRequested.acked = true;
        setStatus("Saving " + name + "...");
        refresh();
      })
      .catch(function (err) {
        if (sameRequest(photo)) {
          lastRequested = null;
        }
        setStatus(errorText(err));
        btn.disabled = false;
      });
  }

  function fillThumb(btn, photo) {
    var name = photo.name || "Photo";
    btn.dataset.phone = photo.phone || "";
    btn.dataset.id = photo.id || "";
    btn.dataset.name = name;
    btn.dataset.saves = String(photo.saves || 0);
    btn.dataset.pending = photo.pending ? "1" : "0";
    btn.dataset.photoKey = photoKey(photo);
    btn.className = photo.saved ? "photo-thumb saved" : "photo-thumb";
    btn.title = name;
    btn.setAttribute("aria-label", photo.saved ? "Saved " + name : "Save " + name);
    // Disabled only while this click or a server request is still outstanding.
    btn.disabled = !!(photo.pending || sameRequest(photo));

    var visual = btn.querySelector("img, .photo-placeholder");
    var wantData = typeof photo.thumb === "string" && photo.thumb.indexOf("data:image/") === 0;
    if (wantData) {
      if (!visual || visual.tagName !== "IMG") {
        if (visual) {
          btn.removeChild(visual);
        }
        visual = document.createElement("img");
        visual.alt = "";
        btn.insertBefore(visual, btn.firstChild);
      }
      if (visual.getAttribute("src") !== photo.thumb) {
        visual.src = photo.thumb;
      }
    } else if (!visual || !visual.classList.contains("photo-placeholder")) {
      if (visual) {
        btn.removeChild(visual);
      }
      btn.insertBefore(el("span", "photo-placeholder", "Photo"), btn.firstChild);
    }

    var label = btn.querySelector(".photo-name");
    if (!label) {
      label = el("span", "photo-name", name);
      btn.appendChild(label);
    } else if (label.textContent !== name) {
      label.textContent = name;
    }

    var badge = btn.querySelector(".photo-saved");
    if (photo.saved) {
      if (!badge) {
        btn.appendChild(el("span", "photo-saved", "Saved"));
      }
    } else if (badge) {
      btn.removeChild(badge);
    }
  }

  function thumbButton(photo) {
    var btn = document.createElement("button");
    btn.type = "button";
    fillThumb(btn, photo);
    btn.addEventListener("click", function () {
      requestPhoto(photoFromButton(btn), btn);
    });
    return btn;
  }

  function noteSaved(photos) {
    if (!lastRequested || !lastRequested.acked || !photos) {
      return;
    }
    for (var i = 0; i < photos.length; i++) {
      var p = photos[i];
      if (!p || p.id !== lastRequested.id || p.phone !== lastRequested.phone) {
        continue;
      }
      if (p.pending) {
        return;
      }
      // An already-saved row is not this click. Only a higher save count is.
      if ((p.saves || 0) > (lastRequested.saves || 0)) {
        setStatus("Saved " + (p.name || "photo"));
      } else {
        setStatus("Could not save " + (p.name || "the photo"));
      }
      lastRequested = null;
      return;
    }
    setStatus("Could not save the photo");
    lastRequested = null;
  }

  function render(photos) {
    var section = document.getElementById("photos-section");
    var strip = document.getElementById("photos");
    if (!section || !strip) {
      return;
    }
    noteSaved(photos);
    if (!photos || !photos.length) {
      section.hidden = true;
      return;
    }
    section.hidden = false;

    var wanted = {};
    var ordered = [];
    for (var i = 0; i < photos.length; i++) {
      var photo = photos[i];
      if (!photo || !photo.id) {
        continue;
      }
      var key = photoKey(photo);
      if (wanted[key]) {
        continue;
      }
      wanted[key] = photo;
      ordered.push(key);
    }

    var existing = {};
    var child = strip.firstChild;
    while (child) {
      var next = child.nextSibling;
      var have = child.dataset && child.dataset.photoKey;
      if (!have || !wanted[have] || existing[have] || child.tagName !== "BUTTON") {
        strip.removeChild(child);
      } else {
        existing[have] = child;
        fillThumb(child, wanted[have]);
      }
      child = next;
    }

    var cursor = strip.firstChild;
    for (var n = 0; n < ordered.length; n++) {
      var orderedKey = ordered[n];
      var node = existing[orderedKey];
      if (!node) {
        node = thumbButton(wanted[orderedKey]);
      }
      if (cursor !== node) {
        strip.insertBefore(node, cursor);
      } else {
        cursor = cursor.nextSibling;
      }
    }
  }

  function refresh() {
    var call = invoke();
    if (!call) {
      return;
    }
    var seq = ++statusSeq;
    call("link_status")
      .then(function (status) {
        if (seq !== statusSeq) {
          return;
        }
        render(status && status.photos);
      })
      .catch(function () {});
  }

  function start() {
    refresh();
    window.setInterval(refresh, 2000);
    listen("recent-photos", refresh);
    listen("file-transfer", refresh);
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
