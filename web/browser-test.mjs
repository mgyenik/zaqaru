// Drives the debugger page in headless Chrome over the DevTools protocol and
// checks what it shows: the run loads, a seek lands on the instruction, the
// panels fill in, and clicking a syscall seeks to it.
//
//   node web/browser-test.mjs [chrome-binary] [--only replay|live|snapshot|django]
//
// Serves the repository itself on a local port, so the fixture at
// web/fixture (from web/fixture.sh) is what the page loads.

import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { existsSync, openSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { extname, join, normalize } from "node:path";

const root = new URL("..", import.meta.url).pathname;
const only = process.argv.includes("--only") ? process.argv[process.argv.indexOf("--only") + 1] : null;
const chrome = process.argv.slice(2).find((a, i, all) => !a.startsWith("--") && all[i - 1] !== "--only") ?? "google-chrome";
const runs = (scenario) => only === null || only === scenario;
const types = { ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript", ".wasm": "application/wasm", ".bin": "application/octet-stream", ".snapshot": "application/octet-stream", ".txt": "text/plain" };
const demo = existsSync(join(root, "web/demo/hello-django.snapshot"));

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
// Chrome's own stderr goes to a file beside its profile, for when a tab dies.
const chromeLog = `/tmp/zaqaru-browser-test-${process.pid}.log`;
const browser = spawn(chrome, ["--headless=new", "--disable-gpu", "--no-sandbox", `--remote-debugging-port=${debugPort}`, "--user-data-dir=/tmp/zaqaru-browser-test-" + process.pid, "about:blank"], { stdio: ["ignore", "ignore", openSync(chromeLog, "w")] });

let failures = 0;
function check(name, condition, detail = "") {
  if (condition) console.log(`ok   ${name}`);
  else {
    console.log(`FAIL ${name} ${detail}`);
    failures++;
  }
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const consoleLines = [];

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
  socket.onmessage = (event) => {
    const message = JSON.parse(event.data);
    if (message.id && waiting.has(message.id)) {
      waiting.get(message.id)(message);
      waiting.delete(message.id);
    }
    if (message.method === "Runtime.consoleAPICalled") consoleLines.push(message.params.args.map((a) => a.value ?? a.description).join(" "));
    if (message.method === "Runtime.exceptionThrown") consoleLines.push("exception: " + JSON.stringify(message.params.exceptionDetails.exception?.description ?? message.params.exceptionDetails.text));
    if (message.method === "Inspector.targetCrashed" || message.method === "Target.targetCrashed") crashed = true;
  };
  let crashed = false;
  socket.onclose = () => (crashed = true);
  browser.on("exit", () => (crashed = true));
  const send = (method, params = {}, sessionId) =>
    new Promise((resolve, reject) => {
      const id = next++;
      // A tab that has crashed answers nothing; say so rather than hang.
      const timer = setInterval(() => {
        if (crashed) {
          clearInterval(timer);
          waiting.delete(id);
          reject(`the page or the browser died; see ${chromeLog}`);
        }
      }, 500);
      waiting.set(id, (reply) => {
        clearInterval(timer);
        resolve(reply);
      });
      socket.send(JSON.stringify({ id, method, params, sessionId }));
    });
  const { result: { targetId } } = await send("Target.createTarget", { url: page });
  const { result: { sessionId } } = await send("Target.attachToTarget", { targetId, flatten: true });
  await send("Runtime.enable", {}, sessionId);
  await send("Inspector.enable", {}, sessionId);
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
      // The worker's failures land in the status line; do not wait out a
      // timeout on a page that has already said what went wrong.
      const status = await evaluate(`document.getElementById("status")?.textContent ?? ""`);
      if (/Error|error:|\bat .*\.js:\d+/.test(status) && !expression.includes("status")) throw `the page reports: ${status.slice(0, 400)}`;
      await sleep(250);
    }
  };

  if (runs("replay")) {
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
  const disassembled = await evaluate("window.zaqaruDebug.disassembled");
  const ripLine = await evaluate(`document.querySelector(".line.rip")?.textContent ?? ""`);
  check("the disassembly panel shows the instructions at rip", disassembled > 1 && /0x[0-9a-f]+/.test(ripLine), `${disassembled} lines; ${ripLine.slice(0, 80)}`);
  const stack = await evaluate(`document.querySelectorAll("#stack .word").length`);
  check("the stack panel shows the words under rsp", stack === 16, String(stack));

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
  }

  if (runs("live")) {
  // Live: the server module, run against the page's own clock, with a
  // request sent through the edge box.
  await send("Page.navigate", { url: `http://127.0.0.1:${port}/web/?module=fixture/server.wasm&live=8080` }, sessionId);
  await until(`document.readyState === "complete" && !!window.zaqaruDebug && window.zaqaruDebug.live === true`);
  await until(`!window.zaqaruDebug.busy && document.getElementById("status").textContent.includes("press play")`);
  await evaluate(`document.getElementById("request").value = "ping\\n"; document.getElementById("send").click()`);
  const response = await until(`(() => { const r = document.getElementById("responses").textContent; return /pong|not published|error/.test(r) ? r : ""; })()`, 120000);
  check("a request through the edge is answered by the server in the container", /answered at [\d,]+.*pong/s.test(response), response.slice(0, 200));
  await until(`document.getElementById("status").textContent.includes("exited 0")`, 60000);
  const liveStatus = await evaluate(`document.getElementById("status").textContent`);
  check("the live run finishes", /exited 0/.test(liveStatus), liveStatus);
  console.log("     " + liveStatus);
  await until(`!window.zaqaruDebug.busy`);
  const frontier = await evaluate("window.zaqaruDebug.frontier");
  await evaluate("window.zaqaruDebug.seek(Math.floor(window.zaqaruDebug.frontier / 2))");
  await until(`!window.zaqaruDebug.busy && window.zaqaruDebug.current === Math.floor(${frontier} / 2)`);
  const midway = await evaluate(`document.getElementById("stdout").textContent`);
  check("seeking into the live run's past re-executes against the recording", midway === "listening on 8080\n" || midway === "", JSON.stringify(midway));
  }

  if (runs("snapshot")) {
  // From a snapshot: the same server, already listening when the page
  // loads it, its history beginning at the file's instant.
  await send("Page.navigate", { url: `http://127.0.0.1:${port}/web/?module=fixture/server.wasm&snapshot=fixture/server.snapshot&live=8080` }, sessionId);
  await until(`document.readyState === "complete" && !!window.zaqaruDebug && window.zaqaruDebug.live === true && window.zaqaruDebug.origin > 0`);
  await until(`!window.zaqaruDebug.busy && document.getElementById("status").textContent.includes("press play")`);
  const origin = await evaluate("window.zaqaruDebug.origin");
  const snapshotStatus = await evaluate(`document.getElementById("status").textContent`);
  check("a snapshot loads listening", origin > 100000 && /from a snapshot/.test(snapshotStatus) && /listening on 8080/.test(snapshotStatus), snapshotStatus);
  const bootOutput = await evaluate(`document.getElementById("stdout").textContent`);
  check("the boot's console came through the file", bootOutput === "listening on 8080\n", JSON.stringify(bootOutput));
  await evaluate(`window.zaqaruDebug.send(8080, "ping\\n")`);
  const snapshotResponse = await until(`(() => { const r = document.getElementById("responses").textContent; return /pong|not published|error/.test(r) ? r : ""; })()`, 120000);
  check("the server continued from the file answers", /pong/.test(snapshotResponse), snapshotResponse.slice(0, 200));
  await until(`document.getElementById("status").textContent.includes("exited 0")`, 60000);
  await until(`!window.zaqaruDebug.busy`);
  const snapshotFrontier = await evaluate("window.zaqaruDebug.frontier");
  const between = Math.floor((origin + snapshotFrontier) / 2);
  await evaluate(`window.zaqaruDebug.seek(${between})`);
  await until(`!window.zaqaruDebug.busy && window.zaqaruDebug.current === ${between}`);
  const betweenRegisters = await evaluate(`document.getElementById("registers").textContent`);
  check("seeking between the file's instant and the frontier re-executes", /rip0x[0-9a-f]+/.test(betweenRegisters), betweenRegisters.slice(0, 80));
  await evaluate(`window.zaqaruDebug.seek(0)`);
  await until(`!window.zaqaruDebug.busy`);
  const clamped = await evaluate("window.zaqaruDebug.current");
  check("history begins at the file's instant", clamped === origin, `${clamped} vs ${origin}`);
  }

  // The demo itself, when it has been made: nginx, gunicorn and Django,
  // booted, answering a request from the page.
  if (demo && runs("django")) {
    await send("Page.navigate", { url: `http://127.0.0.1:${port}/web/?module=demo/hello-django.wasm&snapshot=demo/hello-django.snapshot&live=80` }, sessionId);
    await until(`document.readyState === "complete" && !!window.zaqaruDebug && window.zaqaruDebug.live === true && window.zaqaruDebug.origin > 0`, 180000);
    await until(`!window.zaqaruDebug.busy && document.getElementById("status").textContent.includes("press play")`, 60000);
    const djangoStatus = await evaluate(`document.getElementById("status").textContent`);
    console.log("     " + djangoStatus);
    check("django loads from its snapshot, listening on 80", /listening on 80/.test(djangoStatus), djangoStatus);
    const asked = Date.now();
    await evaluate(`window.zaqaruDebug.send(80, "GET / HTTP/1.0\\r\\n\\r\\n")`);
    const django = await until(`(() => { const r = document.getElementById("responses").textContent; return r.includes("answered at") || r.includes(" — ") ? r : ""; })()`, 300000);
    const took = ((Date.now() - asked) / 1000).toFixed(1);
    check("nginx answers the page's request with django's page", /HTTP\/1\.[01] 200/.test(django) && /Hello|hello/.test(django), django.slice(0, 300));
    console.log(`     answered in ${took} s: ${django.slice(0, 160).replace(/\n/g, " ")}`);
    await evaluate(`document.getElementById("play").textContent === "pause" && document.getElementById("play").click()`);
    await until(`!window.zaqaruDebug.busy`);
    const djangoFrontier = await evaluate("window.zaqaruDebug.frontier");
    const djangoOrigin = await evaluate("window.zaqaruDebug.origin");
    const syscalls = await evaluate("window.zaqaruDebug.syscalls");
    check("the request left syscalls on the timeline", syscalls > 20, String(syscalls));
    const middle = Math.floor((djangoOrigin + djangoFrontier) / 2);
    const seekStarted = Date.now();
    await evaluate(`window.zaqaruDebug.seek(${middle})`);
    await until(`!window.zaqaruDebug.busy && window.zaqaruDebug.current === ${middle}`, 120000);
    const djangoRegisters = await evaluate(`document.getElementById("registers").textContent`);
    const djangoProcesses = await evaluate(`document.getElementById("processes").textContent`);
    check("seeking into the request re-executes to the instant", /rip0x[0-9a-f]+/.test(djangoRegisters) && /pid 4|pid 3|pid 2|pid 1/.test(djangoProcesses), djangoRegisters.slice(0, 80));
    console.log(`     seek to ${middle.toLocaleString()} took ${((Date.now() - seekStarted) / 1000).toFixed(1)} s; ${await evaluate(`document.getElementById("at").textContent`)}`);
    const djangoDisassembly = await evaluate("window.zaqaruDebug.disassembled");
    check("the disassembly panel reads django's code", djangoDisassembly > 1, String(djangoDisassembly));
  } else if (runs("django")) console.log("     (no web/demo/hello-django.snapshot: the django scenario is skipped; make it with web/demo.sh)");
} catch (why) {
  console.log("FAIL " + why);
  failures++;
} finally {
  if (consoleLines.length) console.log("console:\n  " + consoleLines.join("\n  "));
  browser.kill();
  server.close();
}
console.log(failures ? `${failures} failure(s)` : "all passed");
process.exit(failures ? 1 : 0);
