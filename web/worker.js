// The debugger's engine room: owns the container, the checkpoints and the
// timeline, and answers the page's requests. Runs in a Worker so that
// re-executing a few million instructions never freezes the page.
//
// Two ways to load. With a tape, the whole run is replayed once up front,
// checkpointing as it goes, and every instant is then a seek. Without one —
// live — the container runs against JavaScript's own clock and entropy and
// an edge the page can send requests through; everything the host answers
// is recorded, the frontier advances as the page asks, checkpoints are
// taken on the way, and a seek behind the frontier restores a checkpoint
// and re-executes against the recording.
//
// Messages in:  { type: "load", module, tape | null, checkpointEvery, publish }
//               { type: "advance", by }          live: run the frontier on
//               { type: "seek", at }
//               { type: "request", id, port, request }   live: through the edge
// Messages out: { type: "loaded", ... }  { type: "progress", ... }
//               { type: "state", ... }   { type: "response", id, response }
//               { type: "error", message }

import { Container, Edge, KIND, parseTape, standardMounts, text } from "./zaqaru.js";
import { Checkpoints } from "./checkpoints.js";

let module = null;
let tape = null;
let live = null; // the frontier container, in live mode
let edge = null;
let checkpoints = null;
let checkpointEvery = 2000000;
let viewer = null; // the container standing at the last seek
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

async function load(moduleBytes, tapeBytes, every, publish) {
  module = await WebAssembly.compile(moduleBytes);
  checkpointEvery = every;
  checkpoints = new Checkpoints();
  timelineSeen = 0;
  traceSeen = 0;
  finished = null;
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
      const started = performance.now();
      checkpoints.add(total, first);
      diffing += performance.now() - started;
      target = total + checkpointEvery;
    }
    frontier = total;
    const { timeline, trace } = growth(first);
    viewer = first;
    postMessage({
      type: "loaded",
      live: false,
      total,
      frontier,
      finished,
      timeline,
      trace,
      output: text(first.readback(["iso", "console", "stdout"]) ?? new Uint8Array()),
      bytecode: tape.bytecode,
      checkpoints: Array.from({ length: checkpoints.length }, (_, i) => checkpoints.at(i)),
      held: checkpoints.held,
      naive: checkpoints.naive,
      diffing,
    });
    return;
  }
  // Live: the world is JavaScript's, and recorded.
  tape = null;
  edge = new Edge(publish ?? []);
  const mounts = standardMounts({ seed: null, config: { trace: 1 }, edge });
  mounts.record();
  live = await Container.instantiate(module, mounts);
  live.step(0);
  checkpoints.add(0, live);
  frontier = 0;
  viewer = live;
  postMessage({
    type: "loaded",
    live: true,
    total: 0,
    frontier: 0,
    finished: null,
    timeline: [],
    trace: [],
    output: "",
    bytecode: true,
    checkpoints: [0],
    held: checkpoints.held,
    naive: checkpoints.naive,
    diffing: 0,
    published: publish ?? [],
  });
}

/// Live: runs the frontier on by `by` instructions, checkpointing on the
/// way, and reports what happened.
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
    if (turn.kind === KIND.IDLE) {
      // Nothing runnable: the container is waiting on a deadline or the
      // edge. Give the page a moment to supply either rather than spin.
      idles++;
      if (idles > 20) {
        frontier = retired;
        break;
      }
      await sleep(5);
      continue;
    }
    frontier = retired;
    if (frontier >= checkpoints.at(checkpoints.length - 1) + checkpointEvery) checkpoints.add(frontier, live);
    break;
  }
  if (finished !== null) checkpoints.add(frontier, live);
  const { timeline, trace } = growth(live);
  postMessage({
    type: "progress",
    frontier,
    finished,
    timeline,
    trace,
    stdout: text(live.readback(["iso", "console", "stdout"]) ?? new Uint8Array()),
    listening: edge ? [...edge.listening] : [],
    checkpoints: checkpoints.length,
    held: checkpoints.held,
  });
}

/// The machine at `at`: the frontier itself when asked for the frontier,
/// otherwise a restored checkpoint run exactly to the instant.
async function seek(at) {
  if (live && at >= frontier) {
    viewer = live;
    at = frontier;
  } else {
    const index = checkpoints.before(at);
    const base = viewer ?? live;
    viewer = await base.restore(checkpoints.snapshot(index));
    if (at > checkpoints.at(index)) viewer.stopAt(at);
  }
  const statistics = viewer.value("statistics");
  const processes = viewer.value("processes");
  const current = processes.processes.find((p) => p.pid === statistics.current) ?? processes.processes[0];
  const thread = current?.threads.find((t) => t.state === "runnable") ?? current?.threads[0];
  let registers = null;
  let maps = null;
  let descriptors = null;
  if (current && thread) {
    try {
      registers = viewer.value(`processes/${current.pid}/threads/${thread.tid}/registers`);
    } catch (why) {
      registers = { error: String(why) };
    }
    try {
      maps = viewer.value(`processes/${current.pid}/maps`);
    } catch (why) {
      maps = String(why);
    }
    try {
      descriptors = viewer.value(`processes/${current.pid}/descriptors`);
    } catch {
      descriptors = [];
    }
  }
  postMessage({
    type: "state",
    at: statistics.retired,
    statistics,
    processes,
    registers,
    maps,
    descriptors,
    stdout: text(viewer.readback(["iso", "console", "stdout"]) ?? new Uint8Array()),
    stderr: text(viewer.readback(["iso", "console", "stderr"]) ?? new Uint8Array()),
    log: text(viewer.readback(["iso", "log", "error"]) ?? new Uint8Array()),
  });
}

/// Live: a request through the edge. The response arrives once the guest
/// has run far enough to answer, which `advance` drives.
function request(id, port, body) {
  if (!edge) return postMessage({ type: "response", id, error: "not a live run" });
  edge
    .request(port, new TextEncoder().encode(body))
    .then((response) => postMessage({ type: "response", id, response: text(response) }))
    .catch((why) => postMessage({ type: "response", id, error: String(why) }));
}

onmessage = async (event) => {
  const message = event.data;
  try {
    if (message.type === "load") await load(message.module, message.tape, message.checkpointEvery ?? 2000000, message.publish);
    else if (message.type === "advance") await advance(message.by);
    else if (message.type === "seek") await seek(message.at);
    else if (message.type === "request") request(message.id, message.port, message.request);
  } catch (why) {
    postMessage({ type: "error", message: String(why?.stack ?? why) });
  }
};
