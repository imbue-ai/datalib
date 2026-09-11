import { defineConfig } from "@playwright/test";
import { execFileSync, spawn, type ChildProcess } from "node:child_process";
import {
  closeSync,
  copyFileSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readFileSync,
  rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { CONFIG_MUTATING } from "./tests/e2e/config-mutating";

// Materialize the bazel-built fixture once, before any worker starts.
// Tests share the resulting data root via FW_E2E_FIXTURE_ROOT — cached
// in env so worker subprocesses (which re-import this config) don't
// each rebuild the fixture into a fresh temp dir.
const here = path.dirname(fileURLToPath(import.meta.url));
// `datalib/ui/..` — the workspace root, for the `bazel-bin/...`
// fallbacks used when this config is loaded outside bazel.
const workspaceDir = path.resolve(here, "..", "..");
// run_e2e.sh hands us one scratch dir per run and prunes old ones, so
// the roots below land somewhere bounded. Bare `tmpdir()` is the
// `pnpm exec playwright test` path, where nobody reclaims them at all:
// bazel exports TEST_TMPDIR but never TMPDIR.
const scratchParent =
  process.env.FW_E2E_RUN_DIR || process.env.TEST_TMPDIR || tmpdir();
function mintRoot(prefix: string): string {
  return mkdtempSync(path.join(scratchParent, prefix));
}
function materializeRoot(prefix: string): string {
  const materializer =
    process.env.FW_E2E_MATERIALIZE_TNG_ROOT ||
    path.join(workspaceDir, "bazel-bin/tests/fixtures/materialize_tng_root");
  const root = mintRoot(prefix);
  execFileSync(materializer, [root], { stdio: "inherit" });
  return root;
}
function ensureFixtureRoot(): string {
  const existing = process.env.FW_E2E_FIXTURE_ROOT;
  if (existing) return existing;
  const root = materializeRoot("datalib-e2e-");
  process.env.FW_E2E_FIXTURE_ROOT = root;
  return root;
}
const fixtureRoot = ensureFixtureRoot();

  // A second backend on an empty data root, for the first-run onboarding spec.
  // It has to be its own server: the onboarding screen is gated on the root
  // having no `config.toml`, and there is no way back to that state from a
  // populated one.
  //
  // Fresh `mkdtemp` per config load, so the spec that initializes it still
  // sees an uninitialized root next run. Cached in env because worker
  // subprocesses re-import this file and must not mint a second directory.
function emptyRoot(): string {
  const existing = process.env.FW_E2E_EMPTY_ROOT;
  if (existing) return existing;
  const root = mintRoot("datalib-e2e-empty-");
  process.env.FW_E2E_EMPTY_ROOT = root;
  return root;
}
const EMPTY_ROOT = emptyRoot();

  // A third backend on a third empty root: `first-run.spec.ts` already owns
  // EMPTY_ROOT and initializes it, and onboarding is one-shot — a root with a
  // config can never go back to having none.
function onboardingRoot(): string {
  const existing = process.env.FW_E2E_ONBOARDING_ROOT;
  if (existing) return existing;
  const root = mintRoot("datalib-e2e-onboarding-");
  process.env.FW_E2E_ONBOARDING_ROOT = root;
  return root;
}
const ONBOARDING_ROOT = onboardingRoot();

const binDir =
  process.env.FW_E2E_BIN_DIR ||
  path.join(workspaceDir, "bazel-bin/datalib/backend/bin");

// The tree the onboarding spec points its PDF source at. Built here
// rather than in the spec so the spec never has to touch the
// filesystem: it is a *copy* of part of the checked-in corpus, because
// the spec adds a file to it partway through and the corpus itself is a
// bazel input shared with every other test.
const PDF_CORPUS =
  process.env.FW_E2E_PDF_FIXTURE_DIR ||
  path.join(workspaceDir, "datalib/backend/etl/providers/pdf/tests/fixtures/pdf_tng");
process.env.FW_E2E_PDF_LATECOMER = path.join(
  PDF_CORPUS,
  "engineering/warp_core_manual.pdf",
);
function pdfScanDir(): string {
  const existing = process.env.FW_E2E_PDF_SCAN_DIR;
  if (existing) return existing;
  const dir = mintRoot("datalib-e2e-pdfs-");
  for (const f of ["captains_log.pdf", "captains_log_v2.pdf"]) {
    copyFileSync(path.join(PDF_CORPUS, f), path.join(dir, f));
  }
  process.env.FW_E2E_PDF_SCAN_DIR = dir;
  return dir;
}
pdfScanDir();

// The Signal backup the onboarding spec's second source points at.
export const FIXTURE_SIGNAL_AEP = "0".repeat(64);
const SIGNAL_MAKE_FIXTURE = process.env.FW_E2E_SIGNAL_MAKE_FIXTURE;
const SIGNAL_SPEC = process.env.FW_E2E_SIGNAL_SPEC;
function signalBackupDir(): string | undefined {
  const existing = process.env.FW_E2E_SIGNAL_BACKUP_DIR;
  if (existing) return existing;
  // Absent outside bazel (`pnpm exec playwright test` straight from the
  // source tree). The spec skips its Signal half rather than failing,
  // the same way the sync spec skips without its step binary.
  if (!SIGNAL_MAKE_FIXTURE || !SIGNAL_SPEC) return undefined;
  const dir = mintRoot("datalib-e2e-signal-");
  execFileSync(SIGNAL_MAKE_FIXTURE, [SIGNAL_SPEC, dir], { stdio: "pipe" });
  process.env.FW_E2E_SIGNAL_BACKUP_DIR = dir;
  return dir;
}
signalBackupDir();

// ── the config-mutating specs, one data root each ────────────────────
type Sandbox = { spec: string; root: string; url: string };

/// Cached in env like the fixture root: worker subprocesses re-import
/// this config and must attach to what the parent materialized rather
/// than building their own. `url` is filled in below, once the backend
/// on this root has said which port it got.
function sandboxRoots(): Sandbox[] {
  const existing = process.env.FW_E2E_SANDBOXES;
  if (existing) return JSON.parse(existing) as Sandbox[];
  const made = CONFIG_MUTATING.map((spec) => ({
    spec,
    root: materializeRoot(`datalib-e2e-${spec}-`),
    url: "",
  }));
  process.env.FW_E2E_SANDBOXES = JSON.stringify(made);
  return made;
}
const SANDBOX_ROOTS = sandboxRoots();

  // The backend requires its API token on every route, so pin one via
  // DATALIB_TOKEN rather than reading back a random one. `use.extraHTTPHeaders`
  // then authenticates the `request` fixture and every navigation the browser
  // context issues, so the specs stay unaware that auth exists. Cached in env
  // because each worker subprocess re-imports this file.
function cachedToken(): string {
  const existing = process.env.DATALIB_TOKEN;
  if (existing) return existing;
  const token = `e2e-${process.pid}`;
  process.env.DATALIB_TOKEN = token;
  return token;
}
const API_TOKEN = cachedToken();

// Locate the bazel-built http binary. Built via:
//   bazelisk build //datalib/backend/http:datalib_http_bin
const backendBin =
  process.env.DATALIB_HTTP_BIN ||
  path.join(
    workspaceDir,
    "bazel-bin/datalib/backend/http/datalib_http_bin",
  );

// ── the backends ─────────────────────────────────────────────────────
// Every backend binds `127.0.0.1:0` and announces the port the kernel
// gave it through `--url-file`; nothing here picks one. A port picked in
// advance belongs to whoever binds it first, and the roots above take
// tens of seconds to materialize before any server starts, so there is
// no honest way to hold one. Binding is the only claim there is.
//
// The price is Playwright's `webServer`, which needs the URL before the
// server exists: spawning, readiness and teardown are ours instead.
// Readiness is the health poll in `tests/e2e/global-setup.ts` — the
// url-file lands right after `bind()`, several seconds before the
// backend finishes assembling. `start_backend` in
// `datalib/tauri/src/main.rs` is the same handshake for the desktop app.
type Server = { name: string; url: string; pid: number; log: string };
type Pending = { name: string; child: ChildProcess; urlFile: string; log: string };

const ANNOUNCE_TIMEOUT_MS = 30_000;

// Playwright's config module can't be async, so the wait for the
// announcements below is a blocking one.
function sleepSync(ms: number): void {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

function logTail(file: string, lines = 30): string {
  try {
    return readFileSync(file, "utf8").split("\n").slice(-lines).join("\n").trim();
  } catch {
    return "(no output)";
  }
}

function spawnBackend(
  name: string,
  root: string,
  env: Record<string, string> = {},
): Pending {
  const dir = path.join(scratchParent, "servers");
  mkdirSync(dir, { recursive: true });
  const urlFile = path.join(dir, `${name}.url`);
  const log = path.join(dir, `${name}.log`);
  rmSync(urlFile, { force: true });
  const fd = openSync(log, "w");
  const child = spawn(
    backendBin,
    [root, "--no-open", "--url-file", urlFile],
    {
      stdio: ["ignore", fd, fd],
      env: {
        ...process.env,
        DATALIB_BIND: "127.0.0.1:0",
        DATALIB_TOKEN: API_TOKEN,
        ...env,
      },
    },
  );
  closeSync(fd);
  return { name, child, urlFile, log };
}

// The announced URL is `<origin>/?token=<DATALIB_TOKEN>`; the specs want
// the origin. Absent, empty and short of the whole token all read the
// same way here — as "not yet", so a torn read is one more turn of the
// poll rather than a truncated port number that parses.
function announcedOrigin(urlFile: string): string | undefined {
  try {
    const url = new URL(readFileSync(urlFile, "utf8").trim());
    return url.searchParams.get("token") === API_TOKEN ? url.origin : undefined;
  } catch {
    return undefined;
  }
}

function awaitAnnouncements(pending: Pending[]): Server[] {
  const deadline = Date.now() + ANNOUNCE_TIMEOUT_MS;
  const urls = new Map<string, string>();
  for (;;) {
    for (const p of pending) {
      if (urls.has(p.name)) continue;
      const url = announcedOrigin(p.urlFile);
      if (url) urls.set(p.name, url);
    }
    if (urls.size === pending.length) break;
    if (Date.now() >= deadline) {
      const stuck = pending
        .filter((p) => !urls.has(p.name))
        .map((p) => `── ${p.name} (${p.log}):\n${logTail(p.log)}`)
        .join("\n\n");
      throw new Error(
        `datalib-http did not announce a URL within ` +
          `${ANNOUNCE_TIMEOUT_MS / 1000}s:\n\n${stuck}`,
      );
    }
    sleepSync(25);
  }
  return pending.map((p) => ({
    name: p.name,
    url: urls.get(p.name) as string,
    pid: p.child.pid as number,
    log: p.log,
  }));
}

/// Cached in env like everything else the parent process builds: a
/// worker re-importing this config must attach to the running backends,
/// not start a second set of its own.
function servers(): Server[] {
  const existing = process.env.FW_E2E_SERVERS;
  if (existing) return JSON.parse(existing) as Server[];
  const pending = [
    spawnBackend("fixture", fixtureRoot),
    spawnBackend("empty", EMPTY_ROOT),
      // The one server whose PATH carries the dash-named binaries. The
      // scaffold config names `datalib-dag`, its steps and the
      // `unified_index` applet bare, so PATH is how all three are found —
      // the installed-user arrangement, which the other two never exercise.
    spawnBackend("onboarding", ONBOARDING_ROOT, {
      PATH: `${binDir}${path.delimiter}${process.env.PATH ?? ""}`,
      // Signal's download step reads its passphrase from the
      // environment — the wizard writes the *name* of the variable,
      // never the secret. The step inherits this from the runner, which
      // inherits it from this server, which is the same chain a real
      // install has from the user's shell.
      SIGNAL_BACKUP_PASSPHRASE: FIXTURE_SIGNAL_AEP,
    }),
    ...SANDBOX_ROOTS.map((s) => spawnBackend(`sandbox-${s.spec}`, s.root)),
  ];
  let started: Server[];
  try {
    started = awaitAnnouncements(pending);
  } catch (err) {
    // globalTeardown never runs if this config throws, so the ones that
    // did come up have to be cleaned up here or they outlive the run.
    for (const p of pending) p.child.kill("SIGKILL");
    throw err;
  }
  process.env.FW_E2E_SERVERS = JSON.stringify(started);
  return started;
}
const SERVERS = servers();

function serverUrl(name: string): string {
  const found = SERVERS.find((s) => s.name === name);
  if (!found) throw new Error(`no backend named ${name}`);
  return found.url;
}

const BACKEND_URL = serverUrl("fixture");
const EMPTY_URL = serverUrl("empty");
process.env.FW_E2E_EMPTY_URL = EMPTY_URL;
const ONBOARDING_URL = serverUrl("onboarding");
process.env.FW_E2E_ONBOARDING_URL = ONBOARDING_URL;
const SANDBOXES: Sandbox[] = SANDBOX_ROOTS.map((s) => ({
  ...s,
  url: serverUrl(`sandbox-${s.spec}`),
}));
process.env.FW_E2E_SANDBOXES = JSON.stringify(SANDBOXES);

// ── where the recordings go ──────────────────────────────────────────
const REPORT_DIR = process.env.TEST_UNDECLARED_OUTPUTS_DIR
  ? path.join(process.env.TEST_UNDECLARED_OUTPUTS_DIR, "playwright-report")
  : path.join(here, "playwright-report");
const ARTIFACT_DIR = process.env.TEST_TMPDIR
  ? path.join(process.env.TEST_TMPDIR, "playwright-artifacts")
  : path.join(here, "test-results");

export default defineConfig({
  testDir: "tests/e2e",
  testMatch: /.*\.spec\.ts$/,
  // Files run in parallel; tests *within* a file keep their declaration
  // order. That is the right split: several specs are written as a
  // sequence (write the config, sync, assert on what the sync did),
  // while no two files share state any more — see `SANDBOXES`.
  fullyParallel: false,
  workers: 4,
  globalSetup: "./tests/e2e/global-setup.ts",
  globalTeardown: "./tests/e2e/global-teardown.ts",
  outputDir: ARTIFACT_DIR,
  // `list` is what a person watching the terminal reads. `html` is the
  // artifact: a self-contained report that embeds each test's video and
  // trace, with the trace viewer built in — open it and you can scrub
  // the run action by action, with a DOM snapshot before and after
  // each. See tests/e2e/README-artifacts.md for how to open one.
  reporter: [
    ["list"],
    ["html", { outputFolder: REPORT_DIR, open: "never" }],
  ],
    // Drop Playwright's default `-{projectName}-{platform}` snapshot suffix:
    // ours are text dumps of API payloads, identical on every OS. Screenshot
    // snapshots would legitimately differ per platform — opt those back in per
    // `toMatchSnapshot()` call.
  snapshotPathTemplate:
    "{snapshotDir}/{testFileDir}/{testFileName}-snapshots/{arg}{ext}",
  use: {
    baseURL: BACKEND_URL,
    headless: true,
    trace: "retain-on-failure",
    extraHTTPHeaders: { authorization: `Bearer ${API_TOKEN}` },
  },
  projects: [
    {
      // The qmd cold start, owned and named — see
      // tests/e2e/qmd-warmup.setup.ts for why it can't be left to
      // whichever spec sorts first. `testMatch` here overrides the
      // top-level `*.spec.ts` pattern, so this file and only this file
      // runs in the project.
      name: "warmup",
      testMatch: /qmd-warmup\.setup\.ts/,
      use: { browserName: "chromium" },
    },
    {
      name: "chromium",
      use: { browserName: "chromium" },
      dependencies: ["warmup"],
      // Everything that does not rewrite the config, against the one
      // shared read-only fixture root. The rest get a project apiece
      // below, pointed at a root of their own.
      testIgnore: CONFIG_MUTATING.map(
        (spec) => new RegExp(`${spec}\\.spec\\.ts`),
      ),
    },
    // One project per config-mutating spec, doing one job: pointing
    // `baseURL` at that spec's own backend, so the spec itself can go
    // on saying `page.goto("/sources2")`.
    ...SANDBOXES.map((s) => ({
      name: `chromium-${s.spec}`,
      testMatch: new RegExp(`${s.spec}\\.spec\\.ts`),
      use: { browserName: "chromium" as const, baseURL: s.url },
    })),
    {
        // The desktop app runs in a WKWebView, and WebKit's layout has twice
        // shipped an invisible AG Grid: it resolves a child's percentage
        // `height` against the parent's *specified* height, so `height: 100%`
        // under a flex-sized parent computes to `auto` and the grid collapses.
        // Rows stay in the DOM, so every count assertion passes while nothing
        // is painted — see `expectGridPainted`.
      name: "webkit",
      use: { browserName: "webkit" },
      dependencies: ["warmup"],
      testMatch: [
        // Explore / GridCard — the search grid.
        /grid-populated\.spec\.ts/,
        /grid-context-menu\.spec\.ts/,
        /contents-cell-clamp\.spec\.ts/,
        /row-click-scroll\.spec\.ts/,
        /row-msg-index-alignment\.spec\.ts/,
        /score-sort-order\.spec\.ts/,
        /search-qmd-routing\.spec\.ts/,
        /selected-message-outline\.spec\.ts/,
        /qmd-index-columns\.spec\.ts/,
        /url-sync\.spec\.ts/,
        /yolink-plots\.spec\.ts/,
        /gallery\.spec\.ts/,
        // /sources2 — the Manager2 Pipeline table, and the commit-history
        // grid it opens in a modal.
        /manager2-grid\.spec\.ts/,
        /manager2-history\.spec\.ts/,
      ],
    },
  ],
});
