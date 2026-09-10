// The page: a store browser with a time axis. The container is a StructFS
// store; every panel is a read of one of its paths at the chosen instant,
// and the panels themselves come from the store's own `meta` lens rather
// than from a list kept here. The timeline is the container's traffic with
// the host — its reads and writes under `/iso` — beside its syscalls.
// Everything the page knows comes from the worker.
//
// Three ways in. With a tape, the run is fixed and every instant is a seek.
// Live, the container runs against this page's clock and entropy; "play"
// advances the frontier, the slider views anything behind it, and a request
// typed into the edge box goes to a listener inside the container. With a
// snapshot, the live run starts from a container somebody already booted.

const worker = new Worker("./worker.js", { type: "module" });
const $ = (id) => document.getElementById(id);

const LIVE_TICK = 4000000; // instructions per advance while playing live

let live = false;
let origin = 0;
let total = 0;
let frontier = 0;
let finished = null;
let timeline = []; // { at, pid, tid, name } per syscall
let trace = []; // the strace line for each
let exchanges = []; // { op, path, syscall, bytes, text, error } per host exchange
let events = []; // the two merged, as the timeline shows them
let playing = null;
let current = 0;
let busy = false;
let queued = null;
let nextRequest = 0;
let responses = 0;
let disassembled = 0;
let meta = null; // the store's meta lens
let interface_ = {}; // the manifest's description of each path
let context = { pid: null, tid: null, path: "" }; // what the panels look at
let values = {}; // the last state's values, by concrete path
let pinned = null; // { at, values } when pinned
let nextRead = 0;
const reads = new Map(); // id -> { resolve, reject }

function seek(at) {
  at = Math.max(origin, Math.min(live ? frontier : total, Math.round(at)));
  if (busy) {
    queued = { seek: at };
    return;
  }
  busy = true;
  worker.postMessage({ type: "seek", at, context });
}

function advance(by) {
  if (busy) {
    queued = { advance: by };
    return;
  }
  busy = true;
  worker.postMessage({ type: "advance", by });
}

function drain() {
  if (queued === null) return false;
  const next = queued;
  queued = null;
  if (next.seek !== undefined) seek(next.seek);
  else advance(next.advance);
  return true;
}

/// One path of the store, read at the instant in view.
function read(path) {
  return new Promise((resolve, reject) => {
    const id = nextRead++;
    reads.set(id, { resolve, reject });
    worker.postMessage({ type: "read", id, path });
  });
}

const mb = (n) => (n / 1048576).toFixed(1) + " MB";

function status(extra = "") {
  if (live) {
    const parts = [
      `live${origin ? `, from a snapshot at ${origin.toLocaleString()}` : ""}: ${(frontier - origin).toLocaleString()} instructions so far`,
      `${timeline.length} syscalls, ${exchanges.length} exchanges`,
      extra,
      finished !== null ? `exited ${finished}` : "",
    ].filter(Boolean);
    $("status").textContent = parts.join(", ");
  }
}

worker.onmessage = (event) => {
  const message = event.data;
  if (message.type === "error") {
    $("status").textContent = message.message;
    busy = false;
    stop();
    return;
  }
  if (message.type === "loaded") {
    live = message.live;
    origin = message.origin;
    total = message.total;
    frontier = message.frontier;
    finished = message.finished;
    timeline = message.timeline;
    trace = message.trace;
    exchanges = message.exchanges;
    meta = message.meta;
    interface_ = message.interface;
    $("slider").min = origin;
    $("slider").max = live ? frontier : total;
    $("controls").classList.add("hidden");
    buildPanels();
    if (live) {
      $("port").value = message.published[0] ?? 8080;
      status(`${message.checkpoints.length} checkpoint holding ${mb(message.held)}${message.listening.length ? `, listening on ${message.listening.join(", ")}` : `, publishing ${message.published.join(", ") || "no ports"}`}, loaded in ${(message.loading / 1000).toFixed(1)} s${message.inflated ? ` (${(message.inflated / 1000).toFixed(1)} s inflating)` : ""} — press play, then send a request`);
    } else {
      $("status").textContent = `${total.toLocaleString()} instructions, ${timeline.length} syscalls, ${exchanges.length} exchanges, ${message.checkpoints.length} checkpoints holding ${mb(message.held)} (${mb(message.naive)} as full copies, diffed in ${Math.round(message.diffing)} ms), ${message.bytecode ? "bytecode" : "interpreter"}, loaded in ${(message.loading / 1000).toFixed(1)} s`;
    }
    rebuildEvents();
    renderEvents(true);
    seek(origin);
    return;
  }
  if (message.type === "progress") {
    busy = false;
    frontier = message.frontier;
    finished = message.finished;
    timeline.push(...message.timeline);
    trace.push(...message.trace);
    exchanges.push(...message.exchanges);
    $("slider").max = frontier;
    status(`${message.checkpoints} checkpoints holding ${mb(message.held)}${message.listening.length ? `, listening on ${message.listening.join(", ")}` : ""}${message.idle && finished === null ? ", idle" : ""}`);
    rebuildEvents();
    renderEvents(true);
    if (finished !== null) stop();
    if (!drain()) seek(frontier);
    return;
  }
  if (message.type === "state") {
    busy = false;
    render(message);
    if (!drain() && playing) {
      if (live) {
        if (finished === null) advance(LIVE_TICK);
        else stop();
      } else seek(current + playing);
    }
    return;
  }
  if (message.type === "value") {
    const waiting = reads.get(message.id);
    reads.delete(message.id);
    if (!waiting) return;
    if (message.error !== undefined) waiting.reject(message.error);
    else waiting.resolve(message.value);
    return;
  }
  if (message.type === "response") {
    const box = $(`exchange-${message.id}`);
    if (!box) return;
    responses++;
    if (message.error) box.querySelector(".meta").textContent += ` — ${message.error}`;
    else {
      const meta_ = box.querySelector(".meta");
      meta_.textContent = `#${message.id} sent at ${message.sent.toLocaleString()}, answered at ${message.answered.toLocaleString()} — `;
      const link = document.createElement("a");
      link.textContent = `seek to the answer`;
      link.onclick = () => {
        stop();
        seek(message.answered);
      };
      meta_.appendChild(link);
      const body = document.createElement("pre");
      body.textContent = message.response;
      box.appendChild(body);
    }
  }
};

// ---- the timeline ------------------------------------------------------------
//
// Two kinds of event on one axis of retired instructions: the syscalls the
// kernel stamped, and the exchanges with the host — placed at the syscall
// each was made in. A run is a million events long before it is
// interesting, so the list holds rows only for a window around the present
// and rebuilds the window when the present leaves it.

const WINDOW = 150;
let rendered = { from: -1, to: -1, filter: "" };

function describeExchange(e) {
  const arrow = e.op === "read" ? "→" : "←";
  let what;
  if (e.error) what = `✗ ${e.error}`;
  else if (e.op === "read" && e.text === null && e.bytes === 0) what = "nothing there";
  else if (e.text !== null && e.text.length === e.bytes) what = JSON.stringify(e.text);
  else if (e.text !== null) what = `${e.bytes} bytes ${JSON.stringify(e.text)}…`;
  else what = `${e.bytes} bytes`;
  return `${e.op.padEnd(5)} /${e.path} ${arrow} ${what}`;
}

/// Whether an exchange is a poll that found nothing: the kernel, idle,
/// asking the host for network events or a shutdown it has not been given.
function emptyPoll(e) {
  return e.op === "read" && e.text === null && e.bytes === 0 && !e.error;
}

function rebuildEvents() {
  const filter = $("filter").value;
  const raw = filter === "raw";
  events = [];
  // An exchange made during a syscall comes before the syscall's own row:
  // the row is what the call returned, written as it returned.
  if (filter !== "exchange") for (let i = 0; i < timeline.length; i++) events.push({ at: timeline[i].at, kind: "syscall", index: i, order: 1, count: 1 });
  if (filter !== "syscall") {
    // Folded: repeats of one path at one stamp — an idle container reads
    // the clock at every wake — become one row saying how many; polls that
    // found nothing are left out. "Everything" shows each as it was.
    let stamp = -1;
    let groups = new Map(); // op|path -> the row folding it, within one stamp
    for (let i = 0; i < exchanges.length; i++) {
      const e = exchanges[i];
      if (!raw && emptyPoll(e)) continue;
      if (e.syscall !== stamp) {
        stamp = e.syscall;
        groups = new Map();
      }
      const key = `${e.op}|${e.path}`;
      const folded = !raw && !e.error ? groups.get(key) : undefined;
      if (folded) {
        folded.count++;
        continue;
      }
      const row = { at: timeline[e.syscall]?.at ?? frontier, kind: "exchange", index: i, order: 0, count: 1 };
      events.push(row);
      if (!raw && !e.error) groups.set(key, row);
    }
  }
  events.sort((a, b) => a.at - b.at || a.order - b.order || a.index - b.index);
}

/// The index of the last event at or before `current`.
function position() {
  let low = 0;
  let high = events.length;
  while (low < high) {
    const middle = (low + high) >> 1;
    if (events[middle].at <= current) low = middle + 1;
    else high = middle;
  }
  return low - 1;
}

function eventText(event) {
  if (event.kind === "syscall") return trace[event.index] ?? timeline[event.index].name;
  return describeExchange(exchanges[event.index]) + (event.count > 1 ? `  ×${event.count}` : "");
}

function renderEvents(force = false) {
  const list = $("events");
  const filter = $("filter").value;
  const now = position();
  const from = Math.max(0, now - WINDOW);
  const to = Math.min(events.length, now + WINDOW);
  if (force || from !== rendered.from || to !== rendered.to || filter !== rendered.filter) {
    rendered = { from, to, filter };
    list.innerHTML = "";
    if (from > 0) {
      const elided = document.createElement("div");
      elided.className = "elided";
      elided.textContent = `… ${from.toLocaleString()} earlier`;
      list.appendChild(elided);
    }
    for (let index = from; index < to; index++) {
      const event = events[index];
      const row = document.createElement("div");
      row.className = `event ${event.kind}${event.kind === "exchange" && exchanges[event.index].error ? " error" : ""}`;
      row.dataset.at = event.at;
      row.dataset.index = index;
      row.textContent = `${event.at.toLocaleString().padStart(14)}  ${eventText(event)}`;
      row.onclick = () => {
        stop();
        seek(event.at);
      };
      list.appendChild(row);
    }
    if (to < events.length) {
      const elided = document.createElement("div");
      elided.className = "elided";
      elided.textContent = `… ${(events.length - to).toLocaleString()} later`;
      list.appendChild(elided);
    }
  }
  for (const row of list.querySelectorAll(".event")) {
    const index = Number(row.dataset.index);
    row.classList.toggle("past", index <= now);
    row.classList.toggle("now", index === now);
  }
  const marked = list.querySelector(".event.now");
  if (marked) marked.scrollIntoView({ block: "nearest" });
}

$("filter").onchange = () => {
  rebuildEvents();
  renderEvents(true);
};

// ---- the panels --------------------------------------------------------------
//
// One panel per readable pattern of the meta lens. A few patterns have a
// renderer that knows their shape; the rest are shown as the JSON they
// are. Which panels stand in the open and which fold under "the machine"
// is the one opinion the page keeps.

const OPEN = ["processes", "processes/{pid}/files/{path}", "processes/{pid}/descriptors", "net"];
const MACHINE_ORDER = [
  "processes/{pid}/threads/{tid}/registers",
  "processes/{pid}/threads/{tid}/disassembly",
  "processes/{pid}/memory/{address}/{length}",
  "processes/{pid}/maps",
  "processes/{pid}/mapped",
  "statistics",
  "cache",
  "caches",
  "layout",
];
const panels = new Map(); // pattern -> { element, body, raw, diff, pathLabel }

function escape(text) {
  return String(text).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

function makePanel(pattern, title, into) {
  const element = document.createElement("section");
  element.className = "panel";
  element.dataset.pattern = pattern;
  element.innerHTML = `<h2><span class="path">${escape(title)}</span><span class="tools"><button class="rawtoggle" title="${escape(interface_[pattern]?.read ?? "")}">raw</button></span></h2><div class="body"></div><pre class="raw"></pre><div class="diff"></div>`;
  element.querySelector(".rawtoggle").onclick = (event) => {
    element.classList.toggle("raw");
    event.target.classList.toggle("on");
  };
  into.appendChild(element);
  const panel = {
    element,
    body: element.querySelector(".body"),
    raw: element.querySelector(".raw"),
    diff: element.querySelector(".diff"),
    pathLabel: element.querySelector(".path"),
  };
  panels.set(pattern, panel);
  return panel;
}

function buildPanels() {
  panels.clear();
  $("panels").innerHTML = "";
  $("machine").querySelector(".grid").innerHTML = "";
  const readable = Object.entries(meta?.paths ?? {})
    .filter(([pattern, lens]) => lens.readable && pattern !== "meta" && !pattern.startsWith("meta/"))
    .map(([pattern]) => pattern);
  for (const pattern of OPEN) if (readable.includes(pattern)) makePanel(pattern, pattern, $("panels"));
  // The host's side of the boundary, which is not a path of the
  // container's store but is what the page is to it: its edge, its console.
  makeEdgePanel();
  makeConsolePanel();
  const rest = readable.filter((pattern) => !OPEN.includes(pattern));
  rest.sort((a, b) => (MACHINE_ORDER.indexOf(a) + 1 || 99) - (MACHINE_ORDER.indexOf(b) + 1 || 99) || a.localeCompare(b));
  for (const pattern of rest) makePanel(pattern, pattern, $("machine").querySelector(".grid"));
  // The path bar's suggestions: every pattern, filled in as far as known.
  $("paths").innerHTML = readable.map((pattern) => `<option value="${escape(pattern)}">`).join("");
}

function makeEdgePanel() {
  const element = document.createElement("section");
  element.className = "panel edge";
  element.id = "edge";
  element.innerHTML = `<h2><span class="path">/iso/net</span> — this page, as the container's peer</h2>
    <form id="send-form">
      <label>port <input id="port" type="number" value="8080" style="width:6em"></label>
      <input id="request" type="text" value="GET / HTTP/1.0\\r\\n\\r\\n">
      <button id="send" type="submit">send</button>
    </form>
    <div id="responses"></div>`;
  $("panels").appendChild(element);
  $("send-form").onsubmit = (event) => {
    event.preventDefault();
    send(Number($("port").value), $("request").value.replace(/\\n/g, "\n").replace(/\\r/g, "\r"));
  };
}

function makeConsolePanel() {
  const element = document.createElement("section");
  element.className = "panel";
  element.innerHTML = `<h2><span class="path">/iso/console</span> — what the container wrote</h2><pre id="stdout"></pre><pre id="stderr" style="opacity:.7"></pre>`;
  $("panels").appendChild(element);
}

/// Any value, as a tree of keys and values.
function renderJson(value, depth = 0) {
  if (value === null || typeof value !== "object") return `<span class="value">${escape(JSON.stringify(value))}</span>`;
  if (Array.isArray(value)) {
    if (!value.length) return "[]";
    if (value.every((v) => v === null || typeof v !== "object")) return escape(JSON.stringify(value));
    return `<div class="kv">${value.map((v, i) => `<div><span class="key">${i}</span>${renderJson(v, depth + 1)}</div>`).join("")}</div>`;
  }
  const entries = Object.entries(value);
  if (!entries.length) return "{}";
  return `<div class="kv">${entries.map(([k, v]) => `<div><span class="key">${escape(k)}</span>${renderJson(v, depth + 1)}</div>`).join("")}</div>`;
}

/// 256 bytes from rsp as quadwords, two a row, little-endian read out.
function hexDump(memory) {
  if (!memory?.bytes) return "";
  const base = BigInt(memory.address);
  const hex = memory.bytes;
  const rows = [];
  for (let byte = 0; byte * 2 < hex.length; byte += 16) {
    const words = [];
    for (let w = 0; w < 2; w++) {
      const from = (byte + w * 8) * 2;
      if (from >= hex.length) break;
      words.push("0x" + hex.slice(from, from + 16).match(/../g).reverse().join("").padStart(16, "0"));
    }
    rows.push(`<div class="word"><span class="address">0x${(base + BigInt(byte)).toString(16)}</span>  ${words.join("  ")}</div>`);
  }
  return rows.join("");
}

/// Bytes as hex, sixteen a row with the printable characters beside.
function hexRows(hex) {
  const rows = [];
  for (let at = 0; at < hex.length; at += 32) {
    const chunk = hex.slice(at, at + 32).match(/../g);
    const printable = chunk.map((h) => {
      const byte = parseInt(h, 16);
      return byte >= 0x20 && byte < 0x7f ? String.fromCharCode(byte) : ".";
    });
    rows.push(`<div class="word"><span class="address">${(at / 2).toString(16).padStart(6, "0")}</span>  ${chunk.join(" ").padEnd(47)}  ${escape(printable.join(""))}</div>`);
  }
  return rows.join("");
}

function browse(path) {
  context.path = path.replace(/\/+/g, "/").replace(/^\//, "");
  seek(current);
}

function view(pid, tid = null) {
  context.pid = pid;
  context.tid = tid;
  seek(current);
}

/// The directory a file path is in, with a trailing slash stripped.
function parentOf(path) {
  const trimmed = path.replace(/\/+$/, "");
  const cut = trimmed.lastIndexOf("/");
  return cut <= 0 ? "" : trimmed.slice(0, cut);
}

const renderers = {
  processes(value, panel, state) {
    const s = values.statistics?.value;
    const running = s?.current;
    let html = s ? `<div style="opacity:.7;margin-bottom:4px">retired ${s.retired.toLocaleString()} · in bytecode ${s.accelerated.toLocaleString()} · blocks decoded ${s.decoded} · running pid ${running}</div>` : "";
    for (const p of value.processes) {
      const state_ = typeof p.state === "string" ? p.state : JSON.stringify(p.state);
      html += `<div class="process${p.pid === running ? " current" : ""}${p.pid === state.pid ? " viewing" : ""}" data-pid="${p.pid}" title="view this process's paths"><b>pid ${p.pid}</b> parent ${p.parent} · ${escape(state_)}${p.displaced ? ` · ${p.displaced} pages displaced` : ""}${p.pid === state.pid ? " · in view" : ""}` +
        p.threads.map((t) => `<div class="thread${t.tid === state.tid && p.pid === state.pid ? " viewing" : ""}" data-pid="${p.pid}" data-tid="${t.tid}">tid ${t.tid} @ ${t.rip} · ${escape(t.state)} · retired ${t.retired.toLocaleString()}</div>`).join("") +
        `</div>`;
    }
    panel.body.innerHTML = html;
    for (const box of panel.body.querySelectorAll(".process")) box.onclick = (event) => {
      const thread = event.target.closest(".thread");
      view(Number(box.dataset.pid), thread ? Number(thread.dataset.tid) : null);
    };
  },

  "processes/{pid}/files/{path}"(value, panel) {
    const path = value.path;
    const parts = path.split("/").filter(Boolean);
    let crumb = `<div class="crumb"><a data-path="">/</a>`;
    parts.forEach((part, i) => {
      crumb += `<a data-path="${escape(parts.slice(0, i + 1).join("/"))}">${escape(part)}</a><span class="sep">/</span>`;
    });
    crumb += `<span style="opacity:.6"> · cwd ${escape(value.cwd ?? "?")}</span></div>`;
    let html = crumb;
    if (value.kind === "directory") {
      html += `<table><tr><th>name</th><th>kind</th><th>size</th></tr>`;
      if (path !== "/") html += `<tr class="entry directory"><td><a data-path="${escape(parentOf(path))}">..</a></td><td></td><td></td></tr>`;
      for (const entry of value.entries) {
        const target = (path === "/" ? "" : path.replace(/^\//, "") + "/") + entry.name;
        html += `<tr class="entry ${entry.kind}"><td><a data-path="${escape(target)}">${escape(entry.name)}</a></td><td>${entry.kind}</td><td>${entry.kind === "file" ? entry.size.toLocaleString() : ""}</td></tr>`;
      }
      html += `</table>`;
      if (value.truncated) html += `<div class="elided">… and more: the listing is capped</div>`;
    } else if (value.kind === "file") {
      html += `<div class="filehead">file · ${value.size.toLocaleString()} bytes · mode ${value.mode}${value.truncated ? " · showing the beginning" : ""}</div>`;
      if (value.text !== undefined) html += `<pre>${escape(value.text)}</pre>`;
      else html += hexRows(value.hex);
    } else if (value.kind === "symlink") {
      const target = value.target.startsWith("/") ? value.target : parentOf(path) + "/" + value.target;
      html += `<div>symlink → <a class="link" data-path="${escape(target)}">${escape(value.target)}</a></div>`;
    } else html += `<div>${escape(value.kind)}</div>`;
    panel.body.innerHTML = html;
    for (const link of panel.body.querySelectorAll("a[data-path]")) link.onclick = () => browse(link.dataset.path);
  },

  "processes/{pid}/descriptors"(value, panel) {
    let html = `<table><tr><th>fd</th><th>what</th><th>path</th><th>offset</th><th>flags</th></tr>`;
    for (const d of value) {
      html += `<tr><td>${d.fd}</td><td>${escape(d.what)}</td><td>${d.path ? `<a class="link" data-path="${escape(d.path)}">${escape(d.path)}</a>` : ""}</td><td>${d.offset}</td><td>${d.flags}${d.cloexec ? " cloexec" : ""}</td></tr>`;
    }
    panel.body.innerHTML = html + "</table>";
    for (const link of panel.body.querySelectorAll("a[data-path]")) link.onclick = () => browse(link.dataset.path);
  },

  net(value, panel) {
    if (!value.sockets.length) {
      panel.body.innerHTML = `<div class="refusal">no sockets</div>`;
      return;
    }
    panel.body.innerHTML = value.sockets.map((s) => {
      let state;
      if (s.state === "idle") state = "idle";
      else if (s.state.bound) state = `bound ${s.state.bound}`;
      else if (s.state.listening) state = `listening on ${s.state.listening.address} · backlog ${s.state.listening.backlog} · ${s.state.listening.queued} waiting to be accepted`;
      else if (s.state.connected) {
        const c = s.state.connected;
        state = `connected ${c.local} ↔ ${c.peer} · ${c.receive_queued} bytes to read, ${c.transmit_queued} to send${c.read_shut ? " · read shut" : ""}${c.write_shut ? " · write shut" : ""}`;
      } else state = escape(JSON.stringify(s.state));
      return `<div class="socket"><span class="id">socket${s.id}</span> ${s.family} ${s.kind === 1 ? "stream" : s.kind === 2 ? "dgram" : "kind " + s.kind}${s.edge !== null ? ` · <b>edge conn/${s.edge}</b>` : ""} · held by pid ${s.holders.join(", ") || "nobody"}<div class="thread">${state}</div></div>`;
    }).join("");
  },

  "processes/{pid}/threads/{tid}/registers"(r, panel, state) {
    const names = ["rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "rsp", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15", "rip", "fs_base"];
    panel.body.innerHTML = `<div class="registers"><div><span class="name">thread</span><span class="value">pid ${state.pid} tid ${state.tid}</span></div>` +
      names.map((n) => `<div><span class="name">${n}</span><span class="value">${r[n]}</span></div>`).join("") +
      `<div><span class="name">flags</span><span class="value ${r.flags_stale ? "stale" : ""}">${r.flags}${r.flags_stale === true ? " (stale: a later instruction overwrote them before anything read them)" : r.flags_stale === null ? " (freshness unknown at this stop)" : ""}</span></div></div>`;
  },

  "processes/{pid}/threads/{tid}/disassembly"(lines, panel) {
    disassembled = lines.length;
    panel.body.innerHTML = lines.length
      ? lines.map((line, i) => `<div class="line${i === 0 ? " rip" : ""}"><span>${line.address}</span><span class="bytes">${line.bytes.match(/../g).join(" ")}</span><span>${escape(line.text)}</span></div>`).join("")
      : "<i>nothing executable at rip, or the module was baked without disassembly</i>";
  },

  "processes/{pid}/memory/{address}/{length}"(memory, panel) {
    panel.body.innerHTML = `<div id="stack">${hexDump(memory)}</div>`;
  },

  "processes/{pid}/maps"(maps, panel) {
    panel.body.innerHTML = `<pre id="maps">${escape(maps)}</pre>`;
  },

  "processes/{pid}/mapped"(value, panel) {
    panel.body.innerHTML = `<pre>${value.ranges.map(([a, b]) => `${a} – ${b}`).join("\n")}</pre>`;
  },
};

/// Which concrete path of the last state a pattern was read at.
function concreteOf(pattern) {
  return Object.keys(values).find((path) => values[path].pattern === pattern);
}

function render(state) {
  current = state.at;
  values = state.values;
  $("slider").value = current;
  $("at").textContent = current.toLocaleString();
  $("took").textContent = live && current === frontier ? "the frontier" : state.restored ? `restored in ${Math.round(state.restored)} ms` : "";
  for (const [pattern, panel] of panels) {
    const path = concreteOf(pattern);
    const held = path ? values[path] : null;
    panel.pathLabel.textContent = path ?? pattern;
    if (!held) {
      panel.body.innerHTML = `<div class="refusal">nothing to read here at this instant</div>`;
      panel.raw.textContent = "";
    } else if (held.error !== undefined) {
      panel.body.innerHTML = `<div class="refusal">${escape(held.error)}</div>`;
      panel.raw.textContent = held.error;
      if (pattern === "processes/{pid}/files/{path}") {
        panel.body.innerHTML += ` <a class="link" data-path="">back to /</a>`;
        panel.body.querySelector("a").onclick = () => browse("");
      }
    } else {
      const renderer = renderers[pattern];
      if (renderer) renderer(held.value, panel, state);
      else panel.body.innerHTML = renderJson(held.value);
      panel.raw.textContent = JSON.stringify(held.value, null, 1);
    }
    renderDiff(pattern, panel, path, held);
  }
  $("stdout").textContent = state.stdout;
  $("stderr").textContent = state.stderr + (state.log ? "\n" + state.log : "");
  renderEvents();
}

// ---- pin and diff --------------------------------------------------------------
//
// Values are structured, so "what changed between two instants" is a diff
// of two JSON values: keys added, keys gone, leaves changed. Arrays whose
// elements carry an identity — a pid, an fd, a socket id, a name — are
// matched by it, so a descriptor closed in the middle shows as the one
// descriptor gone rather than as every later one changed.

const IDENTITIES = ["pid", "tid", "fd", "id", "name", "address"];

function identityOf(array) {
  if (!array.length || !array.every((v) => v && typeof v === "object" && !Array.isArray(v))) return null;
  return IDENTITIES.find((key) => array.every((v) => v[key] !== undefined)) ?? null;
}

function diff(before, after, at = "", out = []) {
  if (out.length > 200) return out;
  if (before === after) return out;
  const kind = (v) => (Array.isArray(v) ? "array" : v === null ? "null" : typeof v);
  if (kind(before) !== kind(after) || kind(after) !== "object" && kind(after) !== "array") {
    if (JSON.stringify(before) !== JSON.stringify(after)) out.push({ at, before, after });
    return out;
  }
  if (kind(after) === "array") {
    const key = identityOf(before) ?? identityOf(after);
    if (key && (identityOf(before) === key || !before.length) && (identityOf(after) === key || !after.length)) {
      const was = new Map(before.map((v) => [v[key], v]));
      const is = new Map(after.map((v) => [v[key], v]));
      for (const [id, v] of was) if (!is.has(id)) out.push({ at: `${at}[${key}=${id}]`, before: v, after: undefined });
      for (const [id, v] of is) {
        if (!was.has(id)) out.push({ at: `${at}[${key}=${id}]`, before: undefined, after: v });
        else diff(was.get(id), v, `${at}[${key}=${id}]`, out);
      }
      return out;
    }
    const length = Math.max(before.length, after.length);
    for (let i = 0; i < length; i++) diff(before[i], after[i], `${at}[${i}]`, out);
    return out;
  }
  for (const key of new Set([...Object.keys(before), ...Object.keys(after)])) diff(before[key], after[key], at ? `${at}.${key}` : key, out);
  return out;
}

function short(value) {
  if (value === undefined) return "";
  const text = JSON.stringify(value);
  return text.length > 60 ? text.slice(0, 57) + "…" : text;
}

function renderDiff(pattern, panel, path, held) {
  if (!pinned) {
    panel.diff.innerHTML = "";
    return;
  }
  const then = pinned.values[path];
  if (!path || !then || !held) {
    const was = Object.keys(pinned.values).find((p) => pinned.values[p].pattern === pattern);
    panel.diff.innerHTML = `<div class="none">since ${pinned.at.toLocaleString()}: ${was ? `was ${escape(was)} then` : "not read then"}</div>`;
    return;
  }
  const changes = diff(then.value ?? then.error, held.value ?? held.error);
  if (!changes.length) {
    panel.diff.innerHTML = `<div class="none">since ${pinned.at.toLocaleString()}: unchanged</div>`;
    return;
  }
  panel.diff.innerHTML = `<div style="opacity:.7">since ${pinned.at.toLocaleString()}: ${changes.length} change${changes.length === 1 ? "" : "s"}</div>` + changes.slice(0, 40).map((c) => {
    if (c.before === undefined) return `<div class="add">+ ${escape(c.at)} ${escape(short(c.after))}</div>`;
    if (c.after === undefined) return `<div class="remove">− ${escape(c.at)} ${escape(short(c.before))}</div>`;
    return `<div class="change">~ ${escape(c.at)}: ${escape(short(c.before))} → ${escape(short(c.after))}</div>`;
  }).join("") + (changes.length > 40 ? `<div class="elided">… ${changes.length - 40} more</div>` : "");
}

function pin() {
  if (pinned) {
    pinned = null;
    $("pin").textContent = "pin";
    $("pinned").textContent = "";
  } else {
    pinned = { at: current, values: JSON.parse(JSON.stringify(values)) };
    $("pin").textContent = "unpin";
    $("pinned").textContent = `pinned at ${current.toLocaleString()}; panels show what changed since`;
  }
  for (const [pattern, panel] of panels) {
    const path = concreteOf(pattern);
    renderDiff(pattern, panel, path, path ? values[path] : null);
  }
}
$("pin").onclick = pin;

// ---- the path bar --------------------------------------------------------------

$("pathbar").onsubmit = async (event) => {
  event.preventDefault();
  const path = $("path").value.trim().replace(/^\/+/, "");
  if (!path) return;
  $("answer").textContent = `reading ${path} at ${current.toLocaleString()}…`;
  try {
    const value = await read(path);
    $("answer").textContent = `${path} at ${current.toLocaleString()}:\n${JSON.stringify(value, null, 1)}`;
  } catch (why) {
    $("answer").textContent = `${path} at ${current.toLocaleString()}: ${why}`;
  }
};

// ---- controls ----------------------------------------------------------------

function stop() {
  playing = null;
  $("play").textContent = "play";
}

$("slider").oninput = (event) => {
  stop();
  seek(Number(event.target.value));
};
$("play").onclick = () => {
  if (playing) return stop();
  if (live && finished !== null) return;
  playing = Number($("speed").value);
  $("play").textContent = "pause";
  if (live) advance(LIVE_TICK);
  else seek(current + playing);
};
$("back").onclick = () => { stop(); seek(current - 1); };
$("forward").onclick = () => { stop(); if (live && current === frontier) advance(1); else seek(current + 1); };
$("prev").onclick = () => {
  stop();
  const now = position();
  if (now >= 0 && events[now].at === current && now > 0) seek(events[now - 1].at);
  else if (now >= 0) seek(events[now].at);
};
$("next").onclick = () => {
  stop();
  const after = position() + 1;
  if (after < events.length) seek(events[after].at);
};

function send(port, request) {
  const id = nextRequest++;
  worker.postMessage({ type: "request", id, port, request });
  const box = document.createElement("div");
  box.className = "exchange-box";
  box.id = `exchange-${id}`;
  box.innerHTML = `<div class="meta">#${id} → port ${port}: ${escape(JSON.stringify(request))}</div>`;
  $("responses").prepend(box);
  if (!playing && finished === null) $("play").click();
  return id;
}

async function loadFiles(moduleFile, tapeFile, snapshotFile, every) {
  $("status").textContent = "loading…";
  const module = await moduleFile.arrayBuffer();
  const tape = tapeFile ? await tapeFile.arrayBuffer() : null;
  const snapshot = snapshotFile ? await snapshotFile.arrayBuffer() : null;
  const publish = $("publish").value.split(",").map((s) => Number(s.trim())).filter(Boolean);
  worker.postMessage({ type: "load", module, tape, snapshot, checkpointEvery: every, publish }, [module, tape, snapshot].filter(Boolean));
}
$("load").onclick = () => {
  const module = $("module").files[0];
  if (module) loadFiles(module, $("tape").files[0] ?? null, $("snapshot").files[0] ?? null, Number($("every").value));
  else $("status").textContent = "choose a module, and a tape to replay or none to run live";
};

// For a test driving the page from outside.
window.zaqaruDebug = {
  seek,
  advance,
  send,
  read,
  browse,
  view,
  pin,
  get current() {
    return current;
  },
  get origin() {
    return origin;
  },
  get frontier() {
    return frontier;
  },
  get total() {
    return total;
  },
  get busy() {
    return busy;
  },
  get live() {
    return live;
  },
  get responses() {
    return responses;
  },
  get disassembled() {
    return disassembled;
  },
  get syscalls() {
    return timeline.length;
  },
  get exchanges() {
    return exchanges.length;
  },
  get values() {
    return values;
  },
  get meta() {
    return meta;
  },
  get context() {
    return context;
  },
  get pinned() {
    return pinned;
  },
};

// `?module=…&tape=…` replays from URLs on the same origin; `?module=…&live=8080`
// runs live with those ports published; `&snapshot=…` starts from a booted
// container; `&every=…` sets the checkpoint interval.
const params = new URLSearchParams(location.search);
if (params.get("module")) {
  $("status").textContent = "fetching…";
  if (params.get("live") !== null) $("publish").value = params.get("live");
  const every = Number(params.get("every") ?? (params.get("snapshot") ? 20000000 : $("every").value));
  $("every").value = every;
  const fetchOptional = (name) => (params.get(name) ? fetch(params.get(name)).then((r) => (r.ok ? r.blob() : Promise.reject(`${params.get(name)}: ${r.status}`))) : Promise.resolve(null));
  Promise.all([fetch(params.get("module")).then((r) => r.blob()), fetchOptional("tape"), fetchOptional("snapshot")])
    .then(([m, t, s]) => loadFiles(m, t, s, every))
    .catch((why) => ($("status").textContent = String(why)));
}
