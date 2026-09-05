// Boots a container under Node and writes it to a file once it is quiet, so
// that the debugger can start from a booted server rather than boot one.
//
//   node web/preboot.mjs <module.wasm> <out.snapshot> [--publish 80,8080]
//                        [--quiet-ms 3000] [--quiet-instructions 2000000]
//
// The container runs live against this process's clock and entropy, with
// the ports named published on an edge nobody sends anything to. It is
// quiet when, for `quiet-ms` of wall time, it retires fewer than
// `quiet-instructions` — a server whose processes are all parked on
// timeouts, waking only to see nothing has happened. What is written is
// what `snapshot.js` describes; the boot's own syscall log is not kept,
// since nothing can seek into the time before the file.

import { readFileSync, writeFileSync } from "node:fs";
import { Container, Edge, KIND, standardMounts, text } from "./zaqaru.js";
import { changedSince, encode, gzip } from "./snapshot.js";

const args = process.argv.slice(2);
const positional = args.filter((a) => !a.startsWith("--"));
const option = (name, fallback) => {
  const at = args.indexOf(`--${name}`);
  return at >= 0 ? args[at + 1] : fallback;
};
const [modulePath, outPath] = positional;
if (!outPath) {
  console.error("usage: node web/preboot.mjs <module.wasm> <out.snapshot> [--publish 80] [--quiet-ms 3000] [--quiet-instructions 2000000]");
  process.exit(2);
}
const publish = (option("publish", "") || "").split(",").map((s) => Number(s.trim())).filter(Boolean);
const quietMs = Number(option("quiet-ms", 3000));
const quietInstructions = Number(option("quiet-instructions", 2000000));

const started = performance.now();
const module = await WebAssembly.compile(readFileSync(modulePath));
const fresh = await Container.instantiate(module, standardMounts());
const freshMemory = new Uint8Array(fresh.memory.buffer).slice();

const edge = new Edge(publish);
const mounts = standardMounts({ seed: null, config: { trace: 1 }, edge, echo: (path, data) => process.stderr.write(text(data)) });
const container = await Container.instantiate(module, mounts);
container.step(0);

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
let retired = 0;
let finished = null;
let windowStart = performance.now();
let windowRetired = 0;
let lastReport = 0;
for (;;) {
  const turn = container.step(retired + 5000000);
  retired = container.value("statistics").retired;
  if (turn.kind === KIND.FINISHED) {
    finished = turn.status;
    break;
  }
  if (turn.kind === KIND.IDLE) await sleep(5);
  const now = performance.now();
  if (now - lastReport > 5000) {
    lastReport = now;
    console.error(`preboot: ${retired.toLocaleString()} instructions, ${((now - started) / 1000).toFixed(0)} s, listening on ${[...edge.listening].join(", ") || "nothing"}`);
  }
  if (now - windowStart >= quietMs) {
    if (retired - windowRetired < quietInstructions && publish.every((port) => edge.reachable(port))) break;
    windowStart = now;
    windowRetired = retired;
  }
}
if (finished !== null) {
  console.error(`preboot: the container exited ${finished} before it was quiet; nothing written`);
  process.exit(1);
}

const memory = new Uint8Array(container.memory.buffer);
const pages = changedSince(freshMemory, memory);
const file = encode({
  at: retired,
  stackPointer: container.stackPointer,
  length: memory.length,
  pages,
  mounts: mounts.save({ drop: ["iso/log"] }),
});
const compressed = await gzip(file);
writeFileSync(outPath, compressed);
console.error(
  `preboot: quiet at ${retired.toLocaleString()} instructions after ${((performance.now() - started) / 1000).toFixed(0)} s; ` +
    `${pages.size} pages changed of ${(memory.length / 1048576).toFixed(0)} MB; ` +
    `${(file.length / 1048576).toFixed(1)} MB, ${(compressed.length / 1048576).toFixed(1)} MB compressed → ${outPath}`,
);
