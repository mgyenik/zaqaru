// Checks a built demo under Node before anything serves it: the module and
// the snapshot load, the container stands up listening, a request through
// the edge is answered, and the answer says what it should.
//
//   node web/check-demo.mjs <module.wasm> <snapshot> <port> <request> <expected-text>
//
// The request's \r and \n escapes are interpreted. Exits 0 when the
// response contains the expected text.

import { readFileSync } from "node:fs";
import { Container, Edge, KIND, MountTable, text } from "./zaqaru.js";
import { decode, gunzip } from "./snapshot.js";

const [modulePath, snapshotPath, portText, requestText, expected] = process.argv.slice(2);
if (!expected) {
  console.error('usage: node web/check-demo.mjs <module.wasm> <snapshot> <port> <request> <expected-text>');
  process.exit(2);
}
const port = Number(portText);
const started = performance.now();
const module = await WebAssembly.compile(readFileSync(modulePath));
const file = decode(await gunzip(new Uint8Array(readFileSync(snapshotPath))));
const edge = new Edge([port]);
const mounts = MountTable.load(file.mounts, { edge });
mounts.record();
const container = await Container.continueFrom(module, file, mounts);
const loaded = performance.now() - started;
if (!edge.reachable(port)) {
  console.error(`check-demo: the snapshot is not listening on ${port} (listening on ${[...edge.listening].join(", ") || "nothing"})`);
  process.exit(1);
}
let answered = null;
edge
  .request(port, new TextEncoder().encode(requestText.replace(/\\r/g, "\r").replace(/\\n/g, "\n")))
  .then((response) => (answered = text(response)))
  .catch((why) => (answered = `ERROR ${why}`));
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
let frontier = file.at;
const asked = performance.now();
while (answered === null && performance.now() - asked < 60000) {
  const turn = container.step(frontier + 4000000);
  frontier = container.value("statistics").retired;
  if (turn.kind === KIND.FINISHED) {
    console.error(`check-demo: the container exited ${turn.status} before answering`);
    process.exit(1);
  }
  if (turn.kind === KIND.IDLE) await sleep(5);
}
if (answered === null) {
  console.error("check-demo: no answer in 60 s");
  process.exit(1);
}
const ok = answered.includes(expected);
console.error(
  `check-demo: loaded in ${(loaded / 1000).toFixed(1)} s, answered in ${((performance.now() - asked) / 1000).toFixed(2)} s and ${(frontier - file.at).toLocaleString()} instructions: ` +
    `${JSON.stringify(answered.split("\n")[0])}${ok ? "" : ` — does not contain ${JSON.stringify(expected)}`}`,
);
process.exit(ok ? 0 : 1);
