// The debugger's engine room: owns the container, the checkpoints and the
// timeline, and answers the page's requests. Runs in a Worker so that
// re-executing a few million instructions never freezes the page.
//
// Three ways to load. With a tape, the whole run is replayed once up front,
// checkpointing as it goes, and every instant is then a seek. Without one —
// live — the container runs against JavaScript's own clock and entropy and
// an edge the page can send requests through; everything the host answers
// is recorded, the frontier advances as the page asks, checkpoints are
// taken on the way, and a seek behind the frontier restores a checkpoint
// and re-executes against the recording. With a snapshot file (see
// `snapshot.js`), the live run starts from a container somebody already
// booted, and its history begins there.
//
// Messages in:  { type: "load", module, tape | null, snapshot | null, checkpointEvery, publish }
//               { type: "advance", by }          live: run the frontier on
//               { type: "seek", at }
//               { type: "request", id, port, request }   live: through the edge
// Messages out: { type: "loaded", ... }  { type: "progress", ... }
//               { type: "state", ... }   { type: "response", id, response }
//               { type: "error", message }

import { Container, Edge, KIND, MountTable, parseTape, standardMounts, text } from "./zaqaru.js";
import { Checkpoints } from "./checkpoints.js";
import { decode, gunzip } from "./snapshot.js";

let module = null;
let tape = null;
let live = null; // the frontier container, in live mode
let edge = null;
let checkpoints = null;
let checkpointEvery = 2000000;
let viewer = null; // the container standing at the last seek
let origin = 0; // where history begins: 0, or a snapshot's instant
let frontier = 0;
let finished = null;
let timelineSeen = 0; // bytes of the timeline sink already reported
let traceSeen = 0;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function replayMounts() {
  const table = standardMounts();
  table.replay(tape);
  return table;
}

function parseTimeline(textSoFar) {
  return textSoFar
    .trim()
    .split("\n")
    .filter(Boolean)
    .map((line) => {
      const [at, pid, tid, name] = line.split(" ");
      return { at: Number(at), pid: Number(pid), tid: Number(tid), name };
    });
}

/// What the container has logged since last asked: new timeline entries
/// and trace lines.
function growth(container) {
  const timelineText = text(container.readback(["iso", "log", "timeline"]) ?? new Uint8Array());
  const traceText = text(container.readback(["iso", "log", "debug"]) ?? new Uint8Array());
  const timeline = parseTimeline(timelineText.slice(timelineSeen));
  const trace = traceText.slice(traceSeen).split("\n").filter(Boolean);
  timelineSeen = timelineText.length;
  traceSeen = traceText.length;
  return { timeline, trace };
}

function console_(container, stream) {
  return text(container.readback(["iso", "console", stream]) ?? new Uint8Array());
}

async function load({ module: moduleBytes, tape: tapeBytes, snapshot: snapshotBytes, checkpointEvery: every, publish }) {
  const started = performance.now();
  module = await WebAssembly.compile(moduleBytes);
  checkpointEvery = every ?? 2000000;
  checkpoints = new Checkpoints();
  timelineSeen = 0;
  traceSeen = 0;
  finished = null;
  origin = 0;
  if (tapeBytes) {
    tape = parseTape(new Uint8Array(tapeBytes));
    live = null;
    const first = await Container.instantiate(module, replayMounts());
    first.step(0);
    checkpoints.add(0, first);
    let target = checkpointEvery;
    let total = 0;
    let diffing = 0;
    for (;;) {
      const turn = first.step(target);
      total = first.value("statistics").retired;
      if (turn.kind === KIND.FINISHED) {
        finished = turn.status;
        break;
      }
      const began = performance.now();
      checkpoints.add(total, first);
      diffing += performance.now() - began;
      target = total + checkpointEvery;
    }
    frontier = total;
    const { timeline, trace } = growth(first);
    viewer = first;
    postMessage({
      type: "loaded",
      live: false,
      origin: 0,
      total,
      frontier,
      finished,
      timeline,
      trace,
      output: console_(first, "stdout"),
      bytecode: tape.bytecode,
      checkpoints: Array.from({ length: checkpoints.length }, (_, i) => checkpoints.at(i)),
      held: checkpoints.held,
      naive: checkpoints.naive,
      diffing,
      loading: performance.now() - started,
    });
    return;
  }
  // Live: the world is JavaScript's, and recorded.
  tape = null;
  edge = new Edge(publish ?? []);
  if (snapshotBytes) {
    const file = decode(await gunzip(new Uint8Array(snapshotBytes)));
    const mounts = MountTable.load(file.mounts, { edge });
    mounts.record();
    live = await Container.continueFrom(module, file, mounts);
    origin = file.at;
    // The container has run, so its clock and its logs are consulted from
    // here: whatever the file kept of the console is the boot's output.
  } else {
    const mounts = standardMounts({ seed: null, config: { trace: 1 }, edge });
    mounts.record();
    live = await Container.instantiate(module, mounts);
    live.step(0);
  }
  const statistics = live.value("statistics");
  if (statistics.retired !== origin) throw `the container stands at ${statistics.retired}, not the snapshot's ${origin}`;
  checkpoints.add(origin, live);
  frontier = origin;
  viewer = live;
  growth(live); // the boot's logs, if any were kept, are not the timeline
  postMessage({
    type: "loaded",
    live: true,
    origin,
    total: origin,
    frontier: origin,
    finished: null,
    timeline: [],
    trace: [],
    output: console_(live, "stdout"),
    bytecode: true,
    checkpoints: [origin],
    held: checkpoints.held,
    naive: checkpoints.naive,
    diffing: 0,
    published: publish ?? [],
    listening: [...edge.listening],
    loading: performance.now() - started,
  });
}

/// Live: runs the frontier on by `by` instructions, checkpointing on the
/// way, and reports what happened. Returns early on an idle container: the
/// page may have a request to deliver, and spinning on a parked machine
/// would only burn the core.
async function advance(by) {
  if (!live || finished !== null) return;
  const target = frontier + by;
  let idles = 0;
  for (;;) {
    const turn = live.step(target);
    const retired = live.value("statistics").retired;
    if (turn.kind === KIND.FINISHED) {
      finished = turn.status;
      frontier = retired;
      break;
    }
    frontier = retired;
    if (turn.kind === KIND.IDLE) {
      idles++;
      if (idles > 20) break;
      await sleep(5);
      continue;
    }
    if (frontier >= checkpoints.at(checkpoints.length - 1) + checkpointEvery) checkpoints.add(frontier, live);
    break;
  }
  if (finished !== null) checkpoints.add(frontier, live);
  const { timeline, trace } = growth(live);
  postMessage({
    type: "progress",
    frontier,
    finished,
    idle: idles > 20,
    timeline,
    trace,
    stdout: console_(live, "stdout"),
    listening: edge ? [...edge.listening] : [],
    checkpoints: checkpoints.length,
    held: checkpoints.held,
  });
}

function ask(container, path, fallback) {
  try {
    return container.value(path);
  } catch (why) {
    return typeof fallback === "function" ? fallback(String(why)) : fallback;
  }
}

/// The machine at `at`: the frontier itself when asked for the frontier,
/// otherwise a restored checkpoint run exactly to the instant.
async function seek(at) {
  if (at < origin) at = origin;
  let restored = 0;
  if (live && at >= frontier) {
    viewer = live;
    at = frontier;
  } else {
    const began = performance.now();
    const index = checkpoints.before(at);
    viewer = await Container.fromSnapshot(module, checkpoints.snapshot(index));
    if (at > checkpoints.at(index)) viewer.stopAt(at);
    restored = performance.now() - began;
  }
  const statistics = viewer.value("statistics");
  const processes = viewer.value("processes");
  const current = processes.processes.find((p) => p.pid === statistics.current) ?? processes.processes[0];
  const thread = current?.threads.find((t) => t.state === "runnable") ?? current?.threads[0];
  let registers = null;
  let disassembly = [];
  let stack = null;
  let maps = null;
  let descriptors = [];
  if (current && thread) {
    const base = `processes/${current.pid}`;
    registers = ask(viewer, `${base}/threads/${thread.tid}/registers`, (why) => ({ error: why }));
    disassembly = ask(viewer, `${base}/threads/${thread.tid}/disassembly`, []);
    if (registers && !registers.error) stack = ask(viewer, `${base}/memory/${registers.rsp}/256`, null);
    maps = ask(viewer, `${base}/maps`, (why) => why);
    descriptors = ask(viewer, `${base}/descriptors`, []);
  }
  postMessage({
    type: "state",
    at: statistics.retired,
    restored,
    statistics,
    processes,
    thread: thread ? { pid: current.pid, tid: thread.tid } : null,
    registers,
    disassembly,
    stack,
    maps,
    descriptors,
    stdout: console_(viewer, "stdout"),
    stderr: console_(viewer, "stderr"),
    log: text(viewer.readback(["iso", "log", "error"]) ?? new Uint8Array()),
  });
}

/// Live: a request through the edge. The response arrives once the guest
/// has run far enough to answer, which `advance` drives.
function request(id, port, body) {
  if (!edge) return postMessage({ type: "response", id, error: "not a live run" });
  const sent = frontier;
  edge
    .request(port, new TextEncoder().encode(body))
    .then((response) => postMessage({ type: "response", id, response: text(response), sent, answered: frontier }))
    .catch((why) => postMessage({ type: "response", id, error: String(why) }));
}

onmessage = async (event) => {
  const message = event.data;
  try {
    if (message.type === "load") await load(message);
    else if (message.type === "advance") await advance(message.by);
    else if (message.type === "seek") await seek(message.at);
    else if (message.type === "request") request(message.id, message.port, message.request);
  } catch (why) {
    postMessage({ type: "error", message: String(why?.stack ?? why) });
  }
};
