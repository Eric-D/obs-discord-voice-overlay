// Minimal theme: text-only list of names; speaking = bold/full opacity.
(() => {
  "use strict";

  const list = document.getElementById("speakers");
  /** @type {Map<string, HTMLLIElement>} */
  const nodes = new Map();

  // Minimal theme is text-only, but we keep the same try/catch wrapper so a
  // garbled SSE payload doesn't break the next event handler.
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
  };
  if (opts.speakingOnly) document.body.dataset.speakingOnly = "true";
  const isHidden = (id) => opts.hide.has(String(id));

  function classesFor(p) {
    const cls = [];
    if (p.speaking) cls.push("speaking");
    if (p.deaf || p.self_deaf) cls.push("deaf");
    else if (p.mute || p.self_mute) cls.push("muted");
    return cls.join(" ");
  }

  function makeNode(p) {
    const li = document.createElement("li");
    li.dataset.userId = p.user_id;
    li.textContent = p.display_name;
    li.className = classesFor(p);
    return li;
  }

  function applyState(state) {
    list.innerHTML = "";
    nodes.clear();
    for (const p of state.participants) {
      if (isHidden(p.user_id)) continue;
      const li = makeNode(p);
      list.appendChild(li);
      nodes.set(p.user_id, li);
    }
  }

  function applyJoin(p) {
    if (isHidden(p.user_id)) return;
    if (nodes.has(p.user_id)) return applyUpdate(p);
    const li = makeNode(p);
    list.appendChild(li);
    nodes.set(p.user_id, li);
  }

  function applyLeave(user_id) {
    const li = nodes.get(user_id);
    if (li) {
      li.remove();
      nodes.delete(user_id);
    }
  }

  function applyUpdate(p) {
    const li = nodes.get(p.user_id);
    if (!li) return applyJoin(p);
    li.textContent = p.display_name;
    li.className = classesFor(p);
  }

  function applySpeaking(user_id, speaking) {
    const li = nodes.get(user_id);
    if (li) li.classList.toggle("speaking", !!speaking);
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
  }

  connect();
})();
