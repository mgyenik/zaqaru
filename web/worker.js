// The debugger's engine room: owns the container, the checkpoints and the
// full timeline, and answers the page's requests. Runs in a Worker so that
// re-executing a few million instructions never freezes the page.
//
// Messages in:  { type: "load", module, tape, checkpointEvery }
//               { type: "seek", at }
// Messages out: { type: "loaded", total, timeline, trace, output, bytecode }
//               { type: "state", at, statistics, processes, registers, maps,
//                 descriptors, stdout, stderr, log }
//               { type: "error", message }

import { Container, KIND, parseTape, standardMounts, text } from "./zaqaru.js";
import { Checkpoints } from "./checkpoints.js";

let module = null;
let tape = null;
let checkpoints = null; // a Checkpoints store
let container = null; // the one standing at the last seek

function mounts() {
  const table = standardMounts();
  table.replay(tape);
  return table;
}

async function load(moduleBytes, tapeBytes, checkpointEvery) {
  module = await WebAssembly.compile(moduleBytes);
  tape = parseTape(new Uint8Array(tapeBytes));
  checkpoints = new Checkpoints();
  // One pass over the whole run: checkpoints on the way, and the timeline
  // and trace as the container wrote them.
  const first = await Container.instantiate(module, mounts());
  first.step(0); // boot, and stand at zero
  checkpoints.add(0, first);
  let target = checkpointEvery;
  let total = 0;
  let diffing = 0;
  for (;;) {
    const turn = first.step(target);
    total = first.value("statistics").retired;
    if (turn.kind === KIND.FINISHED) break;
    const started = performance.now();
    checkpoints.add(total, first);
    diffing += performance.now() - started;
    target = total + checkpointEvery;
  }
  const timeline = text(first.readback(["iso", "log", "timeline"]) ?? new Uint8Array())
    .trim()
    .split("\n")
    .filter(Boolean)
    .map((line) => {
      const [at, pid, tid, name] = line.split(" ");
      return { at: Number(at), pid: Number(pid), tid: Number(tid), name };
    });
  const trace = text(first.readback(["iso", "log", "debug"]) ?? new Uint8Array()).trim().split("\n").filter(Boolean);
  const output = text(first.readback(["iso", "console", "stdout"]) ?? new Uint8Array());
  container = first;
  postMessage({
    type: "loaded",
    total,
    timeline,
    trace,
    output,
    bytecode: tape.bytecode,
    checkpoints: Array.from({ length: checkpoints.length }, (_, i) => checkpoints.at(i)),
    held: checkpoints.held,
    naive: checkpoints.naive,
    diffing,
  });
}

async function seek(at) {
  const index = checkpoints.before(at);
  container = await container.restore(checkpoints.snapshot(index));
  if (at > checkpoints.at(index)) container.stopAt(at);
  const statistics = container.value("statistics");
  const processes = container.value("processes");
  const current = processes.processes.find((p) => p.pid === statistics.current) ?? processes.processes[0];
  const thread = current?.threads.find((t) => t.state === "runnable") ?? current?.threads[0];
  let registers = null;
  let maps = null;
  let descriptors = null;
  if (current && thread) {
    try {
      registers = container.value(`processes/${current.pid}/threads/${thread.tid}/registers`);
    } catch (why) {
      registers = { error: String(why) };
    }
    try {
      maps = container.value(`processes/${current.pid}/maps`);
    } catch (why) {
      maps = String(why);
    }
    try {
      descriptors = container.value(`processes/${current.pid}/descriptors`);
    } catch (why) {
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
    stdout: text(container.readback(["iso", "console", "stdout"]) ?? new Uint8Array()),
    stderr: text(container.readback(["iso", "console", "stderr"]) ?? new Uint8Array()),
    log: text(container.readback(["iso", "log", "error"]) ?? new Uint8Array()),
  });
}

onmessage = async (event) => {
  const message = event.data;
  try {
    if (message.type === "load") await load(message.module, message.tape, message.checkpointEvery ?? 2000000);
    else if (message.type === "seek") await seek(message.at);
  } catch (why) {
    postMessage({ type: "error", message: String(why?.stack ?? why) });
  }
};
