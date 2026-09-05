// The page: a timeline over retired instructions, the syscall log as a
// clickable time axis, and panels describing the machine at the chosen
// instant. Everything it knows comes from the worker, which asks the
// container's own store.

const worker = new Worker("./worker.js", { type: "module" });
const $ = (id) => document.getElementById(id);

let total = 0;
let timeline = [];
let trace = [];
let playing = null;
let current = 0;
let busy = false;
let queued = null;

function seek(at) {
  at = Math.max(0, Math.min(total, Math.round(at)));
  if (busy) {
    queued = at;
    return;
  }
  busy = true;
  worker.postMessage({ type: "seek", at });
}

worker.onmessage = (event) => {
  const message = event.data;
  if (message.type === "error") {
    $("status").textContent = message.message;
    busy = false;
    return;
  }
  if (message.type === "loaded") {
    total = message.total;
    timeline = message.timeline;
    trace = message.trace;
    $("slider").max = total;
    $("status").textContent = `${total.toLocaleString()} instructions, ${timeline.length} syscalls, ${message.checkpoints.length} checkpoints, ${message.bytecode ? "bytecode" : "interpreter"}`;
    renderTrace();
    seek(0);
    return;
  }
  if (message.type === "state") {
    busy = false;
    render(message);
    if (queued !== null) {
      const next = queued;
      queued = null;
      seek(next);
    } else if (playing) {
      seek(current + playing);
    }
  }
};

function renderTrace() {
  const list = $("syscalls");
  list.innerHTML = "";
  timeline.forEach((entry, index) => {
    const row = document.createElement("div");
    row.className = "syscall";
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
  $("at").textContent = current.toLocaleString();
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
  playing = Number($("speed").value);
  $("play").textContent = "pause";
  seek(current + playing);
};
$("back").onclick = () => { stop(); seek(current - 1); };
$("forward").onclick = () => { stop(); seek(current + 1); };
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

async function loadFiles(moduleFile, tapeFile) {
  $("status").textContent = "loading…";
  const [module, tape] = await Promise.all([moduleFile.arrayBuffer(), tapeFile.arrayBuffer()]);
  worker.postMessage({ type: "load", module, tape, checkpointEvery: Number($("every").value) }, [module, tape]);
}
$("load").onclick = () => {
  const module = $("module").files[0];
  const tape = $("tape").files[0];
  if (module && tape) loadFiles(module, tape);
  else $("status").textContent = "choose a module and a tape";
};

// For a test driving the page from outside.
window.zaqaruDebug = {
  seek,
  get current() {
    return current;
  },
  get total() {
    return total;
  },
  get busy() {
    return busy;
  },
};

// `?module=…&tape=…` loads from URLs on the same origin.
const params = new URLSearchParams(location.search);
if (params.get("module") && params.get("tape")) {
  $("status").textContent = "fetching…";
  Promise.all([fetch(params.get("module")), fetch(params.get("tape"))])
    .then(async ([m, t]) => loadFiles(await m.blob(), await t.blob()))
    .catch((why) => ($("status").textContent = String(why)));
}
