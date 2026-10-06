// One sign-in "world" per test: a real datalib-http on an empty root, the
// real latchkey CLI on a temp store, and a fake internet in place of the
// third-party sites. Plain JS because it needs Node's APIs and the UI
// package carries no Node types; README.md beside it says how the fakes
// plug into latchkey.
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import {
  chmodSync,
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import https from "node:https";
import net from "node:net";
import path from "node:path";
import { chromium } from "@playwright/test";
import { FAKE_SITES } from "./fake_sites.mjs";

export const API_TOKEN = "e2e-auth";

function need(name) {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is unset; run this suite through //datalib/ui:e2e_auth`);
  return value;
}

const scratch = () => need("DATALIB_TEST_AUTH_SCRATCH");

function freshDir(prefix) {
  mkdirSync(scratch(), { recursive: true });
  return mkdtempSync(path.join(scratch(), prefix));
}

function executable(file, text) {
  writeFileSync(file, text);
  chmodSync(file, 0o755);
  return file;
}

const shellQuote = (s) => `'${String(s).replace(/'/g, `'\\''`)}'`;

// ── the fake internet ────────────────────────────────────────────────

let certCache = null;

/// A self-signed certificate for every host. Nothing checks it: the curl
/// shim passes `-k` and the browser `--ignore-certificate-errors`.
function cert() {
  if (certCache) return certCache;
  const dir = freshDir("cert-");
  execFileSync(
    "openssl",
    [
      "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2", "-subj", "/CN=fake-internet",
      "-keyout", path.join(dir, "key.pem"), "-out", path.join(dir, "cert.pem"),
    ],
    { stdio: "ignore" },
  );
  certCache = {
    key: readFileSync(path.join(dir, "key.pem")),
    cert: readFileSync(path.join(dir, "cert.pem")),
  };
  return certCache;
}

function readBody(req) {
  return new Promise((resolve) => {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
  });
}

/// One TLS server answering for every hostname, by `Host`. Each request
/// is kept in `requests` so a spec can say what reached "slack.com".
/// Anything no fake site answers is a 404 — Chrome's own background
/// traffic lands here too, which is how we know none of it left the
/// machine.
export async function startFakeInternet(sites = FAKE_SITES) {
  const requests = [];
  /** @type {{ matches: (r: any) => boolean, released: Promise<void> }[]} */
  const holds = [];
  /** @type {Map<string, (r: any) => any>} */
  const overrides = new Map();
  const server = https.createServer(cert(), async (req, res) => {
    const host = (req.headers.host ?? "").replace(/:\d+$/, "");
    const url = new URL(req.url ?? "/", `https://${host}`);
    const body = await readBody(req);
    const seen = { host, method: req.method, path: url.pathname, query: url.search, headers: req.headers, body };
    requests.push(seen);
    for (const hold of holds.filter((h) => h.matches(seen))) await hold.released;
    const site = overrides.get(host) ?? sites[host];
    const reply = (await site?.(seen)) ?? {
      status: 404,
      json: { error: `no fake for ${host}${url.pathname}` },
    };
    const headers = { ...(reply.headers ?? {}) };
    let payload = reply.text ?? "";
    if (reply.json !== undefined) {
      headers["content-type"] = "application/json";
      payload = JSON.stringify(reply.json);
    } else if (reply.html !== undefined) {
      headers["content-type"] = "text/html";
      payload = reply.html;
    }
    res.writeHead(reply.status ?? 200, headers);
    res.end(payload);
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = server.address().port;
  return {
    port,
    requests,
    /// Keep every request `matches` accepts unanswered until the
    /// returned function is called — so a spec can look at a page while
    /// a list is half loaded, rather than racing it.
    /// Answer every request to `host` with `handler` instead of its
    /// fake site — how a spec makes one service misbehave.
    override: (host, handler) => overrides.set(host, handler),
    hold: (matches) => {
      let release = () => {};
      const released = new Promise((resolve) => (release = resolve));
      holds.push({ matches, released });
      return release;
    },
    /// Requests that reached `host`, optionally on one path.
    to: (host, pathname) =>
      requests.filter((r) => r.host === host && (pathname === undefined || r.path === pathname)),
    close: () => new Promise((resolve) => server.close(() => resolve())),
  };
}

// ── what stands in for the network, the browser and node ─────────────

/// `LATCHKEY_CURL`: the real `latchkey-curl-router` — and through it
/// curl-impersonate, for the hosts datalib marks — with every host sent
/// to the fake internet. latchkey still sees the real URL, so its own
/// service definitions match and inject as they would in production.
/// The argv is logged, so a spec can read exactly what latchkey injected.
function curlShim(dir, port) {
  const router = need("DATALIB_TEST_AUTH_CURL_ROUTER");
  const log = path.join(dir, "curl.log");
  const file = executable(
    path.join(dir, "curl"),
    `#!/bin/sh\nprintf '%s\\n' "$*" >> ${shellQuote(log)}\n` +
      `exec ${shellQuote(router)} --connect-to ::127.0.0.1:${port} -k "$@"\n`,
  );
  return { file, log };
}

/// Where Playwright keeps its browsers for the person running the suite,
/// so the real `ensure-browser` can find one there under a world's HOME.
function playwrightBrowsers() {
  if (process.env.PLAYWRIGHT_BROWSERS_PATH) return process.env.PLAYWRIGHT_BROWSERS_PATH;
  const home = process.env.HOME ?? "";
  return process.platform === "darwin"
    ? path.join(home, "Library", "Caches", "ms-playwright")
    : path.join(home, ".cache", "ms-playwright");
}

const realNode = () => need("DATALIB_TEST_AUTH_REAL_NODE");
const latchkeyCli = () => need("DATALIB_TEST_AUTH_LATCHKEY_CLI");

/// One line per latchkey the backend ran, as the fake node logged it:
/// `{args, gateway}`. `args` has the cli.js path dropped.
function readSpy(file) {
  if (!existsSync(file)) return [];
  return readFileSync(file, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
}

/// A port that was free a moment ago and has nothing listening on it.
const closedPort = () => freePort();

function freePort() {
  return new Promise((resolve, reject) => {
    const probe = net.createServer();
    probe.once("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const { port } = probe.address();
      probe.close(() => resolve(port));
    });
  });
}

async function poll(what, deadlineMs, check) {
  const deadline = Date.now() + deadlineMs;
  for (;;) {
    const value = await check();
    if (value) return value;
    if (Date.now() >= deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
}

/// What a latchkey run in this world needs in its environment, for the
/// store at `dir`.
function storeEnv(dir, key) {
  return {
    LATCHKEY_DIRECTORY: dir,
    LATCHKEY_ENCRYPTION_KEY: key,
    LATCHKEY_DISABLE_COUNTING: "1",
  };
}

/// The host's environment minus anything that would point a process at
/// the developer's own latchkey, runtime or data.
function hostEnv() {
  const env = {};
  for (const [k, v] of Object.entries(process.env)) {
    if (k.startsWith("LATCHKEY_") || k.startsWith("MINDS_")) continue;
    if (k.startsWith("DATALIB_")) continue;
    env[k] = v;
  }
  return env;
}

function tryLatchkey(env, args) {
  const out = spawnSync(realNode(), [latchkeyCli(), ...args], {
    env: { ...hostEnv(), ...env },
    encoding: "utf8",
  });
  return { status: out.status, stdout: out.stdout, stderr: out.stderr };
}

function runLatchkey(env, args) {
  const out = tryLatchkey(env, args);
  if (out.status !== 0) {
    throw new Error(`latchkey ${args.join(" ")} exited ${out.status}: ${out.stderr}`);
  }
  return out.stdout;
}

// ── a gateway, the shape Minds runs datalib in ───────────────────────

/// A real `latchkey gateway` holding its own store, reaching the fake
/// internet. `seed` runs before it starts, as `latchkey` argv lists.
export async function startGateway(internet, options) {
  const { seed = [] } = options ?? {};
  const dir = freshDir("gateway-");
  const store = path.join(dir, "latchkey");
  mkdirSync(store, { mode: 0o700 });
  const env = {
    ...storeEnv(store, randomBytes(32).toString("base64")),
    LATCHKEY_CURL: curlShim(dir, internet.port).file,
  };
  for (const args of seed) runLatchkey(env, args);
  const port = await freePort();
  const password = randomBytes(12).toString("hex");
  const log = path.join(dir, "gateway.log");
  const child = spawn(
    realNode(),
    [latchkeyCli(), "gateway", "--host", "127.0.0.1", "--port", String(port)],
    {
      env: { ...hostEnv(), ...env, LATCHKEY_GATEWAY_LISTEN_PASSWORD: password },
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  const output = [];
  child.stdout.on("data", (c) => output.push(String(c)));
  child.stderr.on("data", (c) => output.push(String(c)));
  const url = `http://127.0.0.1:${port}`;
  await poll(`the latchkey gateway at ${url} (${log})`, 30_000, () =>
    fetch(`${url}/`, { headers: { "X-Latchkey-Gateway-Password": password } })
      .then((r) => r.ok)
      .catch(() => false),
  );
  return {
    url,
    password,
    stop: async () => {
      child.kill("SIGTERM");
      writeFileSync(log, output.join(""));
    },
  };
}

// ── the world ────────────────────────────────────────────────────────

/// Start a world. Options:
/// - `gateway`: a `startGateway` result; the backend then holds no
///   store of its own, the way it runs under Minds.
/// - `browser`: `"found"` (latchkey's own discovery finds one),
///   `"download"` (it finds none, and the download fetches one once
///   `releaseDownload()` is called) or `"none"` (the download fails too).
/// - `runtime: false`: no bundled runtime at all.
/// - `offline: true`: no network — every host refuses the connection.
export async function startWorld(internet, options) {
  const { gateway = null, browser = "found", runtime = true, offline = false } = options ?? {};
  const dir = freshDir("world-");
  const store = path.join(dir, "latchkey");
  mkdirSync(store, { mode: 0o700 });
  const root = path.join(dir, "root");
  mkdirSync(root);
  const home = path.join(dir, "home");
  mkdirSync(home);
  const key = randomBytes(32).toString("base64");
  // Offline: every host is a port nothing listens on.
  const curl = curlShim(dir, offline ? await closedPort() : internet.port);
  const spyLog = path.join(dir, "latchkey-runs.jsonl");

  const env = {
    ...hostEnv(),
    // A HOME of its own, so anything that falls back to `~` writes here.
    HOME: home,
    DATALIB_BIND: "127.0.0.1:0",
    DATALIB_TOKEN: API_TOKEN,
    DATALIB_PARENT_PIPE: "0",
    DATALIB_STEP_BIN: need("DATALIB_STEP_BIN"),
    DATALIB_RUNTIME_DIR: runtime ? need("DATALIB_TEST_AUTH_RUNTIME_DIR") : path.join(dir, "no-runtime"),
    DATALIB_CACHE_DIR: path.join(dir, "cache"),
    DATALIB_TEST_AUTH_SPY_LOG: spyLog,
  };
  if (gateway) {
    // No store, no key and no curl override: the client hands every
    // request to the gateway, which holds all three.
    Object.assign(env, {
      LATCHKEY_DIRECTORY: store,
      LATCHKEY_DISABLE_COUNTING: "1",
      LATCHKEY_GATEWAY: gateway.url,
      LATCHKEY_GATEWAY_PASSWORD: gateway.password,
    });
  } else {
    Object.assign(env, storeEnv(store, key), { LATCHKEY_CURL: curl.file });
  }
  // The real `ensure-browser` runs; fake_node.mjs then wraps what it
  // found so it runs headless against the fake internet.
  env.PLAYWRIGHT_BROWSERS_PATH = playwrightBrowsers();
  env.DATALIB_TEST_AUTH_BROWSER_WRAPPER = path.join(dir, "browser");
  env.DATALIB_TEST_AUTH_FAKE_PORT = String(internet.port);
  const downloadGate = path.join(dir, "download-gate");
  if (browser !== "found") env.DATALIB_TEST_AUTH_NO_BROWSER = "1";
  if (browser === "download") {
    env.DATALIB_TEST_AUTH_DOWNLOADS = chromium.executablePath();
    env.DATALIB_TEST_AUTH_DOWNLOAD_GATE = downloadGate;
  }
  // latchkey refuses a browser login on Linux with no display named,
  // though the headless browser above never opens one.
  if (process.platform === "linux" && !env.DISPLAY && !env.WAYLAND_DISPLAY) env.DISPLAY = ":0";

  const urlFile = path.join(dir, "url");
  const log = path.join(dir, "datalib-http.log");
  const out = [];
  const child = spawn(need("DATALIB_HTTP_BIN"), [root, "--no-open", "--url-file", urlFile], {
    env,
    stdio: ["pipe", "pipe", "pipe"],
  });
  child.stdout.on("data", (c) => out.push(String(c)));
  child.stderr.on("data", (c) => out.push(String(c)));
  const flushLog = () => writeFileSync(log, out.join(""));

  let origin;
  try {
    origin = await poll(`datalib-http to announce a URL (${log})`, 30_000, () => {
      try {
        return new URL(readFileSync(urlFile, "utf8").trim()).origin;
      } catch {
        return undefined;
      }
    });
    const auth = { authorization: `Bearer ${API_TOKEN}` };
    await poll(`datalib-http /api/health (${log})`, 60_000, () =>
      fetch(`${origin}/api/health`, { headers: auth })
        .then((r) => r.ok)
        .catch(() => false),
    );
    const init = await fetch(`${origin}/api/config/init`, { method: "POST", headers: auth });
    if (!init.ok) throw new Error(`POST /api/config/init: HTTP ${init.status}`);
  } catch (e) {
    flushLog();
    child.kill("SIGKILL");
    throw e;
  }

  return {
    origin,
    dir,
    gatewayUrl: gateway?.url ?? null,
    /// Run the real latchkey CLI against this world's store, the way a
    /// person sets one up in a terminal before ever opening the app.
    latchkey: (...args) => runLatchkey(storeEnv(store, key), args),
    /// The same, for a run that is expected to fail: its exit status and output.
    latchkeyTry: (...args) => tryLatchkey(storeEnv(store, key), args),
    /// Where latchkey keeps its plugins in this world.
    pluginsDir: path.join(store, "plugins"),
    /// Write a file under the world's directory; returns its path.
    writeFile: (rel, text) => {
      const file = path.join(dir, rel);
      mkdirSync(path.dirname(file), { recursive: true });
      writeFileSync(file, text);
      return file;
    },
    readFile: (file) => (existsSync(file) ? readFileSync(file, "utf8") : null),
    /// The Garmin plugin, put in place the way a person with their own
    /// clone would have it: no stamp of datalib's.
    installOwnGarminPlugin: () => {
      const to = path.join(store, "plugins", "garmin");
      cpSync(need("DATALIB_TEST_AUTH_GARMIN_PLUGIN"), to, { recursive: true, dereference: true });
      return to;
    },
    /// Every latchkey the backend (or a step it spawned) ran so far.
    latchkeyRuns: () => readSpy(spyLog),
    /// Let the stood-in browser download finish.
    releaseDownload: () => writeFileSync(downloadGate, ""),
    /// The browser latchkey's own `ensure-browser` found, if it ran.
    browserFound: () => {
      const file = `${env.DATALIB_TEST_AUTH_BROWSER_WRAPPER}.found`;
      return existsSync(file) ? readFileSync(file, "utf8") : null;
    },
    /// Every argv the curl shim was handed so far.
    curlCalls: () => (existsSync(curl.log) ? readFileSync(curl.log, "utf8").split("\n").filter(Boolean) : []),
    stop: async () => {
      child.stdin.end();
      child.kill("SIGTERM");
      await new Promise((resolve) => {
        if (child.exitCode !== null) return resolve();
        const timer = setTimeout(() => {
          child.kill("SIGKILL");
          resolve();
        }, 5_000);
        child.once("exit", () => {
          clearTimeout(timer);
          resolve();
        });
      });
      flushLog();
    },
    cleanup: () => rmSync(dir, { recursive: true, force: true }),
  };
}
