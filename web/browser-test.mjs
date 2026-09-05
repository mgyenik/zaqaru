// Drives the debugger page in headless Chrome over the DevTools protocol and
// checks what it shows: the run loads, a seek lands on the instruction, the
// panels fill in, and clicking a syscall seeks to it.
//
//   node web/browser-test.mjs [chrome-binary]
//
// Serves the repository itself on a local port, so the fixture at
// web/fixture (from web/fixture.sh) is what the page loads.

import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname, join, normalize } from "node:path";

const root = new URL("..", import.meta.url).pathname;
const chrome = process.argv[2] ?? "google-chrome";
const types = { ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript", ".wasm": "application/wasm", ".bin": "application/octet-stream", ".txt": "text/plain" };

const server = createServer(async (request, response) => {
  const path = normalize(decodeURIComponent(new URL(request.url, "http://x").pathname));
  const file = join(root, path.endsWith("/") ? path + "index.html" : path);
  try {
    const body = await readFile(file);
    response.writeHead(200, { "content-type": types[extname(file)] ?? "application/octet-stream" });
    response.end(body);
  } catch {
    response.writeHead(404);
    response.end();
  }
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;
const page = `http://127.0.0.1:${port}/web/?module=fixture/module.wasm&tape=fixture/tape.bin`;

const debugPort = 9300 + Math.floor(Math.random() * 500);
const browser = spawn(chrome, ["--headless=new", "--disable-gpu", "--no-sandbox", `--remote-debugging-port=${debugPort}`, "--user-data-dir=/tmp/zaqaru-browser-test-" + process.pid, "about:blank"], { stdio: "ignore" });

let failures = 0;
function check(name, condition, detail = "") {
  if (condition) console.log(`ok   ${name}`);
  else {
    console.log(`FAIL ${name} ${detail}`);
    failures++;
  }
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

try {
  let version = null;
  for (let attempt = 0; attempt < 50 && !version; attempt++) {
    try {
      version = await (await fetch(`http://127.0.0.1:${debugPort}/json/version`)).json();
    } catch {
      await sleep(200);
    }
  }
  if (!version) throw "chrome did not open its debugging port";
  const socket = new WebSocket(version.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    socket.onopen = resolve;
    socket.onerror = reject;
  });
  let next = 1;
  const waiting = new Map();
  const consoleLines = [];
  socket.onmessage = (event) => {
    const message = JSON.parse(event.data);
    if (message.id && waiting.has(message.id)) {
      waiting.get(message.id)(message);
      waiting.delete(message.id);
    }
    if (message.method === "Runtime.consoleAPICalled") consoleLines.push(message.params.args.map((a) => a.value ?? a.description).join(" "));
    if (message.method === "Runtime.exceptionThrown") consoleLines.push("exception: " + JSON.stringify(message.params.exceptionDetails.exception?.description ?? message.params.exceptionDetails.text));
  };
  const send = (method, params = {}, sessionId) =>
    new Promise((resolve) => {
      const id = next++;
      waiting.set(id, resolve);
      socket.send(JSON.stringify({ id, method, params, sessionId }));
    });
  const { result: { targetId } } = await send("Target.createTarget", { url: page });
  const { result: { sessionId } } = await send("Target.attachToTarget", { targetId, flatten: true });
  await send("Runtime.enable", {}, sessionId);
  const evaluate = async (expression) => {
    const reply = await send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true }, sessionId);
    if (reply.result?.exceptionDetails) throw reply.result.exceptionDetails.exception?.description ?? "evaluation failed";
    return reply.result?.result?.value;
  };
  const until = async (expression, timeout = 120000) => {
    const started = Date.now();
    for (;;) {
      const value = await evaluate(expression);
      if (value) return value;
      if (Date.now() - started > timeout) throw `timed out waiting for ${expression}`;
      await sleep(250);
    }
  };

  await until(`document.readyState === "complete" && !!document.getElementById("status") && !!window.zaqaruDebug`);
  const status = await until(`(() => { const s = document.getElementById("status")?.textContent ?? ""; return s.includes("instructions") ? s : (s.includes("error") || s.includes("Error") ? s : ""); })()`);
  check("the run loads", /instructions/.test(status), status);
  console.log("     " + status);
  await until(`!window.zaqaruDebug.busy && document.getElementById("at").textContent === "0"`);
  const total = await evaluate("window.zaqaruDebug.total");
  check("the timeline knows the run's length", total > 4000000, String(total));

  await evaluate("window.zaqaruDebug.seek(777777)");
  await until(`!window.zaqaruDebug.busy && window.zaqaruDebug.current === 777777`);
  const at = await evaluate(`document.getElementById("at").textContent`);
  check("a seek lands on the instruction", at === "777,777", at);
  const registers = await evaluate(`document.getElementById("registers").textContent`);
  check("the registers panel shows rip", /rip0x[0-9a-f]+/.test(registers), registers.slice(0, 120));
  const maps = await evaluate(`document.getElementById("maps").textContent`);
  check("the memory map panel is filled", /r-xp|rw-p/.test(maps), maps.slice(0, 80));
  const processes = await evaluate(`document.getElementById("processes").textContent`);
  check("the processes panel names pid 1", /pid 1/.test(processes), processes.slice(0, 120));

  const rows = await evaluate(`document.querySelectorAll(".syscall").length`);
  check("the syscall log has rows", rows > 10, String(rows));
  const lastAt = await evaluate(`(() => { const rows = document.querySelectorAll(".syscall"); const row = rows[rows.length - 1]; row.click(); return Number(row.dataset.at); })()`);
  await until(`!window.zaqaruDebug.busy && window.zaqaruDebug.current === ${lastAt}`);
  const stdout = await evaluate(`document.getElementById("stdout").textContent`);
  check("clicking the last syscall shows the output written by then", /child said/.test(stdout), JSON.stringify(stdout));
  const now = await evaluate(`document.querySelector(".syscall.now")?.dataset.at`);
  check("the clicked syscall is marked current", Number(now) === lastAt, `${now} vs ${lastAt}`);

  await evaluate("window.zaqaruDebug.seek(1)");
  await until(`!window.zaqaruDebug.busy && window.zaqaruDebug.current === 1`);
  const early = await evaluate(`document.getElementById("stdout").textContent`);
  check("seeking back empties the console", early === "", JSON.stringify(early));
  if (consoleLines.length) console.log("console:\n  " + consoleLines.join("\n  "));
} catch (why) {
  console.log("FAIL " + why);
  failures++;
} finally {
  browser.kill();
  server.close();
}
console.log(failures ? `${failures} failure(s)` : "all passed");
process.exit(failures ? 1 : 0);
