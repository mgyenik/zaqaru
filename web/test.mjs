// A smoke test of the harness under Node: replays a recorded run and checks
// it against what the wasmtime host produced, then steps, stops, asks,
// snapshots and restores.
//
//   node web/test.mjs <module.wasm> <tape.bin> <stdout.txt>
//
// The fixture is made by `web/fixture.sh`.

import { readFileSync } from "node:fs";
import { Container, KIND, parseTape, standardMounts, text } from "./zaqaru.js";

const [modulePath, tapePath, stdoutPath] = process.argv.slice(2);
if (!stdoutPath) {
  console.error("usage: node web/test.mjs <module.wasm> <tape.bin> <stdout.txt>");
  process.exit(2);
}
const moduleBytes = readFileSync(modulePath);
const tape = parseTape(new Uint8Array(readFileSync(tapePath)));
const expected = readFileSync(stdoutPath, "utf8");
const module = await WebAssembly.compile(moduleBytes);

let failures = 0;
function check(name, condition, detail = "") {
  if (condition) console.log(`ok   ${name}`);
  else {
    console.log(`FAIL ${name} ${detail}`);
    failures++;
  }
}

function replayMounts() {
  const mounts = standardMounts();
  mounts.replay(tape);
  return mounts;
}

// 1. A straight replay reproduces the recorded run.
{
  const container = await Container.instantiate(module, replayMounts());
  const manifest = JSON.parse(container.manifest());
  check("the manifest names the container", manifest.name === "zaqaru-container", manifest.name);
  const status = container.boot();
  const out = text(container.readback(["iso", "console", "stdout"]) ?? new Uint8Array());
  check("the replay exits 0", status === 0, `status ${status}`);
  check("the replay prints what the recording printed", out === expected, JSON.stringify({ out, expected }));
  const statistics = container.value("statistics");
  check("a finished container reports its cost", statistics.retired > 0, JSON.stringify(statistics));
  const timeline = text(container.readback(["iso", "log", "timeline"]) ?? new Uint8Array());
  check("the timeline stamps every traced syscall", /^\d+ \d+ \d+ \w+\n/.test(timeline), timeline.slice(0, 80));
  check("the trace was on in the recording", (container.readback(["iso", "log", "debug"]) ?? []).length > 0);
  globalThis.total = statistics.retired;
}

// 2. Stepping in quanta reaches the same end, and the machine can be asked
//    while stopped.
{
  const container = await Container.instantiate(module, replayMounts());
  let steps = 0;
  let status = null;
  let asked = null;
  for (;;) {
    steps++;
    const turn = container.step(steps * 100000);
    if (turn.kind === KIND.FINISHED) {
      status = turn.status;
      break;
    }
    if (steps === 5) asked = container.value("processes");
  }
  check("stepping takes many steps", steps > 20, `${steps}`);
  check("stepping exits 0", status === 0);
  check("a stopped container lists its processes", asked && asked.processes[0].pid === 1, JSON.stringify(asked));
  const out = text(container.readback(["iso", "console", "stdout"]));
  check("stepping prints the same output", out === expected);
  check("stepping retires the same count", container.value("statistics").retired === globalThis.total);
}

// 3. An exact stop lands on the instruction, twice.
{
  const target = 777777;
  const places = [];
  for (let i = 0; i < 2; i++) {
    const container = await Container.instantiate(module, replayMounts());
    const turn = container.stopAt(target);
    check(`stop_at(${target}) stops`, turn.kind === KIND.STOPPED, JSON.stringify(turn));
    const statistics = container.value("statistics");
    check("the stop is exact", statistics.retired === target, JSON.stringify(statistics));
    places.push(JSON.stringify(container.value("processes/1/threads/1/registers")));
  }
  check("two stops stand in the same place", places[0] === places[1]);
}

// 4. Snapshot and restore: byte for byte.
{
  const original = await Container.instantiate(module, replayMounts());
  for (let step = 1; step <= 6; step++) original.step(step * 100000);
  const snapshot = original.snapshot();
  const status = original.boot();
  const out = text(original.readback(["iso", "console", "stdout"]));
  const memory = original.snapshot().memory;
  const restored = await original.restore(snapshot);
  const restoredStatus = restored.boot();
  check("a restored container exits the same", restoredStatus === status);
  check("a restored container prints the same", text(restored.readback(["iso", "console", "stdout"])) === out);
  const restoredMemory = restored.snapshot().memory;
  let first = -1;
  if (memory.length !== restoredMemory.length) first = -2;
  else for (let i = 0; i < memory.length; i++) if (memory[i] !== restoredMemory[i]) { first = i; break; }
  check("a restored container's memory is byte-identical at the end", first === -1, `first difference at ${first}`);
  // And a restore of an exact stop stands where the stop stood.
  const stopped = await Container.instantiate(module, replayMounts());
  stopped.stopAt(654321);
  const registers = stopped.ask("processes/1/threads/1/registers");
  const again = await stopped.restore(stopped.snapshot());
  check("a restored exact stop stands in the same place", again.ask("processes/1/threads/1/registers") === registers);
}

console.log(failures ? `${failures} failure(s)` : "all passed");
process.exit(failures ? 1 : 0);
