// The page: a timeline over retired instructions, the syscall log as a
// clickable time axis, and panels describing the machine at the chosen
// instant. Everything it knows comes from the worker, which asks the
// container's own store.
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
let timeline = [];
let trace = [];
let playing = null;
let current = 0;
let busy = false;
let queued = null;
let nextRequest = 0;
let responses = 0;
let disassembled = 0;

function seek(at) {
  at = Math.max(origin, Math.min(live ? frontier : total, Math.round(at)));
  if (busy) {
    queued = { seek: at };
    return;
  }
  busy = true;
  worker.postMessage({ type: "seek", at });
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

const mb = (n) => (n / 1048576).toFixed(1) + " MB";

function status(extra = "") {
  if (live) {
    const parts = [
      `live${origin ? `, from a snapshot at ${origin.toLocaleString()}` : ""}: ${(frontier - origin).toLocaleString()} instructions so far`,
      `${timeline.length} syscalls`,
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
    $("slider").min = origin;
    $("slider").max = live ? frontier : total;
    $("controls").classList.add("hidden");
    if (live) {
      $("port").value = message.published[0] ?? 8080;
      status(`${message.checkpoints.length} checkpoint holding ${mb(message.held)}${message.listening.length ? `, listening on ${message.listening.join(", ")}` : `, publishing ${message.published.join(", ") || "no ports"}`}, loaded in ${(message.loading / 1000).toFixed(1)} s — press play, then send a request`);
    } else {
      $("status").textContent = `${total.toLocaleString()} instructions, ${timeline.length} syscalls, ${message.checkpoints.length} checkpoints holding ${mb(message.held)} (${mb(message.naive)} as full copies, diffed in ${Math.round(message.diffing)} ms), ${message.bytecode ? "bytecode" : "interpreter"}, loaded in ${(message.loading / 1000).toFixed(1)} s`;
    }
    renderTrace(true);
    seek(origin);
    return;
  }
  if (message.type === "progress") {
    busy = false;
    frontier = message.frontier;
    finished = message.finished;
    timeline.push(...message.timeline);
    trace.push(...message.trace);
    $("slider").max = frontier;
    status(`${message.checkpoints} checkpoints holding ${mb(message.held)}${message.listening.length ? `, listening on ${message.listening.join(", ")}` : ""}${message.idle && finished === null ? ", idle" : ""}`);
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
  if (message.type === "response") {
    const box = $(`exchange-${message.id}`);
    if (!box) return;
    responses++;
    if (message.error) box.querySelector(".meta").textContent += ` — ${message.error}`;
    else {
      const meta = box.querySelector(".meta");
      meta.textContent = `#${message.id} sent at ${message.sent.toLocaleString()}, answered at ${message.answered.toLocaleString()} — `;
      const link = document.createElement("a");
      link.textContent = `seek to the answer`;
      link.onclick = () => {
        stop();
        seek(message.answered);
      };
      meta.appendChild(link);
      const body = document.createElement("pre");
      body.textContent = message.response;
      box.appendChild(body);
    }
  }
};

// ---- the syscall list --------------------------------------------------------
//
// A run is a million syscalls long before it is interesting, so the list
// holds rows only for a window around the present, and rebuilds the window
// when the present leaves it.

const WINDOW = 150;
let rendered = { from: -1, to: -1 };

/// The index of the last timeline entry at or before `current`.
function position() {
  let low = 0;
  let high = timeline.length;
  while (low < high) {
    const middle = (low + high) >> 1;
    if (timeline[middle].at <= current) low = middle + 1;
    else high = middle;
  }
  return low - 1;
}

function renderTrace(force = false) {
  const list = $("syscalls");
  const now = position();
  const from = Math.max(0, now - WINDOW);
  const to = Math.min(timeline.length, now + WINDOW);
  if (force || from !== rendered.from || to !== rendered.to) {
    rendered = { from, to };
    list.innerHTML = "";
    if (from > 0) {
      const elided = document.createElement("div");
      elided.className = "elided";
      elided.textContent = `… ${from.toLocaleString()} earlier`;
      list.appendChild(elided);
    }
    for (let index = from; index < to; index++) {
      const entry = timeline[index];
      const row = document.createElement("div");
      row.className = "syscall";
      row.dataset.at = entry.at;
      row.dataset.index = index;
      row.textContent = `${entry.at.toLocaleString().padStart(14)}  ${trace[index] ?? entry.name}`;
      row.onclick = () => {
        stop();
        seek(entry.at);
      };
      list.appendChild(row);
    }
    if (to < timeline.length) {
      const elided = document.createElement("div");
      elided.className = "elided";
      elided.textContent = `… ${(timeline.length - to).toLocaleString()} later`;
      list.appendChild(elided);
    }
  }
  for (const row of list.querySelectorAll(".syscall")) {
    const index = Number(row.dataset.index);
    row.classList.toggle("past", index <= now);
    row.classList.toggle("now", index === now);
  }
  const marked = list.querySelector(".syscall.now");
  if (marked) marked.scrollIntoView({ block: "nearest" });
}

// ---- the panels --------------------------------------------------------------

/// 256 bytes from rsp as quadwords, two a row, little-endian read out.
function hexDump(stack) {
  if (!stack?.bytes) return "";
  const base = BigInt(stack.address);
  const hex = stack.bytes;
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

function render(state) {
  current = state.at;
  $("slider").value = current;
  $("at").textContent = current.toLocaleString();
  $("took").textContent = live && current === frontier ? "the frontier" : state.restored ? `restored in ${Math.round(state.restored)} ms` : "";
  const s = state.statistics;
  $("statistics").textContent = `retired ${s.retired.toLocaleString()}   in bytecode ${s.accelerated.toLocaleString()}   blocks decoded ${s.decoded}   current pid ${s.current}`;
  const processes = $("processes");
  processes.innerHTML = "";
  for (const p of state.processes.processes) {
    const box = document.createElement("div");
    box.className = "process" + (p.pid === s.current ? " current" : "");
    const state_ = typeof p.state === "string" ? p.state : JSON.stringify(p.state);
    box.innerHTML = `<b>pid ${p.pid}</b> parent ${p.parent} · ${state_}` + p.threads.map((t) => `<div class="thread">tid ${t.tid} @ ${t.rip} · ${t.state} · retired ${t.retired.toLocaleString()}</div>`).join("");
    processes.appendChild(box);
  }
  const registers = $("registers");
  if (state.registers && !state.registers.error) {
    const r = state.registers;
    const names = ["rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "rsp", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15", "rip", "fs_base"];
    registers.innerHTML =
      `<div><span class="name">thread</span><span class="value">pid ${state.thread.pid} tid ${state.thread.tid}</span></div>` +
      names.map((n) => `<div><span class="name">${n}</span><span class="value">${r[n]}</span></div>`).join("") +
      `<div><span class="name">flags</span><span class="value ${r.flags_stale ? "stale" : ""}">${r.flags}${r.flags_stale === true ? " (stale: a later instruction overwrote them before anything read them)" : r.flags_stale === null ? " (freshness unknown at this stop)" : ""}</span></div>`;
  } else registers.textContent = state.registers?.error ?? "no running thread";
  const disassembly = $("disassembly");
  disassembled = state.disassembly.length;
  disassembly.innerHTML = state.disassembly.length
    ? state.disassembly.map((line, i) => `<div class="line${i === 0 ? " rip" : ""}"><span>${line.address}</span><span class="bytes">${line.bytes.match(/../g).join(" ")}</span><span>${line.text.replace(/</g, "&lt;")}</span></div>`).join("")
    : "<i>nothing executable at rip, or the module was baked without disassembly</i>";
  $("stack").innerHTML = hexDump(state.stack);
  $("maps").textContent = state.maps ?? "";
  $("descriptors").textContent = (state.descriptors ?? []).map((d) => `${d.fd}: ${d.what}  offset ${d.offset}  flags ${d.flags}${d.cloexec ? "  cloexec" : ""}`).join("\n") + "\n";
  $("stdout").textContent = state.stdout;
  $("stderr").textContent = state.stderr + (state.log ? "\n" + state.log : "");
  renderTrace();
}

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
  if (now >= 0 && timeline[now].at === current && now > 0) seek(timeline[now - 1].at);
  else if (now >= 0) seek(timeline[now].at);
};
$("next").onclick = () => {
  stop();
  const after = position() + 1;
  if (after < timeline.length) seek(timeline[after].at);
};

function send(port, request) {
  const id = nextRequest++;
  worker.postMessage({ type: "request", id, port, request });
  const box = document.createElement("div");
  box.className = "exchange";
  box.id = `exchange-${id}`;
  box.innerHTML = `<div class="meta">#${id} → port ${port}: ${JSON.stringify(request)}</div>`;
  $("responses").prepend(box);
  if (!playing && finished === null) $("play").click();
  return id;
}
$("send-form").onsubmit = (event) => {
  event.preventDefault();
  send(Number($("port").value), $("request").value.replace(/\\n/g, "\n").replace(/\\r/g, "\r"));
};

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
