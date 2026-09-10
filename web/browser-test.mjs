// Drives the debugger page in a headless browser and checks what it shows:
// the run loads, a seek lands on the instruction, the panels fill in,
// clicking a syscall seeks to it, a live server answers through the edge, a
// snapshot continues, and — when web/demo.sh has been run — Django answers
// and is seekable.
//
//   node web/browser-test.mjs [--browser chrome|firefox] [--binary path]
//                             [--driver geckodriver] [--only replay|live|snapshot|django]
//
// Chrome is driven over the DevTools protocol, Firefox over WebDriver
// through geckodriver. A Firefox installed as a snap wants the snap's own
// driver, `--driver firefox.geckodriver`, since a geckodriver outside the
// snap may not start the browser inside it. Serves the repository itself
// on a local port, so the fixture at web/fixture (from web/fixture.sh) is
// what the page loads.

import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { existsSync, openSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { extname, join, normalize } from "node:path";

const root = new URL("..", import.meta.url).pathname;
const option = (name, fallback) => (process.argv.includes(`--${name}`) ? process.argv[process.argv.indexOf(`--${name}`) + 1] : fallback);
const only = option("only", null);
const kind = option("browser", "chrome");
const binary = option("binary", kind === "chrome" ? "google-chrome" : null);
const driver = option("driver", "geckodriver");
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
// The browser's own stderr goes to a file, for when a tab dies.
const browserLog = `/tmp/zaqaru-browser-test-${process.pid}.log`;

/// Headless Chrome over the DevTools protocol: `{ navigate, evaluate, close }`.
async function chrome() {
  const debugPort = 9300 + Math.floor(Math.random() * 500);
  const browser = spawn(binary, ["--headless=new", "--disable-gpu", "--no-sandbox", `--remote-debugging-port=${debugPort}`, "--user-data-dir=/tmp/zaqaru-browser-test-" + process.pid, "about:blank"], { stdio: ["ignore", "ignore", openSync(browserLog, "w")] });
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
  let crashed = false;
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
          reject(`the page or the browser died; see ${browserLog}`);
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
  return {
    navigate: (url) => send("Page.navigate", { url }, sessionId),
    evaluate: async (expression) => {
      const reply = await send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true }, sessionId);
      if (reply.result?.exceptionDetails) throw reply.result.exceptionDetails.exception?.description ?? "evaluation failed";
      return reply.result?.result?.value;
    },
    close: () => browser.kill(),
  };
}

/// Headless Firefox over WebDriver, through geckodriver. Expressions go
/// through `eval` so that a statement list means what it means in Chrome's
/// `Runtime.evaluate`: the last statement's value.
async function firefox() {
  const driverPort = 4400 + Math.floor(Math.random() * 500);
  const process_ = spawn(driver, ["--port", String(driverPort), ...(binary ? ["--binary", binary] : [])], { stdio: ["ignore", "ignore", openSync(browserLog, "w")] });
  let alive = true;
  process_.on("exit", () => (alive = false));
  const base = `http://127.0.0.1:${driverPort}`;
  const call = async (method, path, body) => {
    if (!alive) throw `geckodriver died; see ${browserLog}`;
    const response = await fetch(base + path, { method, headers: { "content-type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body) });
    const reply = await response.json();
    if (!response.ok) throw `${reply.value?.error}: ${reply.value?.message}`;
    return reply.value;
  };
  let session = null;
  let refusal = null;
  for (let attempt = 0; attempt < 60 && !session; attempt++) {
    try {
      session = await call("POST", "/session", { capabilities: { alwaysMatch: { "moz:firefoxOptions": { args: ["-headless"] } } } });
    } catch (why) {
      refusal = String(why);
      await sleep(500);
    }
  }
  if (!session) throw `geckodriver did not open a session: ${refusal}`;
  const id = session.sessionId;
  await call("POST", `/session/${id}/url`, { url: page });
  return {
    navigate: (url) => call("POST", `/session/${id}/url`, { url }),
    evaluate: async (expression) => {
      const value = await call("POST", `/session/${id}/execute/sync`, { script: "return eval(arguments[0]);", args: [expression] });
      return value === null ? undefined : value;
    },
    close: async () => {
      try {
        await call("DELETE", `/session/${id}`);
      } catch {}
      process_.kill();
    },
  };
}

let browser = null;
try {
  browser = await (kind === "firefox" ? firefox() : chrome());
  const { navigate, evaluate } = browser;
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
  const registers = await evaluate(`document.querySelector('[data-pattern="processes/{pid}/threads/{tid}/registers"] .body').textContent`);
  check("the registers panel shows rip", /rip0x[0-9a-f]+/.test(registers), registers.slice(0, 120));
  const maps = await evaluate(`document.getElementById("maps").textContent`);
  check("the memory map panel is filled", /r-xp|rw-p/.test(maps), maps.slice(0, 80));
  const processes = await evaluate(`document.querySelector('[data-pattern="processes"] .body').textContent`);
  check("the processes panel names pid 1", /pid 1/.test(processes), processes.slice(0, 120));
  const panelPaths = await evaluate(`Array.from(document.querySelectorAll(".panel[data-pattern] .path")).map((p) => p.textContent).join(" ")`);
  check("every panel is titled by the path it reads", /processes\/\d+\/threads\/\d+\/registers/.test(panelPaths) && /processes\/\d+\/files/.test(panelPaths) && /\bnet\b/.test(panelPaths), panelPaths);
  const entries = await evaluate(`Array.from(document.querySelectorAll('[data-pattern="processes/{pid}/files/{path}"] .entry a')).map((a) => a.textContent).join(",")`);
  check("the files panel lists the root directory", /\binit\b/.test(entries), entries);
  await evaluate(`window.zaqaruDebug.browse("init")`);
  await until(`!window.zaqaruDebug.busy && window.zaqaruDebug.context.path === "init"`);
  const fileView = await evaluate(`document.querySelector('[data-pattern="processes/{pid}/files/{path}"] .body').textContent`);
  check("browsing to the program shows it as a file", /file · [\d,]+ bytes/.test(fileView) && /7f 45 4c 46/.test(fileView), fileView.slice(0, 160));
  await evaluate(`window.zaqaruDebug.browse("")`);
  await until(`!window.zaqaruDebug.busy && window.zaqaruDebug.context.path === ""`);
  const layout = await evaluate(`window.zaqaruDebug.read("layout").then((v) => JSON.stringify(v))`);
  check("the path bar reads any path at the instant", /guest_block/.test(layout), layout);
  const rawShown = await evaluate(`(() => { const panel = document.querySelector('[data-pattern="net"]'); panel.querySelector(".rawtoggle").click(); return panel.classList.contains("raw") && panel.querySelector(".raw").textContent.includes("sockets"); })()`);
  check("a panel toggles to the raw value", rawShown === true, String(rawShown));
  const disassembled = await evaluate("window.zaqaruDebug.disassembled");
  const ripLine = await evaluate(`document.querySelector(".line.rip")?.textContent ?? ""`);
  check("the disassembly panel shows the instructions at rip", disassembled > 1 && /0x[0-9a-f]+/.test(ripLine), `${disassembled} lines; ${ripLine.slice(0, 80)}`);
  const stack = await evaluate(`document.querySelectorAll("#stack .word").length`);
  check("the stack panel shows the words under rsp", stack === 16, String(stack));

  const rows = await evaluate(`document.querySelectorAll(".syscall").length`);
  check("the syscall log has rows", rows > 10, String(rows));
  const exchangeRows = await evaluate(`document.querySelectorAll(".event.exchange").length`);
  check("the timeline shows the exchanges with the host beside the syscalls", exchangeRows > 3, String(exchangeRows));
  const exchangeText = await evaluate(`document.querySelector(".event.exchange")?.textContent ?? ""`);
  check("an exchange row names its path and what crossed", /\/iso\/\S+ [→←]/.test(exchangeText), exchangeText.slice(0, 100));
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
  // Pin an instant, seek on within the same process, and the panels say
  // what changed.
  await evaluate("window.zaqaruDebug.seek(777777)");
  await until(`!window.zaqaruDebug.busy && window.zaqaruDebug.current === 777777`);
  await evaluate("window.zaqaruDebug.pin()");
  await evaluate("window.zaqaruDebug.seek(1777777)");
  await until(`!window.zaqaruDebug.busy && window.zaqaruDebug.current === 1777777`);
  const changes = await evaluate(`document.querySelector('[data-pattern="processes/{pid}/threads/{tid}/registers"] .diff').textContent`);
  check("a pinned instant diffs against the present", /since 777,777: \d+ changes/.test(changes) && /~ retired:/.test(changes), changes.slice(0, 160));
  const unchanged = await evaluate(`document.querySelector('[data-pattern="layout"] .diff').textContent`);
  check("and says so where nothing changed", /unchanged/.test(unchanged), unchanged);
  await evaluate("window.zaqaruDebug.pin()");
  }

  if (runs("live")) {
  // Live: the server module, run against the page's own clock, with a
  // request sent through the edge box.
  await navigate(`http://127.0.0.1:${port}/web/?module=fixture/server.wasm&live=8080`);
  await until(`document.readyState === "complete" && !!window.zaqaruDebug && window.zaqaruDebug.live === true`);
  await until(`!window.zaqaruDebug.busy && document.getElementById("status").textContent.includes("send the request")`);
  await evaluate(`document.getElementById("request").value = "ping\\n"; document.getElementById("send").click()`);
  const response = await until(`(() => { const r = document.getElementById("responses").textContent; return /pong|not published|error/.test(r) ? r : ""; })()`, 120000);
  check("a request through the edge is answered by the server in the container", /answered at [\d,]+.*pong/s.test(response), response.slice(0, 200));
  // The first answer ends the opening state and stands the machine on the
  // instant the request came in.
  const wasOpening = await evaluate("window.zaqaruDebug.opening");
  check("the page opened on the edge box alone and left it at the first answer", wasOpening === false, String(wasOpening));
  await until(`!window.zaqaruDebug.busy && window.zaqaruDebug.pending === null`);
  const arrivalRow = await evaluate(`document.querySelector(".event.now")?.textContent ?? ""`);
  check("the machine stands on the accept that took the connection", /accept/.test(arrivalRow), arrivalRow.slice(0, 120));
  const spans = await evaluate("window.zaqaruDebug.spans.length");
  check("the request's span is marked", spans === 1, String(spans));
  const liveNames = await evaluate("JSON.stringify(window.zaqaruDebug.names)");
  check("the process is named by what it was started from", /"1":"init"/.test(liveNames), liveNames);
  const who = await evaluate(`document.querySelector(".event.syscall .who")?.textContent ?? ""`);
  check("timeline rows carry the process's name", who === "init", JSON.stringify(who));
  const lanes = await evaluate(`(() => { const c = document.getElementById("lanes"); return c.width > 0 && c.height > 0; })()`);
  check("the lane strip is drawn", lanes === true, String(lanes));
  // Then let it run on to its exit.
  await evaluate(`document.getElementById("status").textContent.includes("exited") || document.getElementById("play").click()`);
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
  await navigate(`http://127.0.0.1:${port}/web/?module=fixture/server.wasm&snapshot=fixture/server.snapshot&live=8080`);
  await until(`document.readyState === "complete" && !!window.zaqaruDebug && window.zaqaruDebug.live === true && window.zaqaruDebug.origin > 0`);
  await until(`!window.zaqaruDebug.busy && document.getElementById("status").textContent.includes("send the request")`);
  const origin = await evaluate("window.zaqaruDebug.origin");
  const snapshotStatus = await evaluate(`document.getElementById("status").textContent`);
  check("a snapshot loads listening", origin > 100000 && /from a snapshot/.test(snapshotStatus) && /listening on 8080/.test(snapshotStatus), snapshotStatus);
  const bootOutput = await evaluate(`document.getElementById("stdout").textContent`);
  check("the boot's console came through the file", bootOutput === "listening on 8080\n", JSON.stringify(bootOutput));
  await evaluate(`window.zaqaruDebug.send(8080, "ping\\n")`);
  const snapshotResponse = await until(`(() => { const r = document.getElementById("responses").textContent; return /pong|not published|error/.test(r) ? r : ""; })()`, 120000);
  await until(`!window.zaqaruDebug.busy && window.zaqaruDebug.pending === null`);
  await evaluate(`document.getElementById("status").textContent.includes("exited") || document.getElementById("play").click()`);
  check("the server continued from the file answers", /pong/.test(snapshotResponse), snapshotResponse.slice(0, 200));
  await until(`document.getElementById("status").textContent.includes("exited 0")`, 60000);
  await until(`!window.zaqaruDebug.busy`);
  const snapshotFrontier = await evaluate("window.zaqaruDebug.frontier");
  const between = Math.floor((origin + snapshotFrontier) / 2);
  await evaluate(`window.zaqaruDebug.seek(${between})`);
  await until(`!window.zaqaruDebug.busy && window.zaqaruDebug.current === ${between}`);
  const betweenRegisters = await evaluate(`document.querySelector('[data-pattern="processes/{pid}/threads/{tid}/registers"] .body').textContent`);
  check("seeking between the file's instant and the frontier re-executes", /rip0x[0-9a-f]+/.test(betweenRegisters), betweenRegisters.slice(0, 80));
  await evaluate(`window.zaqaruDebug.seek(0)`);
  await until(`!window.zaqaruDebug.busy`);
  const clamped = await evaluate("window.zaqaruDebug.current");
  check("history begins at the file's instant", clamped === origin, `${clamped} vs ${origin}`);
  }

  // The demo itself, when it has been made: nginx, gunicorn and Django,
  // booted, answering a request from the page.
  if (demo && runs("django")) {
    await navigate(`http://127.0.0.1:${port}/web/?module=demo/hello-django.wasm&snapshot=demo/hello-django.snapshot&live=80`);
    await until(`document.readyState === "complete" && !!window.zaqaruDebug && window.zaqaruDebug.live === true && window.zaqaruDebug.origin > 0`, 180000);
    await until(`!window.zaqaruDebug.busy && document.getElementById("status").textContent.includes("send the request")`, 60000);
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
    const djangoRegisters = await evaluate(`document.querySelector('[data-pattern="processes/{pid}/threads/{tid}/registers"] .body').textContent`);
    const djangoProcesses = await evaluate(`document.querySelector('[data-pattern="processes"] .body').textContent`);
    check("seeking into the request re-executes to the instant", /rip0x[0-9a-f]+/.test(djangoRegisters) && /pid 4|pid 3|pid 2|pid 1/.test(djangoProcesses), djangoRegisters.slice(0, 80));
    console.log(`     seek to ${middle.toLocaleString()} took ${((Date.now() - seekStarted) / 1000).toFixed(1)} s; ${await evaluate(`document.getElementById("at").textContent`)}`);
    const djangoDisassembly = await evaluate("window.zaqaruDebug.disassembled");
    check("the disassembly panel reads django's code", djangoDisassembly > 1, String(djangoDisassembly));
    const djangoExchanges = await evaluate("window.zaqaruDebug.exchanges");
    check("the request left exchanges with the host on the timeline", djangoExchanges > 5, String(djangoExchanges));
    const rxRow = await evaluate(`Array.from(document.querySelectorAll(".event.exchange")).map((r) => r.textContent).find((t) => t.includes("/iso/net/conn/")) ?? ""`);
    check("the request's bytes crossing the edge are on the timeline", /\/iso\/net\/conn\/\d+\/(rx|tx)/.test(rxRow), rxRow.slice(0, 120));
    const djangoNet = await evaluate(`document.querySelector('[data-pattern="net"] .body').textContent`);
    check("the net panel shows nginx listening on 80", /listening on 0\.0\.0\.0:80 /.test(djangoNet), djangoNet.slice(0, 200));
    const djangoFiles = await evaluate(`Array.from(document.querySelectorAll('[data-pattern="processes/{pid}/files/{path}"] .entry a')).map((a) => a.textContent).join(",")`);
    check("the files panel lists the image's root", /\betc\b/.test(djangoFiles) && /\busr\b/.test(djangoFiles), djangoFiles.slice(0, 200));
    const djangoDescriptors = await evaluate(`document.querySelector('[data-pattern="processes/{pid}/descriptors"] .body').textContent`);
    check("descriptors are named by the path they were opened by", /\/(var|dev|proc|app|usr|etc)\//.test(djangoDescriptors), djangoDescriptors.slice(0, 300));
    await evaluate(`window.zaqaruDebug.browse("etc/nginx")`);
    await until(`!window.zaqaruDebug.busy && window.zaqaruDebug.context.path === "etc/nginx"`, 120000);
    const nginxDir = await evaluate(`Array.from(document.querySelectorAll('[data-pattern="processes/{pid}/files/{path}"] .entry a')).map((a) => a.textContent).join(",")`);
    check("browsing reaches /etc/nginx", /nginx\.conf/.test(nginxDir), nginxDir.slice(0, 200));
    await evaluate(`window.zaqaruDebug.browse("etc/nginx/nginx.conf")`);
    await until(`!window.zaqaruDebug.busy && window.zaqaruDebug.context.path === "etc/nginx/nginx.conf"`, 120000);
    const nginxConf = await evaluate(`document.querySelector('[data-pattern="processes/{pid}/files/{path}"] .body').textContent`);
    check("and reads nginx.conf as text", /worker_processes|http \{|server \{/.test(nginxConf), nginxConf.slice(0, 200));
    const djangoNames = await evaluate("JSON.stringify(Object.values(window.zaqaruDebug.names))");
    check("the processes are named nginx and gunicorn", /nginx/.test(djangoNames) && /gunicorn/.test(djangoNames), djangoNames);
    const open = await evaluate(`document.querySelectorAll("#panels .panel").length`);
    const internals = await evaluate(`document.querySelectorAll("#internals .panel").length`);
    check("six panels stand in the open and the internals are folded away", open === 6 && internals >= 4, `${open} open, ${internals} internal`);
    // A path in a syscall row is a link into the files panel.
    const linked = await evaluate(`(() => { const a = document.querySelector(".event.syscall a.link.file"); if (!a) return null; a.click(); return a.dataset.path; })()`);
    check("a syscall row's path is a link", typeof linked === "string" && linked.startsWith("/"), String(linked));
    await until(`!window.zaqaruDebug.busy && window.zaqaruDebug.pending === null && window.zaqaruDebug.context.path === ${JSON.stringify((linked ?? "/").replace(/^\/+/, ""))}`, 120000);
    const focusedFiles = await evaluate(`document.querySelector('.panel.focused[data-pattern="processes/{pid}/files/{path}"]') !== null`);
    check("clicking it browses there and lights the files panel", focusedFiles === true, String(focusedFiles));
    // A descriptor in a syscall row is a link to what it names.
    const fdLinked = await evaluate(`(() => { const a = Array.from(document.querySelectorAll(".event.syscall a.link.fd")).find((a) => Number(a.dataset.fd) > 2); if (!a) return null; a.click(); return Number(a.dataset.fd); })()`);
    check("a syscall row's descriptor is a link", typeof fdLinked === "number", String(fdLinked));
    await until(`!window.zaqaruDebug.busy && window.zaqaruDebug.pending === null`, 120000);
    const focusedSomething = await evaluate(`document.querySelector(".panel.focused")?.dataset.pattern ?? document.querySelector(".panel.focused")?.id ?? ""`);
    check("clicking it lights the panel that holds what it names", /files|descriptors|net/.test(focusedSomething), focusedSomething);
  } else if (runs("django")) console.log("     (no web/demo/hello-django.snapshot: the django scenario is skipped; make it with web/demo.sh)");
} catch (why) {
  console.log("FAIL " + why);
  failures++;
} finally {
  if (consoleLines.length) console.log("console:\n  " + consoleLines.join("\n  "));
  if (browser) await browser.close();
  server.close();
}
console.log(failures ? `${failures} failure(s)` : "all passed");
process.exit(failures ? 1 : 0);
