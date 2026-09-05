// The page: a timeline over retired instructions, the syscall log as a
// clickable time axis, and panels describing the machine at the chosen
// instant. Everything it knows comes from the worker, which asks the
// container's own store.
//
// Two modes. With a tape, the run is fixed and every instant is a seek. Live,
// the container runs against this page's clock and entropy; "play" advances
// the frontier, the slider views anything behind it, and a request typed into
// the edge box goes to a listener inside the container.

const worker = new Worker("./worker.js", { type: "module" });
const $ = (id) => document.getElementById(id);

let live = false;
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

function seek(at) {
  at = Math.max(0, Math.min(live ? frontier : total, Math.round(at)));
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
    total = message.total;
    frontier = message.frontier;
    finished = message.finished;
    timeline = message.timeline;
    trace = message.trace;
    $("slider").max = live ? frontier : total;
    $("edge").hidden = !live;
    const mb = (n) => (n / 1048576).toFixed(1) + " MB";
    $("status").textContent = live
      ? `live, publishing port${message.published.length === 1 ? "" : "s"} ${message.published.join(", ") || "none"} — press play`
      : `${total.toLocaleString()} instructions, ${timeline.length} syscalls, ${message.checkpoints.length} checkpoints holding ${mb(message.held)} (${mb(message.naive)} as full copies, diffed in ${Math.round(message.diffing)} ms), ${message.bytecode ? "bytecode" : "interpreter"}`;
    renderTrace();
    seek(live ? 0 : 0);
    return;
  }
  if (message.type === "progress") {
    busy = false;
    frontier = message.frontier;
    finished = message.finished;
    timeline.push(...message.timeline);
    trace.push(...message.trace);
    $("slider").max = frontier;
    renderTrace();
    $("status").textContent = `live: ${frontier.toLocaleString()} instructions so far, ${timeline.length} syscalls, ${message.checkpoints} checkpoints holding ${(message.held / 1048576).toFixed(1)} MB${message.listening.length ? `, listening on ${message.listening.join(", ")}` : ""}${finished !== null ? `, exited ${finished}` : ""}`;
    if (!drain()) {
      if (playing && finished === null) advance(playing);
      else {
        if (finished !== null) stop();
        seek(frontier);
      }
    }
    return;
  }
  if (message.type === "state") {
    busy = false;
    render(message);
    if (!drain() && playing) {
      if (live) advance(playing);
      else seek(current + playing);
    }
    return;
  }
  if (message.type === "response") {
    const box = $("responses");
    const line = document.createElement("pre");
    line.textContent = message.error ? `#${message.id} ${message.error}` : `#${message.id} ← ${message.response}`;
    box.prepend(line);
  }
};

function renderTrace() {
  const list = $("syscalls");
  list.innerHTML = "";
  timeline.forEach((entry, index) => {
    const row = document.createElement("div");
    row.className = "syscall" + (entry.at <= current ? " past" : "");
    row.dataset.at = entry.at;
    const line = trace[index] ?? entry.name;
    row.textContent = `${entry.at.toLocaleString().padStart(14)}  ${line}`;
    row.onclick = () => {
      stop();
      seek(entry.at);
    };
    list.appendChild(row);
  });
}

function render(state) {
  current = state.at;
  $("slider").value = current;
  $("at").textContent = current.toLocaleString() + (live && current === frontier ? " (frontier)" : "");
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
    registers.innerHTML = names.map((n) => `<div><span class="name">${n}</span><span class="value">${r[n]}</span></div>`).join("") + `<div><span class="name">flags</span><span class="value ${r.flags_stale ? "stale" : ""}">${r.flags}${r.flags_stale === true ? " (stale: a later instruction overwrote them before anything read them)" : r.flags_stale === null ? " (freshness unknown at this stop)" : ""}</span></div>`;
  } else registers.textContent = state.registers?.error ?? "no running thread";
  $("maps").textContent = state.maps ?? "";
  $("descriptors").textContent = (state.descriptors ?? []).map((d) => `${d.fd}: ${d.what}  offset ${d.offset}  flags ${d.flags}${d.cloexec ? "  cloexec" : ""}`).join("\n");
  $("stdout").textContent = state.stdout;
  $("stderr").textContent = state.stderr + (state.log ? "\n" + state.log : "");
  for (const row of document.querySelectorAll(".syscall")) row.classList.toggle("past", Number(row.dataset.at) <= current);
  const last = [...document.querySelectorAll(".syscall.past")].pop();
  for (const row of document.querySelectorAll(".syscall.now")) row.classList.remove("now");
  if (last) {
    last.classList.add("now");
    last.scrollIntoView({ block: "nearest" });
  }
}

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
  if (live) advance(playing);
  else seek(current + playing);
};
$("back").onclick = () => { stop(); seek(current - 1); };
$("forward").onclick = () => { stop(); if (live && current === frontier) advance(1); else seek(current + 1); };
$("prev").onclick = () => {
  stop();
  const before = timeline.filter((e) => e.at < current);
  if (before.length) seek(before[before.length - 1].at);
};
$("next").onclick = () => {
  stop();
  const after = timeline.find((e) => e.at > current);
  if (after) seek(after.at);
};
$("send").onclick = () => {
  const id = nextRequest++;
  worker.postMessage({ type: "request", id, port: Number($("port").value), request: $("request").value.replace(/\\n/g, "\n").replace(/\\r/g, "\r") });
  const box = $("responses");
  const line = document.createElement("pre");
  line.textContent = `#${id} → port ${$("port").value}: ${JSON.stringify($("request").value)}`;
  box.prepend(line);
  if (!playing) $("play").click();
};

async function loadFiles(moduleFile, tapeFile) {
  $("status").textContent = "loading…";
  const module = await moduleFile.arrayBuffer();
  const tape = tapeFile ? await tapeFile.arrayBuffer() : null;
  const publish = $("publish").value.split(",").map((s) => Number(s.trim())).filter(Boolean);
  worker.postMessage({ type: "load", module, tape, checkpointEvery: Number($("every").value), publish }, tape ? [module, tape] : [module]);
}
$("load").onclick = () => {
  const module = $("module").files[0];
  const tape = $("tape").files[0];
  if (module) loadFiles(module, tape ?? null);
  else $("status").textContent = "choose a module, and a tape to replay or none to run live";
};

// For a test driving the page from outside.
window.zaqaruDebug = {
  seek,
  advance,
  get current() {
    return current;
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
};

// `?module=…&tape=…` replays from URLs on the same origin; `?module=…&live=8080`
// runs live with those ports published.
const params = new URLSearchParams(location.search);
if (params.get("module")) {
  $("status").textContent = "fetching…";
  if (params.get("live") !== null) $("publish").value = params.get("live");
  const tapeUrl = params.get("tape");
  Promise.all([fetch(params.get("module")), tapeUrl ? fetch(tapeUrl) : Promise.resolve(null)])
    .then(async ([m, t]) => loadFiles(await m.blob(), t ? await t.blob() : null))
    .catch((why) => ($("status").textContent = String(why)));
}
