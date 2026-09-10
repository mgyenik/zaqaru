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
// What a seek hands back is not a fixed set of panels: the worker reads
// the container's `meta` lens once, and at every instant reads every path
// it declares that the instant can fill — `{pid}` and `{tid}` from the
// process and thread in view, `{path}` from the page's browsing, the
// memory under `rsp`. A path the kernel adds appears on the page with no
// change here.
//
// Messages in:  { type: "load", module, tape | null, snapshot | null, checkpointEvery, publish }
//               { type: "advance", by }                 live: run the frontier on
//               { type: "seek", at, context }           context: { pid, tid, path }
//               { type: "read", id, path }              one path, at the instant in view
//               { type: "request", id, port, request }  live: through the edge
// Messages out: { type: "loaded", ... }  { type: "progress", ... }
//               { type: "state", ... }   { type: "value", id, path, value | error }
//               { type: "response", id, response }
//               { type: "error", message }

import { Container, Edge, KIND, MountTable, parseTape, standardMounts, text } from "./zaqaru.js";
import { Checkpoints } from "./checkpoints.js";
import { decode, inflate, refill } from "./snapshot.js";

let module = null;
let tape = null;
let live = null; // the frontier container, in live mode
let edge = null;
let checkpoints = null;
let checkpointEvery = 2000000;
let viewer = null; // the container standing at the last seek
let origin = 0; // where history begins: 0, or a snapshot's instant
let inflated = 0; // ms spent inflating the snapshot
let frontier = 0;
let finished = null;
let timelineSeen = 0; // bytes of the timeline sink already reported
let traceSeen = 0;
let observed = null; // the mount table's exchange log
let observedSeen = 0; // entries of it already reported
let meta = null; // the store's meta lens: { paths: { pattern: { readable, writable } } }

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

/// What the container has logged since last asked: new timeline entries,
/// trace lines, and exchanges across the boundary.
function growth(container) {
  const timelineText = text(container.readback(["iso", "log", "timeline"]) ?? new Uint8Array());
  const traceText = text(container.readback(["iso", "log", "debug"]) ?? new Uint8Array());
  const timeline = parseTimeline(timelineText.slice(timelineSeen));
  const trace = traceText.slice(traceSeen).split("\n").filter(Boolean);
  timelineSeen = timelineText.length;
  traceSeen = traceText.length;
  const exchanges = observed ? observed.slice(observedSeen) : [];
  observedSeen = observed ? observed.length : 0;
  return { timeline, trace, exchanges };
}

function console_(container, stream) {
  return text(container.readback(["iso", "console", stream]) ?? new Uint8Array());
}

/// The store's declaration of itself: the meta lens for what is readable,
/// the manifest for what each path means.
function describe(container) {
  meta = container.value("meta");
  let manifest = null;
  try {
    manifest = JSON.parse(container.manifest());
  } catch {
    manifest = null;
  }
  return { meta, interface: manifest?.paths ?? {} };
}

async function load({ module: moduleBytes, tape: tapeBytes, snapshot: snapshotBytes, checkpointEvery: every, publish }) {
  const started = performance.now();
  module = await WebAssembly.compile(moduleBytes);
  checkpointEvery = every ?? 2000000;
  checkpoints = new Checkpoints();
  timelineSeen = 0;
  traceSeen = 0;
  observedSeen = 0;
  finished = null;
  origin = 0;
  if (tapeBytes) {
    tape = parseTape(new Uint8Array(tapeBytes));
    live = null;
    const mounts = replayMounts();
    observed = mounts.observe();
    const first = await Container.instantiate(module, mounts);
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
    const { timeline, trace, exchanges } = growth(first);
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
      exchanges,
      ...describe(first),
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
    const inflating = performance.now();
    const file = decode(await inflate(new Uint8Array(snapshotBytes)));
    inflated = performance.now() - inflating;
    const mounts = MountTable.load(file.mounts, { edge });
    mounts.record();
    observed = mounts.observe();
    live = await Container.continueFrom(module, file, mounts);
    if (file.refill) refill(live);
    origin = file.at;
    // The container has run, so its clock and its logs are consulted from
    // here: whatever the file kept of the console is the boot's output.
  } else {
    const mounts = standardMounts({ seed: null, config: { trace: 1 }, edge });
    mounts.record();
    observed = mounts.observe();
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
    exchanges: [],
    ...describe(live),
    output: console_(live, "stdout"),
    bytecode: true,
    checkpoints: [origin],
    held: checkpoints.held,
    naive: checkpoints.naive,
    diffing: 0,
    published: publish ?? [],
    listening: [...edge.listening],
    loading: performance.now() - started,
    inflated,
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
  const { timeline, trace, exchanges } = growth(live);
  postMessage({
    type: "progress",
    frontier,
    finished,
    idle: idles > 20,
    timeline,
    trace,
    exchanges,
    stdout: console_(live, "stdout"),
    listening: edge ? [...edge.listening] : [],
    checkpoints: checkpoints.length,
    held: checkpoints.held,
  });
}

/// One read of the container in view: `{ pattern, value }` or
/// `{ pattern, error }`, never a throw — a refusal is something to show.
function read(container, pattern, path) {
  try {
    return { pattern, value: container.value(path) };
  } catch (why) {
    return { pattern, error: String(why) };
  }
}

/// A pattern of the meta lens with its parameters filled from `fill`, or
/// null when one of them has no value here.
function instantiate(pattern, fill) {
  let missing = false;
  const path = pattern.replace(/\{(\w+)\}/g, (_, name) => {
    if (fill[name] === undefined || fill[name] === null) missing = true;
    return String(fill[name] ?? "");
  });
  return missing ? null : path.replace(/\/+$/, "");
}

/// Stands a container at `at`: the frontier itself when asked for the
/// frontier, otherwise a restored checkpoint run exactly to the instant.
async function stand(at) {
  if (at < origin) at = origin;
  if (live && at >= frontier) {
    viewer = live;
    return 0;
  }
  const began = performance.now();
  const index = checkpoints.before(at);
  viewer = await Container.fromSnapshot(module, checkpoints.snapshot(index));
  if (at > checkpoints.at(index)) viewer.stopAt(at);
  return performance.now() - began;
}

/// The machine at `at`, as every path of its store the instant can fill.
/// `context` says which process and thread the page is looking at (the
/// running ones when it says nothing) and where it is browsing the files.
async function seek(at, context = {}) {
  const restored = await stand(at);
  const values = {};
  const statistics = read(viewer, "statistics", "statistics");
  const processes = read(viewer, "processes", "processes");
  values.statistics = statistics;
  values.processes = processes;
  const all = processes.value?.processes ?? [];
  const running = statistics.value?.current;
  const current = all.find((p) => p.pid === context.pid) ?? all.find((p) => p.pid === running) ?? all[0];
  const thread = current?.threads.find((t) => t.tid === context.tid) ?? current?.threads.find((t) => t.state === "runnable") ?? current?.threads[0];
  const fill = { pid: current?.pid, tid: thread?.tid, path: (context.path ?? "").replace(/^\/+/, "") };
  for (const [pattern, lens] of Object.entries(meta?.paths ?? {})) {
    if (!lens.readable || pattern === "meta" || pattern.startsWith("meta/") || pattern.includes("{address}")) continue;
    const path = instantiate(pattern, fill);
    if (path === null || values[path]) continue;
    values[path] = read(viewer, pattern, path);
  }
  // The memory under the stack pointer, once the registers say where it is.
  const registers = fill.tid !== undefined ? values[`processes/${fill.pid}/threads/${fill.tid}/registers`]?.value : null;
  if (registers?.rsp) {
    const pattern = "processes/{pid}/memory/{address}/{length}";
    const path = instantiate(pattern, { ...fill, address: registers.rsp, length: 256 });
    values[path] = read(viewer, pattern, path);
  }
  postMessage({
    type: "state",
    at: statistics.value?.retired ?? at,
    restored,
    pid: current?.pid ?? null,
    tid: thread?.tid ?? null,
    values,
    stdout: console_(viewer, "stdout"),
    stderr: console_(viewer, "stderr"),
    log: text(viewer.readback(["iso", "log", "error"]) ?? new Uint8Array()),
  });
}

/// One path, read at the instant in view.
function readOne(id, path) {
  const answer = viewer ? read(viewer, path, path) : { pattern: path, error: "nothing is loaded" };
  postMessage({ type: "value", id, path, ...answer });
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
    else if (message.type === "seek") await seek(message.at, message.context);
    else if (message.type === "read") readOne(message.id, message.path);
    else if (message.type === "request") request(message.id, message.port, message.request);
  } catch (why) {
    postMessage({ type: "error", message: String(why?.stack ?? why) });
  }
};
