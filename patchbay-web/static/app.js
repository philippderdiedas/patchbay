// SPDX-License-Identifier: GPL-3.0-or-later
// Shared helpers: texts from <body data-msg-…> (base.html), presence labels, the status toast.
const pbxMsg = (key, fallback) => document.body.dataset[key] || (fallback ?? key);
// toast text for a POST /call answer ({ok, message})
const pbxCallMsg = (r) => pbxMsg(r.ok ? "msgCallRinging" : (r.message === "not registered" ? "msgNotRegistered" : "msgCallFailed"));
const pbxPresence = (state) => pbxMsg("msgPresence" + state.charAt(0).toUpperCase() + state.slice(1), state);

function pbxToast(text, ok) {
  let t = document.getElementById("toast");
  if (!t) {
    t = document.createElement("div");
    t.id = "toast";
    t.className = "toast";
    t.setAttribute("role", "status");
    document.body.appendChild(t);
  }
  t.textContent = text;
  t.classList.toggle("toast-error", !ok);
  t.hidden = false;
  clearTimeout(t._timer);
  t._timer = setTimeout(() => { t.hidden = true; }, 5000);
}

// Theme switch (auto/light/dark, stored per browser) and target selectors.
(function () {
  const root = document.documentElement;
  const KEY = "pbx-theme";

  function applyTheme(mode) {
    root.dataset.theme = mode;
    document.querySelectorAll("[data-theme-set]").forEach((b) =>
      b.setAttribute("aria-pressed", String(b.dataset.themeSet === mode)));
  }

  document.addEventListener("DOMContentLoaded", () => {
    let mode = "auto";
    try { mode = localStorage.getItem(KEY) || "auto"; } catch (e) {}
    applyTheme(mode);
    document.querySelectorAll("[data-theme-set]").forEach((b) =>
      b.addEventListener("click", () => {
        const m = b.dataset.themeSet;
        try { localStorage.setItem(KEY, m); } catch (e) {}
        applyTheme(m);
      }));

    // Target selector: show only the value input that matches the chosen kind;
    // hidden inputs are disabled so they are not submitted.
    document.querySelectorAll("[data-target]").forEach((box) => {
      const kind = box.querySelector("select[data-kind]");
      const update = () => {
        box.querySelectorAll("[data-for]").forEach((el) => {
          const on = el.dataset.for.split(" ").includes(kind.value);
          el.hidden = !on;
          el.disabled = !on;
        });
      };
      kind.addEventListener("change", update);
      update();
    });

    // Add/edit forms (the same look on every admin page): a row's "Edit" button
    // (data-fill="<form id>", data-v-<field>="value") fills the form and switches it to editing:
    // [data-adding] / [data-editing] inside the form show while adding / editing, a heading with
    // data-edit-heading="<form id>" shows its data-edit-text, fields with data-edit-readonly are
    // read-only, the edited row is highlighted. Reset (Cancel) clears the id field [data-edit-key]
    // and goes back to adding.
    function editMode(form, on, row) {
      // not form.id: a field named "id" (blocklist, trunk rules) shadows it
      const fid = form.getAttribute("id");
      form.querySelectorAll("[data-editing]").forEach((el) => { el.hidden = !on; });
      form.querySelectorAll("[data-adding]").forEach((el) => { el.hidden = on; });
      form.querySelectorAll("[data-edit-readonly]").forEach((el) => { el.readOnly = on; });
      form.classList.toggle("editing", on);
      document.querySelectorAll(`[data-edit-heading="${fid}"]`).forEach((h) => {
        if (h.dataset.addText === undefined) h.dataset.addText = h.textContent;
        h.textContent = on ? h.dataset.editText : h.dataset.addText;
      });
      document.querySelectorAll(`tr.editing[data-edit-row="${fid}"]`).forEach((r) => {
        r.classList.remove("editing");
        r.removeAttribute("data-edit-row");
      });
      if (on && row) {
        row.classList.add("editing");
        row.dataset.editRow = fid;
      }
      // a card that only exists for editing (data-edit-card) shows while editing
      const card = form.closest("[data-edit-card]");
      if (card) card.hidden = !on;
      if (!on) form.querySelectorAll("[data-edit-key]").forEach((el) => { el.value = ""; });
    }
    document.addEventListener("reset", (e) => {
      const form = e.target;
      if (!form.querySelector || !form.querySelector("[data-edit-key]")) return;
      editMode(form, false);
      // after the reset has restored the defaults: update fields that depend on others
      setTimeout(() => form.querySelectorAll("select:not([data-autosubmit])").forEach((el) => el.dispatchEvent(new Event("change", { bubbles: true }))));
    }, true);

    document.addEventListener("click", (e) => {
      const btn = e.target.closest("button[data-fill]");
      if (!btn) return;
      const form = document.getElementById(btn.dataset.fill);
      if (!form) return;
      const changed = [];
      for (const [k, v] of Object.entries(btn.dataset)) {
        if (!k.startsWith("v") || k === "v" || k.length < 2) continue;
        // dataset turns data-v-kc_group into vKc_group
        const name = k.charAt(1).toLowerCase() + k.slice(2);
        const field = form.elements.namedItem(name);
        if (!field) continue;
        if (typeof RadioNodeList !== "undefined" && field instanceof RadioNodeList) {
          // several fields of that name: checkbox/radio group (value: comma list) or the
          // alternative value inputs of a target selector (all get the value, one is enabled)
          const vals = v.split(",");
          field.forEach((el) => {
            if (el.type === "checkbox" || el.type === "radio") el.checked = vals.includes(el.value);
            else el.value = v;
          });
        } else if (field.type === "checkbox") field.checked = v !== "";
        else field.value = v;
        changed.push(field instanceof Element ? field : field[0]);
      }
      // show/hide what depends on the filled fields (target selector, data-show-when)
      changed.forEach((el) => el && !el.matches("[data-autosubmit]") && el.dispatchEvent(new Event("change", { bubbles: true })));
      if (form.querySelector("[data-edit-key]")) editMode(form, true, btn.closest("tr"));
      const card = form.closest(".card") || form;
      card.scrollIntoView({ behavior: "smooth", block: "start" });
      card.classList.add("flash");
      setTimeout(() => card.classList.remove("flash"), 1200);
      const first = form.querySelector("input:not([type=hidden]):not([readonly]), select, textarea");
      if (first) first.focus({ preventScroll: true });
    });

    // Reorderable tables (data-reorder="<form id>", rows with data-id): drag a row by its handle
    // (mouse, pen or touch; Esc cancels) or use the ▲ ▼ buttons; the new order of ids goes into
    // the form's "order" field and is saved.
    document.querySelectorAll("table[data-reorder]").forEach((table) => {
      const form = document.getElementById(table.dataset.reorder);
      if (!form) return;
      const rows = () => Array.from(table.querySelectorAll("tr[data-id]"));
      const ids = () => rows().map((r) => r.dataset.id).join(",");
      const save = () => {
        form.elements.namedItem("order").value = ids();
        form.submit();
      };
      const start = ids();
      const restore = () => {
        const byId = new Map(rows().map((r) => [r.dataset.id, r]));
        const parent = rows()[0].parentNode;
        start.split(",").forEach((id) => parent.appendChild(byId.get(id)));
      };
      // the dragged row moves in the DOM, which drops a pointer capture, so the drag listens on
      // the document instead
      let drag = null; // { row, pointerId }
      const finish = (keep) => {
        if (!drag) return;
        drag.row.classList.remove("dragging");
        drag = null;
        if (!keep) restore();
        else if (ids() !== start) save();
      };
      document.addEventListener("pointermove", (e) => {
        if (!drag || e.pointerId !== drag.pointerId) return;
        e.preventDefault();
        const row = drag.row;
        const over = rows().find((r) => {
          const box = r.getBoundingClientRect();
          return r !== row && e.clientY >= box.top && e.clientY < box.bottom;
        });
        if (!over) return;
        const box = over.getBoundingClientRect();
        over.parentNode.insertBefore(row, e.clientY > box.top + box.height / 2 ? over.nextSibling : over);
      });
      document.addEventListener("pointerup", (e) => { if (drag && e.pointerId === drag.pointerId) finish(true); });
      document.addEventListener("pointercancel", (e) => { if (drag && e.pointerId === drag.pointerId) finish(false); });
      rows().forEach((row) => {
        // grip: the cell holding the ☰ handle (bigger target for fingers), not its ▲ ▼ buttons
        const grip = row.querySelector(".draghandle")?.closest("td");
        if (grip) {
          grip.classList.add("dragcell");
          grip.addEventListener("pointerdown", (e) => {
            if (drag || e.button !== 0 || !e.isPrimary || e.target.closest("button")) return;
            e.preventDefault();
            // no implicit touch capture: moves go to whatever is under the finger and reach the document
            if (e.target.hasPointerCapture?.(e.pointerId)) e.target.releasePointerCapture(e.pointerId);
            drag = { row, pointerId: e.pointerId };
            row.classList.add("dragging");
          });
        }
        row.querySelectorAll("button[data-move]").forEach((b) => b.addEventListener("click", () => {
          if (drag) return;
          const up = b.dataset.move === "-1";
          const other = up ? row.previousElementSibling : row.nextElementSibling;
          if (!other || !other.dataset.id) return;
          row.parentNode.insertBefore(row, up ? other : other.nextSibling);
          save();
        }));
      });
      document.addEventListener("keydown", (e) => { if (e.key === "Escape") finish(false); });
      window.addEventListener("blur", () => finish(false)); // released outside the window
    });

    // Show parts of a form depending on a select: data-show-when="field=value" or
    // "field!=a,b" (shown unless the value is one of a, b)
    document.querySelectorAll("[data-show-when]").forEach((el) => {
      const spec = el.dataset.showWhen;
      const neg = spec.includes("!=");
      const [field, list] = spec.split(neg ? "!=" : "=");
      const values = list.split(",");
      const sel = el.closest("form")?.querySelector(`[name="${field}"]`);
      if (!sel) return;
      const update = () => { el.hidden = values.includes(sel.value) === neg; };
      sel.addEventListener("change", update);
      update();
    });

    // Confirmation for destructive forms
    document.querySelectorAll("form[data-confirm]").forEach((f) =>
      f.addEventListener("submit", (e) => { if (!confirm(f.dataset.confirm)) e.preventDefault(); }));
  });
})();

// Click-to-call, ring test and the provisioning watcher (QR page).
(function () {
  async function post(url, data) {
    const body = new URLSearchParams(data || {});
    const r = await fetch(url, { method: "POST", body, credentials: "same-origin" });
    if (!r.ok) throw new Error(r.status);
    return r.json();
  }

  document.addEventListener("submit", async (e) => {
    const f = e.target.closest("form[data-c2c]");
    if (!f) return;
    e.preventDefault();
    try {
      const r = await post("/call", Object.fromEntries(new FormData(f)));
      pbxToast(pbxCallMsg(r), r.ok);
    } catch (_) { pbxToast(pbxMsg("msgCallFailed"), false); }
  });

  document.addEventListener("click", async (e) => {
    const tt = e.target.closest("button[data-trunktest]");
    if (tt) {
      e.preventDefault();
      try {
        const r = await post(`/admin/trunks/${tt.dataset.trunktest}/test`);
        pbxToast(r.message, r.ok);
      } catch (_) { pbxToast(pbxMsg("msgCallFailed"), false); }
      return;
    }
    const rt = e.target.closest("button[data-ringtest]");
    if (rt) {
      e.preventDefault();
      try {
        const r = await post(`/devices/${rt.dataset.ringtest}/ringtest`);
        pbxToast(r.ok ? pbxMsg("msgRingtest") : pbxMsg("msgNotRegistered"), r.ok);
      } catch (_) { pbxToast(pbxMsg("msgCallFailed"), false); }
    }
  });

  document.addEventListener("DOMContentLoaded", () => {
    const w = document.querySelector("[data-provwatch]");
    if (!w) return;
    const id = w.dataset.provwatch;
    const started = Date.now();
    const timer = setInterval(async () => {
      if (Date.now() - started > 16 * 60 * 1000) { clearInterval(timer); return; }
      try {
        const r = await (await fetch(`/devices/${id}/provstatus`, { credentials: "same-origin" })).json();
        if (r.fetched) {
          document.querySelectorAll("[data-before-fetch]").forEach((el) => { el.hidden = true; });
          document.querySelector("[data-fetched]").hidden = false;
        }
        if (r.registered) {
          document.querySelector("[data-registered]").hidden = false;
          clearInterval(timer);
        }
      } catch (_) {}
    }, 2000);
  });
  // Voicemail greeting: record in the browser (MediaRecorder), preview, upload
  document.addEventListener("DOMContentLoaded", () => {
    document.querySelectorAll("[data-recorder]").forEach((box) => {
      const $ = (s) => box.querySelector(s);
      const start = $("[data-rec-start]"), stop = $("[data-rec-stop]"), save = $("[data-rec-save]");
      const time = $("[data-rec-time]"), preview = $("[data-rec-preview]");
      const max = parseInt(box.dataset.max || "60", 10);
      let rec = null, chunks = [], blob = null, timer = null, t0 = 0;
      if (!navigator.mediaDevices || !window.MediaRecorder) {
        start.disabled = true;
        time.textContent = pbxMsg("msgRecUnsupported");
        return;
      }
      const finish = () => { if (rec && rec.state === "recording") rec.stop(); };
      start.addEventListener("click", async () => {
        let stream;
        try { stream = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: true, noiseSuppression: true } }); }
        catch (e) { pbxToast(pbxMsg("msgRecNomic"), false); return; }
        chunks = []; blob = null;
        rec = new MediaRecorder(stream);
        rec.ondataavailable = (e) => { if (e.data.size) chunks.push(e.data); };
        rec.onstop = () => {
          clearInterval(timer);
          stream.getTracks().forEach((tr) => tr.stop());
          blob = new Blob(chunks, { type: rec.mimeType || "audio/webm" });
          preview.src = URL.createObjectURL(blob);
          preview.hidden = false; save.hidden = false; stop.hidden = true; start.hidden = false;
          start.textContent = pbxMsg("msgRecAgain");
          start.classList.remove("primary");
          time.textContent = "";
        };
        rec.start();
        t0 = Date.now();
        start.hidden = true; stop.hidden = false; save.hidden = true; preview.hidden = true;
        const tick = () => {
          const s = Math.floor((Date.now() - t0) / 1000);
          time.textContent = "● " + s + " / " + max + " s";
          if (s >= max) finish();
        };
        tick(); timer = setInterval(tick, 250);
      });
      stop.addEventListener("click", finish);
      save.addEventListener("click", async () => {
        if (!blob) return;
        save.disabled = true;
        const fd = new FormData();
        const ext = blob.type.includes("ogg") ? "ogg" : blob.type.includes("mp4") ? "m4a" : "webm";
        fd.append("file", blob, "recording." + ext);
        const r = await fetch("/voicemail/greeting", { method: "POST", body: fd });
        if (r.ok) { location.href = "/voicemail#greeting"; location.reload(); }
        else { save.disabled = false; pbxToast((await r.text()).replace(/<[^>]+>/g, " ").replace(/\s+/g, " ").trim().slice(0, 200) || pbxMsg("msgSaveFailed"), false); }
      });
    });
  });
  // Live registration state of devices (data-regpoll="<json url>", cells data-reg="<device id>")
  document.addEventListener("DOMContentLoaded", () => {
    const table = document.querySelector("[data-regpoll]");
    if (!table) return;
    const names = { online: "msgRegOnline", unreachable: "msgRegUnreachable", offline: "msgRegOffline" };
    const poll = async () => {
      if (document.hidden) return;
      try {
        const r = await fetch(table.dataset.regpoll, { credentials: "same-origin" });
        if (!r.ok) return;
        const states = await r.json();
        table.querySelectorAll("[data-reg]").forEach((td) => {
          const s = states[td.dataset.reg];
          if (!s || !s.enabled) return;
          td.textContent = "";
          const span = document.createElement("span");
          span.className = "reg reg-" + s.state;
          span.textContent = pbxMsg(names[s.state] || "msgRegOffline");
          td.appendChild(span);
          if (s.detail) {
            td.appendChild(document.createElement("br"));
            const sm = document.createElement("small");
            sm.className = "muted";
            sm.textContent = s.detail;
            td.appendChild(sm);
          }
        });
      } catch (_) { /* keep the last state */ }
    };
    setInterval(poll, 10000);
    document.addEventListener("visibilitychange", poll);
  });
  // selects that submit their form on change (e.g. ring delay)
  document.addEventListener("change", (e) => {
    const s = e.target.closest("select[data-autosubmit]");
    if (s && s.form) s.form.requestSubmit ? s.form.requestSubmit() : s.form.submit();
  });
})();

// List tables: sort by clicking a column header, filter rows, page through long lists.
// Applies to every table in .tablewrap whose first row holds the <th> headers; form tables
// (table.form) and tables marked data-plain are left alone. A cell's data-sort overrides its text.
(function () {
  const PAGE = 50;
  const coll = new Intl.Collator(document.documentElement.lang || undefined, { numeric: true, sensitivity: "base" });

  // Sort key: dates "DD.MM.YY[YY] HH:MM", durations "m:ss", numbers ("12 s", "35 ms", "1.234"), else text.
  function key(cell) {
    const raw = (cell.dataset.sort ?? cell.textContent).trim();
    let m = raw.match(/^(\d{1,2})\.(\d{1,2})\.(\d{2,4})?(?:\s+(\d{1,2}):(\d{2}))?$/);
    if (m) {
      let y = m[3] ? +m[3] : new Date().getFullYear();
      if (y < 100) y += 2000;
      return { n: (((y * 100 + +m[2]) * 100 + +m[1]) * 100 + +(m[4] || 0)) * 100 + +(m[5] || 0) };
    }
    m = raw.match(/^(\d+):(\d{2})$/);
    if (m) return { n: +m[1] * 60 + +m[2] };
    m = raw.match(/^[+-]?\d+(?:[.,]\d+)?(?:\s*[a-zA-Z%]{0,3})$/);
    if (m && !/^0\d/.test(raw)) return { n: parseFloat(raw.replace(",", ".")) };
    return { s: raw };
  }

  function cmp(a, b) {
    if (a.s === "" && b.s !== "") return 1; // empty cells last
    if (b.s === "" && a.s !== "") return -1;
    if (a.n !== undefined && b.n !== undefined) return a.n - b.n;
    if (a.n !== undefined) return -1;
    if (b.n !== undefined) return 1;
    return coll.compare(a.s, b.s);
  }

  function enhance(table) {
    const rows = Array.from(table.rows);
    const head = rows[0];
    if (!head || !head.cells.length || Array.from(head.cells).some((c) => c.tagName !== "TH")) return;
    const body = rows.slice(1);
    if (body.length < 2) return;
    const tbody = body[0].parentNode;
    let shown = body.slice(), page = 0;

    // toolbar: filter + row count, pager below
    const bar = document.createElement("div");
    bar.className = "tablebar";
    const filter = document.createElement("input");
    filter.type = "search";
    filter.placeholder = pbxMsg("msgFilter", "Filter…");
    filter.setAttribute("aria-label", filter.placeholder);
    const count = document.createElement("span");
    count.className = "muted";
    bar.append(filter, count);
    const wrap = table.closest(".tablewrap") || table;
    if (body.length >= 8) wrap.before(bar);
    const pager = document.createElement("div");
    pager.className = "pager";
    wrap.after(pager);

    function render() {
      const pages = Math.max(1, Math.ceil(shown.length / PAGE));
      page = Math.min(page, pages - 1);
      const from = page * PAGE, to = from + PAGE;
      const visible = new Set(shown.slice(from, to));
      body.forEach((r) => { r.hidden = !visible.has(r); });
      count.textContent = shown.length === body.length ? `${body.length}` : `${shown.length} / ${body.length}`;
      pager.replaceChildren();
      if (pages > 1) {
        const btn = (label, p, disabled) => {
          const b = document.createElement("button");
          b.type = "button";
          b.className = "secondary";
          b.textContent = label;
          b.disabled = disabled;
          b.addEventListener("click", () => { page = p; render(); });
          return b;
        };
        const info = document.createElement("span");
        info.className = "muted";
        info.textContent = `${from + 1}–${Math.min(to, shown.length)} / ${shown.length}`;
        pager.append(btn("«", 0, page === 0), btn("‹", page - 1, page === 0), info,
          btn("›", page + 1, page >= pages - 1), btn("»", pages - 1, page >= pages - 1));
      }
    }

    filter.addEventListener("input", () => {
      const terms = filter.value.toLowerCase().split(/\s+/).filter(Boolean);
      shown = body.filter((r) => {
        const text = r.textContent.toLowerCase();
        return terms.every((t) => text.includes(t));
      });
      page = 0;
      render();
    });

    Array.from(head.cells).forEach((th, col) => {
      if (!th.textContent.trim()) return; // action columns
      th.classList.add("sortable");
      th.tabIndex = 0;
      const sort = () => {
        const dir = th.getAttribute("aria-sort") === "ascending" ? -1 : 1;
        Array.from(head.cells).forEach((c) => c.removeAttribute("aria-sort"));
        th.setAttribute("aria-sort", dir === 1 ? "ascending" : "descending");
        const keyed = body.map((r, i) => ({ r, i, k: r.cells[col] ? key(r.cells[col]) : { s: "" } }));
        keyed.sort((a, b) => cmp(a.k, b.k) * dir || a.i - b.i);
        keyed.forEach(({ r }) => tbody.appendChild(r));
        const order = new Map(keyed.map(({ r }, i) => [r, i]));
        body.sort((a, b) => order.get(a) - order.get(b));
        shown.sort((a, b) => order.get(a) - order.get(b));
        page = 0;
        render();
      };
      th.addEventListener("click", sort);
      th.addEventListener("keydown", (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); sort(); } });
    });
    render();
  }

  document.addEventListener("DOMContentLoaded", () => {
    document.querySelectorAll(".tablewrap table:not(.form):not([data-plain])").forEach(enhance);
  });
})();

// Live line state in the phone book (data-presence-poll="<json url>", cells data-presence="<extension>")
(function () {
  document.addEventListener("DOMContentLoaded", () => {
    const table = document.querySelector("[data-presence-poll]");
    if (!table) return;
    const poll = async () => {
      if (document.hidden) return;
      try {
        const r = await fetch(table.dataset.presencePoll, { credentials: "same-origin" });
        if (!r.ok) return;
        const states = await r.json();
        table.querySelectorAll("[data-presence]").forEach((td) => {
          const s = states[td.dataset.presence];
          if (!s) return;
          const span = td.querySelector(".presence") || td.appendChild(document.createElement("span"));
          span.className = "presence presence-" + s;
          span.textContent = pbxPresence(s);
        });
      } catch (_) { /* keep the last state */ }
    };
    setInterval(poll, 5000);
    document.addEventListener("visibilitychange", poll);
  });
})();

// Print buttons (data-print)
document.addEventListener("DOMContentLoaded", () => {
  document.querySelectorAll("[data-print]").forEach((b) => b.addEventListener("click", () => window.print()));
});

// Call companion (companion.rs): current calls with actions, pickup offers, quick toggles.
// Polls /me/live; built with DOM methods only (no innerHTML with data).
(function () {
  document.addEventListener("DOMContentLoaded", () => {
    const box = document.getElementById("companion");
    if (!box) return;
    let T = {};
    try { T = JSON.parse(box.dataset.t || "{}"); } catch (_) {}
    const t = (k) => T[k] || k;
    const fab = box.querySelector(".cmp-fab");
    const panel = box.querySelector(".cmp-panel");
    const badge = box.querySelector(".cmp-badge");
    const callsEl = box.querySelector(".cmp-calls");
    const offersEl = box.querySelector(".cmp-offers");
    const togglesEl = box.querySelector(".cmp-toggles");
    const OPEN_KEY = "pbx-companion-open";
    let lastJson = "";
    let seenRinging = new Set();
    let closedFor = new Set();      // ringing calls the user closed the panel for
    const ctxCache = new Map();     // number -> context
    let transferFor = null;         // call id with the transfer input open
    let noteEdit = null;            // open note form: {number, id (null = new), draft, focus}
    let stopped = false;

    const el = (tag, attrs, ...kids) => {
      const e = document.createElement(tag);
      for (const [k, v] of Object.entries(attrs || {})) {
        if (k === "class") e.className = v;
        else if (k === "text") e.textContent = v;
        else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
        else if (v !== false && v != null) e.setAttribute(k, v === true ? "" : v);
      }
      for (const c of kids) if (c) e.append(c);
      return e;
    };
    const fmt = (s) => `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;

    function setOpen(open) {
      panel.hidden = !open;
      fab.setAttribute("aria-expanded", String(open));
      box.classList.toggle("open", open);
      try { localStorage.setItem(OPEN_KEY, open ? "1" : "0"); } catch (_) {}
    }
    fab.addEventListener("click", () => setOpen(panel.hidden));
    box.querySelector(".cmp-close").addEventListener("click", () => {
      document.querySelectorAll("[data-ringing-id]").forEach((c) => closedFor.add(c.dataset.ringingId));
      setOpen(false);
    });

    async function act(url, data) {
      try {
        const r = await fetch(url, { method: "POST", body: new URLSearchParams(data || {}), credentials: "same-origin" });
        const j = r.ok ? await r.json() : { ok: false, msg: "" };
        if (!j.ok) pbxToast(t("Action failed.") + (j.msg ? ` (${j.msg})` : ""), false);
        return j.ok;
      } catch (_) { pbxToast(t("Action failed."), false); return false; }
      finally { lastJson = ""; setTimeout(poll, 400); }
    }

    async function context(c) {
      if (!c.number || c.number.length < 5) return null;
      if (ctxCache.has(c.number)) return ctxCache.get(c.number);
      ctxCache.set(c.number, null);
      try {
        const r = await fetch(`/me/live/context?number=${encodeURIComponent(c.number)}`, { credentials: "same-origin" });
        if (r.ok) { ctxCache.set(c.number, await r.json()); lastJson = ""; render(window.__cmpLast); }
      } catch (_) {}
      return null;
    }

    // context of a number again (after blocking it or changing a note)
    function refreshContext(number) {
      ctxCache.delete(number);
      lastJson = "";
      render(window.__cmpLast);
    }

    // a form button keeps the focus, which pauses rendering (see render): let go of it first
    function closeNoteForm(number) {
      noteEdit = null;
      if (box.contains(document.activeElement)) document.activeElement.blur();
      if (number) refreshContext(number); else { lastJson = ""; render(window.__cmpLast); }
    }

    function noteForm(c, x, n) {
      const ta = el("textarea", { rows: "3", maxlength: "1000", "aria-label": t("Note"), oninput: () => { noteEdit.draft = ta.value; } });
      ta.value = noteEdit.draft ?? (n ? n.body : "");
      const sh = x.shared_notes ? el("input", { type: "checkbox", checked: n ? n.shared : false }) : null;
      const form = el("form", { class: "cmp-note-form", onsubmit: async (e) => {
        e.preventDefault();
        if (!ta.value.trim()) return;
        const data = { body: ta.value, shared: sh && sh.checked ? "1" : "" };
        if (!n) data.number = c.number;
        if (await act(n ? `/me/live/notes/${n.id}` : "/me/live/notes", data)) closeNoteForm(c.number);
      } }, ta,
      sh ? el("label", { class: "cmp-switch" }, sh, el("span", { text: t("Visible to all colleagues") })) : null,
      el("div", { class: "cmp-actions" },
        el("button", { class: "primary", text: t("Save") }),
        el("button", { type: "button", text: t("Cancel"), onclick: () => closeNoteForm(null) })));
      if (noteEdit.focus) { noteEdit.focus = false; setTimeout(() => ta.focus(), 0); }
      return form;
    }

    // notes on the number: own ones editable, colleagues' shared ones with the author
    function notesBlock(c, x) {
      const d = el("div", { class: "cmp-notes" }, el("div", { class: "cmp-ctx-h", text: t("Notes") }));
      const editing = (id) => noteEdit && noteEdit.number === c.number && noteEdit.id === id;
      const open = (id) => { noteEdit = { number: c.number, id, draft: null, focus: true }; lastJson = ""; render(window.__cmpLast); };
      for (const n of x.notes) {
        if (editing(n.id)) { d.append(noteForm(c, x, n)); continue; }
        const meta = [n.mine ? "" : n.author, n.at, n.shared ? t("Visible to all colleagues") : ""].filter(Boolean).join(" · ");
        const item = el("div", { class: "cmp-note" }, el("p", { text: n.body }), el("small", { text: meta }));
        if (n.mine) {
          item.append(el("div", { class: "cmp-actions" },
            el("button", { type: "button", text: t("Edit"), onclick: () => open(n.id) }),
            el("button", { type: "button", class: "danger", text: t("Delete"), onclick: async () => {
              if (confirm(t("Delete this note?")) && await act(`/me/live/notes/${n.id}/delete`)) refreshContext(c.number);
            } })));
        }
        d.append(item);
      }
      if (editing(null)) d.append(noteForm(c, x, null));
      else d.append(el("button", { type: "button", class: "cmp-link", text: `+ ${t("Add note")}`, onclick: () => open(null) }));
      return d;
    }

    function contextBlock(c) {
      const x = ctxCache.get(c.number);
      if (x === undefined) { context(c); return null; }
      if (!x) return null;
      const d = el("div", { class: "cmp-ctx" });
      if (x.calls.length) {
        const ul = el("ul");
        x.calls.forEach((p) => ul.append(el("li", { text: `${p.at} · ${p.incoming ? t("in") : t("out")} · ${p.answered ? `${t("answered")} ${fmt(p.secs)}` : t("missed")}` })));
        d.append(el("div", { class: "cmp-ctx-h", text: t("Last calls") }), ul);
      }
      if (x.voicemails > 0) d.append(el("a", { href: "/voicemail", text: `${x.voicemails} ${t("voicemails from this number")}` }));
      if (x.external) {
        const label = `${x.external.book}: ${x.external.name}`;
        d.append(x.external.url
          ? el("a", { href: x.external.url, target: "_blank", rel: "noopener", text: `${t("Open in")} ${label}` })
          : el("span", { class: "muted", text: label }));
      }
      if (x.contact) {
        d.append(el("a", { href: `/contacts/${x.contact.id}`, text: `${t("Open contact")}: ${x.contact.name}` }));
      } else {
        // prefill with the entry's own name (c.name may be "Name (Company)")
        const q = new URLSearchParams({ phone: x.e164 || c.number, name: (x.external && x.external.name) || c.name || "" });
        d.append(el("a", { href: `/contacts/new?${q}`, text: `+ ${t("Save as contact")}` }));
      }
      if (x.e164 && c.incoming) {
        if (x.blocked) d.append(el("a", { href: "/#blocklist", text: t("Blocked for you") }));
        else d.append(el("button", { type: "button", class: "cmp-link", text: t("Block this number for me"), onclick: async () => {
          if (!confirm(t("Block this number for you? Calls from it to you personally will hear busy; calls to your call groups still reach you."))) return;
          if (await act("/me/live/block", { number: c.number })) { pbxToast(t("Blocked for you"), true); refreshContext(c.number); }
        } }));
      }
      if (x.e164) d.append(notesBlock(c, x));
      return d.childNodes.length ? d : null;
    }

    function callCard(c) {
      const state = c.state === "ringing" ? t("Ringing") : c.state === "active" ? t("On the phone") : t("Calling");
      const card = el("div", { class: `cmp-call cmp-${c.state}` });
      if (c.state === "ringing") card.dataset.ringingId = c.id;
      card.append(el("div", { class: "cmp-state" }, el("span", { text: state }), el("span", { class: "cmp-time", text: fmt(c.secs) })));
      // own contact (saved during the call) unless the shown name already has it (group prefix)
      const ctx = ctxCache.get(c.number);
      const hit = ctx && (ctx.contact || ctx.external);
      // the phone system may send "Name (Company)" (PBX_CALLER_NAME_FORMAT): here the company gets its own line
      const norm = (s) => (s || "").replace(/[\x00-\x1f\s",\\]+/g, " ").trim().toLowerCase();
      let base = c.name || "";
      if (hit && hit.organization && base.endsWith(")")) base = base.replace(/\s*\([^()]*\)$/, "");
      const name = hit && !base.includes(hit.name) ? hit.name : base;
      card.append(el("div", { class: "cmp-who", text: name || c.number || "?" }));
      // company of a contact (not for a company's own entry, where it is the name)
      const org = hit && hit.organization && !norm(name).includes(norm(hit.organization)) ? hit.organization : "";
      if (org) card.append(el("div", { class: "cmp-org", text: org }));
      const sub = [name ? c.number : "", c.via ? `${t("via")} ${c.via}` : "", c.muted ? t("Muted") : ""].filter(Boolean).join(" · ");
      if (sub) card.append(el("div", { class: "cmp-sub", text: sub }));
      const btns = el("div", { class: "cmp-actions" });
      const b = (label, cls, fn, title) => btns.append(el("button", { type: "button", class: cls, text: label, title: title || false, onclick: fn }));
      const url = (op) => `/me/live/${encodeURIComponent(c.id)}/${op}`;
      if (c.state === "ringing") {
        b(t("Decline"), "", () => act(url("decline")), t("Decline: the call goes on as if you had not answered (e.g. to your mailbox); colleagues in a group keep ringing."));
        if (c.can_reject) b(t("Reject (busy)"), "danger", () => act(url("reject")), t("Reject: the caller hears busy."));
      } else {
        b(t("Hang up"), "danger", () => act(url("hangup")));
        if (c.state === "active") {
          b(c.muted ? t("Unmute") : t("Mute"), c.muted ? "on" : "", () => act(url(c.muted ? "unmute" : "mute")));
          b(t("Transfer"), "", () => { transferFor = transferFor === c.id ? null : c.id; lastJson = ""; render(window.__cmpLast); });
        }
      }
      card.append(btns);
      if (transferFor === c.id && c.state === "active") {
        const inp = el("input", { type: "text", inputmode: "tel", placeholder: t("Number or extension"), "aria-label": t("Number or extension"), maxlength: "20" });
        const form = el("form", { class: "cmp-transfer", onsubmit: async (e) => {
          e.preventDefault();
          if (!inp.value.trim()) return;
          if (await act(url("transfer"), { to: inp.value.trim() })) transferFor = null;
        } }, inp, el("button", { class: "primary", text: t("Transfer now") }));
        card.append(form);
        setTimeout(() => inp.focus(), 0);
      }
      if (c.number) { const x = contextBlock(c); if (x) card.append(x); }
      return card;
    }

    function offerCard(o) {
      const who = o.caller || (o.internal ? t("internal call") : t("external call"));
      return el("div", { class: "cmp-offer" },
        el("div", {}, el("span", { class: "muted", text: `${t("Ringing at")} ${o.who} (${o.ext}) · ${fmt(o.secs)}` }), el("div", { text: who })),
        el("button", { type: "button", text: t("Pick up"), onclick: async () => {
          if (await act(`/me/live/${encodeURIComponent(o.id)}/pickup`, { ext: o.ext })) pbxToast(t("Pick up at your phone now."), true);
        } }));
    }

    function toggles(v) {
      const d = el("div");
      const sw = (label, on, fn) => {
        const i = el("input", { type: "checkbox", checked: on, onchange: () => fn(i.checked) });
        return el("label", { class: "cmp-switch" }, i, el("span", { text: label }));
      };
      d.append(sw(t("Do not disturb"), v.dnd, (on) => act("/me/live/dnd", { on: on ? "1" : "0" })));
      if (v.forward) d.append(el("a", { class: "cmp-fwd", href: "/#forwarding", text: `${t("Forwarded to")} ${v.forward}` }));
      if (v.groups.length) {
        d.append(el("div", { class: "cmp-ctx-h", text: t("My call groups") }));
        v.groups.forEach((g) => d.append(sw(`${g.name} (${g.number})`, g.on, (on) => act(`/me/live/group/${g.id}`, { on: on ? "1" : "0" }))));
      }
      if ("Notification" in window && Notification.permission === "default") {
        d.append(el("button", { type: "button", class: "cmp-notify", text: t("Enable notifications"), onclick: () => Notification.requestPermission().then(() => { lastJson = ""; render(window.__cmpLast); }) }));
      }
      return d;
    }

    function notify(v) {
      const ringing = v.calls.filter((c) => c.state === "ringing" && c.incoming);
      for (const c of ringing) {
        if (seenRinging.has(c.id)) continue;
        seenRinging.add(c.id);
        if (!closedFor.has(c.id) && panel.hidden) setOpen(true);
        if ("Notification" in window && Notification.permission === "granted" && (document.hidden || !document.hasFocus())) {
          try {
            const n = new Notification(t("Incoming call"), { body: [c.name || c.number, c.name ? c.number : "", c.via ? `${t("via")} ${c.via}` : ""].filter(Boolean).join("\n"), tag: `call-${c.id}` });
            n.onclick = () => { window.focus(); setOpen(true); n.close(); };
          } catch (_) {}
        }
      }
      const ids = new Set(v.calls.map((c) => c.id));
      seenRinging = new Set([...seenRinging].filter((id) => ids.has(id)));
      closedFor = new Set([...closedFor].filter((id) => ids.has(id)));
    }

    function render(v) {
      if (!v) return;
      const j = JSON.stringify(v) + transferFor + JSON.stringify([...ctxCache.keys()]);
      const a = document.activeElement;
      const typing = box.contains(a) && ((a.tagName === "INPUT" && a.type === "text") || a.tagName === "TEXTAREA" || a.closest(".cmp-note-form"));
      // timers tick every poll; keep the transfer input and the note form while typing
      if (j === lastJson || typing) return;
      lastJson = j;
      const n = v.calls.length + v.offers.length;
      badge.hidden = n === 0;
      badge.textContent = String(n);
      box.classList.toggle("ringing", v.calls.some((c) => c.state === "ringing"));
      callsEl.replaceChildren(...(v.offline ? [el("p", { class: "muted", text: t("Phone system not reachable") })]
        : v.calls.length ? v.calls.map(callCard) : [el("p", { class: "muted", text: t("No calls") })]));
      offersEl.replaceChildren(...v.offers.map(offerCard));
      togglesEl.replaceChildren(toggles(v));
    }

    async function poll() {
      if (stopped) return;
      try {
        const r = await fetch("/me/live", { credentials: "same-origin", headers: { Accept: "application/json" } });
        const ct = r.headers.get("content-type") || "";
        if (!r.ok || !ct.includes("json")) { if (r.status === 401 || r.status === 403 || r.redirected) stopped = true; return; }
        const v = await r.json();
        window.__cmpLast = v;
        box.hidden = false;
        if (window.__cmpDevices) window.__cmpDevices(v.devices);
        // DND changed (widget, phone *761/*760, other tab): header chip follows; the start page's
        // DND card is server-rendered, so that page reloads
        const chip = document.querySelector("[data-dnd-chip]");
        if (chip && chip.hidden === v.dnd) {
          chip.hidden = !v.dnd;
          if (document.getElementById("dnd")) location.reload();
        }
        notify(v);
        render(v);
      } catch (_) {}
    }

    // Dial: suggestions from colleagues, groups and own contacts; the own phone rings first (/call)
    const dial = box.querySelector(".cmp-dial");
    if (dial) {
      const inp = dial.querySelector("input");
      const dl = dial.querySelector("datalist");
      let hits = [], timer = null;
      inp.addEventListener("input", () => {
        clearTimeout(timer);
        const q = inp.value.trim();
        if (q.length < 2 || /^[0-9+*]+$/.test(q) && q.length > 6) { dl.replaceChildren(); return; }
        timer = setTimeout(async () => {
          try {
            const r = await fetch(`/me/live/search?q=${encodeURIComponent(q)}`, { credentials: "same-origin" });
            if (!r.ok) return;
            hits = await r.json();
            dl.replaceChildren(...hits.map((h) => el("option", { value: h.number, label: h.label, text: h.label })));
          } catch (_) {}
        }, 200);
      });
      // "call from": all devices or one; remembered per browser
      const sel = dial.querySelector("select[name=device]");
      const DEV_KEY = "pbx-dial-device";
      let devJson = "";
      window.__cmpDevices = (devs) => {
        const j = JSON.stringify(devs || []);
        if (j === devJson) return;
        devJson = j;
        let want = sel.value;
        try { want = localStorage.getItem(DEV_KEY) || want; } catch (_) {}
        sel.replaceChildren(sel.options[0], ...(devs || []).map((d) => el("option", { value: String(d.id), text: d.label })));
        sel.value = [...sel.options].some((o) => o.value === want) ? want : "all";
        sel.closest(".cmp-callrow").classList.toggle("single", (devs || []).length < 2);
      };
      sel.addEventListener("change", () => { try { localStorage.setItem(DEV_KEY, sel.value); } catch (_) {} });

      // fill the dial field from anywhere (phone book popup, call icons next to numbers, ?dial=)
      window.__cmpDial = (num) => {
        setOpen(true);
        inp.value = num;
        inp.focus();
      };

      // phone book popup
      const dlg = document.querySelector("dialog.cmp-book");
      const search = dlg.querySelector("input[type=search]");
      const list = dlg.querySelector(".cmp-book-list");
      let book = null;
      let presence = {};
      async function loadPresence() {
        try {
          const r = await fetch("/phonebook/status", { credentials: "same-origin" });
          if (r.ok) { presence = await r.json(); if (dlg.open) renderBook(); }
        } catch (_) {}
      }
      let presTimer = null;
      dlg.addEventListener("close", () => { clearInterval(presTimer); presTimer = null; });
      const KIND = { colleague: t("Colleagues"), group: t("Call groups"), service: t("Services"), contact: t("My contacts") };
      function renderBook() {
        const q = search.value.trim().toLowerCase();
        const qd = q.replace(/[^0-9]/g, "");
        const rows = (book || []).filter((b) => !q || b.label.toLowerCase().includes(q) || b.sub.toLowerCase().includes(q) || (qd.length > 1 && b.number.replace(/[^0-9]/g, "").includes(qd)));
        const out = [];
        let last = "";
        for (const b of rows.slice(0, 300)) {
          if (b.kind !== last) { out.push(el("li", { class: "cmp-book-h", text: KIND[b.kind] || b.kind })); last = b.kind; }
          const st = b.kind === "colleague" ? presence[b.number] : null;
          out.push(el("li", {}, el("button", { type: "button", onclick: () => { dlg.close(); window.__cmpDial(b.number); } },
            el("span", { class: "cmp-book-text" },
              el("span", { class: "cmp-book-name", text: b.label }),
              el("span", { class: "cmp-book-sub", text: [b.sub, b.number].filter(Boolean).join(" · ") })),
            st ? el("span", { class: `presence presence-${st}`, text: pbxPresence(st) }) : null)));
        }
        list.replaceChildren(...(out.length ? out : [el("li", { class: "muted", text: t("Nothing found") })]));
      }
      search.addEventListener("input", renderBook);
      dlg.querySelector("[data-close]").addEventListener("click", () => dlg.close());
      dlg.addEventListener("click", (e) => { if (e.target === dlg) dlg.close(); });
      dial.querySelector(".cmp-bookbtn").addEventListener("click", async () => {
        search.value = /[a-z]/i.test(inp.value) ? inp.value : "";
        dlg.showModal();
        search.focus();
        loadPresence();
        if (!presTimer) presTimer = setInterval(loadPresence, 5000);
        if (!book) {
          try {
            const r = await fetch("/me/live/book", { credentials: "same-origin" });
            book = r.ok ? await r.json() : [];
          } catch (_) { book = []; }
        }
        renderBook();
      });

      dial.addEventListener("submit", async (e) => {
        e.preventDefault();
        let to = inp.value.replace(/[\s\-\/()]/g, "");
        if (!/^[0-9+*]{1,20}$/.test(to)) {
          const h = hits.find((x) => x.label.toLowerCase().includes(inp.value.trim().toLowerCase()));
          if (!h) { pbxToast(pbxMsg("msgCallFailed", t("Action failed.")), false); return; }
          to = h.number;
        }
        try {
          const r = await fetch("/call", { method: "POST", body: new URLSearchParams({ to, device: sel.value }), credentials: "same-origin" });
          const j = r.ok ? await r.json() : { ok: false };
          pbxToast(pbxCallMsg(j), j.ok);
          if (j.ok) { inp.value = ""; dl.replaceChildren(); }
        } catch (_) { pbxToast(pbxMsg("msgCallFailed"), false); }
      });
    }

    let open = false;
    try { open = localStorage.getItem(OPEN_KEY) === "1"; } catch (_) {}
    // phones: the open panel would cover most of the page after every navigation
    if (window.matchMedia("(max-width: 600px)").matches) open = false;
    setOpen(open);
    document.addEventListener("click", (e) => {
      const a = e.target.closest("[data-dial]");
      if (!a || !window.__cmpDial) return;
      e.preventDefault();
      window.__cmpDial(a.dataset.dial);
    });
    const qs = new URLSearchParams(location.search);
    if (qs.get("dial") && window.__cmpDial) {
      window.__cmpDial(qs.get("dial").replace(/[^0-9+*]/g, "").slice(0, 20));
      qs.delete("dial");
      history.replaceState(null, "", location.pathname + (qs.toString() ? `?${qs}` : "") + location.hash);
    }
    poll();
    (function loop() { setTimeout(() => { poll().finally(loop); }, document.hidden ? 6000 : 2500); })();
  });
})();

// Script apps: test run (POST /apps/<id>/test) and its output
document.addEventListener("click", async (e) => {
  const b = e.target.closest("[data-script-test]");
  if (!b) return;
  const out = b.parentElement.querySelector(".script-out");
  b.disabled = true;
  out.hidden = false;
  out.textContent = "…";
  try {
    const r = await fetch(b.dataset.scriptTest, { method: "POST", credentials: "same-origin" });
    const j = await r.json();
    const lines = [];
    j.says.forEach((s) => lines.push("🔊 " + s));
    j.log.forEach((l) => lines.push("· " + l));
    if (!j.ok && j.error) lines.push("✗ " + j.error);
    out.textContent = lines.join("\n") || "–";
    out.classList.toggle("err", !j.ok);
  } catch (err) {
    out.textContent = "✗ " + err;
  } finally {
    b.disabled = false;
  }
});
