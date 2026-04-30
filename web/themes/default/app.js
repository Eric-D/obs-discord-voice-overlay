// Default theme: classic flex-row of cards with avatar + name + halo.
(() => {
  "use strict";

  const cards = document.getElementById("cards");
  const pill = document.getElementById("status-pill");

  /** @type {Map<string, HTMLElement>} */
  const nodes = new Map();

  // Inline SVG fallback for unreachable / 404 avatar URLs. No frills — a
  // neutral grey circle keeps the layout intact when the CDN drops a request.
  const FALLBACK_AVATAR =
    "data:image/svg+xml;utf8," +
    encodeURIComponent(
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">' +
        '<circle cx="32" cy="32" r="32" fill="#5865f2"/>' +
        '<circle cx="32" cy="26" r="11" fill="#fff"/>' +
        '<path d="M10 60c0-12 10-20 22-20s22 8 22 20" fill="#fff"/>' +
        "</svg>"
    );

  function attachAvatarFallback(img) {
    img.onerror = () => {
      img.onerror = null;
      img.src = FALLBACK_AVATAR;
    };
  }

  function safeParse(data) {
    try {
      return JSON.parse(data);
    } catch (err) {
      console.warn("bad SSE payload", err);
      return null;
    }
  }

  // URL options (see README for the full list).
  const params = new URLSearchParams(window.location.search);
  const opts = {
    speakingOnly: params.get("speaking_only") === "1",
    hide: new Set(
      (params.get("hide") || "")
        .split(",")
        .map((s) => s.trim())
        .filter(Boolean)
    ),
    size: parseInt(params.get("size") || "", 10),
  };
  if (opts.speakingOnly) document.body.dataset.speakingOnly = "true";
  if (Number.isFinite(opts.size) && opts.size >= 24 && opts.size <= 256) {
    document.documentElement.style.setProperty("--avatar-size", opts.size + "px");
  }
  const isHidden = (id) => opts.hide.has(String(id));

  function renderCard(p) {
    const card = document.createElement("div");
    card.className = "card" + (p.speaking ? " speaking" : "");
    card.dataset.userId = p.user_id;

    const wrap = document.createElement("div");
    wrap.className = "avatar-wrap";
    const img = document.createElement("img");
    img.className = "avatar";
    img.alt = "";
    attachAvatarFallback(img);
    img.src = p.avatar_url;
    wrap.appendChild(img);

    if (p.deaf || p.self_deaf) {
      const b = document.createElement("div");
      b.className = "badge deaf";
      b.textContent = "D";
      wrap.appendChild(b);
    } else if (p.mute || p.self_mute) {
      const b = document.createElement("div");
      b.className = "badge mute";
      b.textContent = "M";
      wrap.appendChild(b);
    }
    card.appendChild(wrap);

    const name = document.createElement("div");
    name.className = "name";
    name.textContent = p.display_name;
    card.appendChild(name);

    return card;
  }

  function setStatus(connected) {
    if (connected) {
      pill.classList.add("hidden");
    } else {
      pill.textContent = "disconnected";
      pill.classList.remove("hidden");
    }
  }

  function applyState(state) {
    // Replace the full set.
    cards.innerHTML = "";
    nodes.clear();
    for (const p of state.participants) {
      if (isHidden(p.user_id)) continue;
      const node = renderCard(p);
      cards.appendChild(node);
      nodes.set(p.user_id, node);
    }
    setStatus(state.connected !== false);
  }

  function applyJoin(p) {
    if (isHidden(p.user_id)) return;
    if (nodes.has(p.user_id)) {
      applyUpdate(p);
      return;
    }
    const node = renderCard(p);
    cards.appendChild(node);
    nodes.set(p.user_id, node);
  }

  function applyLeave(user_id) {
    const node = nodes.get(user_id);
    if (node) {
      node.remove();
      nodes.delete(user_id);
    }
  }

  function applyUpdate(p) {
    const old = nodes.get(p.user_id);
    if (!old) {
      applyJoin(p);
      return;
    }
    const next = renderCard(p);
    old.replaceWith(next);
    nodes.set(p.user_id, next);
  }

  function applySpeaking(user_id, speaking) {
    const node = nodes.get(user_id);
    if (!node) return;
    node.classList.toggle("speaking", !!speaking);
  }

  function connect() {
    const es = new EventSource("/events");
    es.addEventListener("state", (e) => {
      const v = safeParse(e.data);
      if (v) applyState(v);
    });
    es.addEventListener("participant_join", (e) => {
      const v = safeParse(e.data);
      if (v) applyJoin(v);
    });
    es.addEventListener("participant_leave", (e) => {
      const v = safeParse(e.data);
      if (v) applyLeave(v.user_id);
    });
    es.addEventListener("voice_state_update", (e) => {
      const v = safeParse(e.data);
      if (v) applyUpdate(v);
    });
    es.addEventListener("speaking_start", (e) => {
      const v = safeParse(e.data);
      if (v) applySpeaking(v.user_id, true);
    });
    es.addEventListener("speaking_stop", (e) => {
      const v = safeParse(e.data);
      if (v) applySpeaking(v.user_id, false);
    });
    es.addEventListener("connection", (e) => {
      const v = safeParse(e.data);
      if (v) setStatus(v.connected);
    });
    es.onerror = () => {
      // EventSource auto-reconnects; show the disconnected pill in the meantime.
      setStatus(false);
    };
  }

  connect();
})();
