import { defineConfig } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { copyFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { CONFIG_MUTATING } from "./tests/e2e/config-mutating";

  // Ask the kernel for a free ephemeral port, via a Node one-liner so we stay
  // synchronous (Playwright's config module isn't async). The small race
  // between close() here and the real listener binding is the standard
  // ephemeral-port pattern, and it lets parallel runs coexist.
function freePort(): number {
  const out = execFileSync("node", [
    "-e",
    "const s=require('net').createServer();s.listen(0,'127.0.0.1',()=>{process.stdout.write(String(s.address().port));s.close()});",
  ]).toString();
  return Number.parseInt(out, 10);
}

// Playwright reloads this config in each worker subprocess; freePort()
// must therefore be idempotent across reloads or each worker will point
// at ports nobody is listening on. Inherit from env when present so the
// values minted in the parent process flow into the workers.
function cachedPort(envVar: string): number {
  const existing = process.env[envVar];
  if (existing) return Number.parseInt(existing, 10);
  const port = freePort();
  process.env[envVar] = String(port);
  return port;
}

// Materialize the bazel-built fixture once, before any worker starts.
// Tests share the resulting data root via FW_E2E_FIXTURE_ROOT — cached
// in env so worker subprocesses (which re-import this config) don't
// each rebuild the fixture into a fresh temp dir.
const here = path.dirname(fileURLToPath(import.meta.url));
// `datalib/ui/..` — the workspace root, for the `bazel-bin/...`
// fallbacks used when this config is loaded outside bazel.
const workspaceDir = path.resolve(here, "..", "..");
function materializeRoot(prefix: string): string {
  const materializer =
    process.env.FW_E2E_MATERIALIZE_TNG_ROOT ||
    path.join(workspaceDir, "bazel-bin/tests/fixtures/materialize_tng_root");
  const root = mkdtempSync(path.join(tmpdir(), prefix));
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

// Ephemeral port so concurrent runs (`bazel test --runs_per_test=N`,
// two devs on one machine) don't collide on a fixed port.
const BACKEND_PORT = cachedPort("FW_E2E_BACKEND_PORT");
const BACKEND_URL = `http://127.0.0.1:${BACKEND_PORT}`;

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
  const root = mkdtempSync(path.join(tmpdir(), "datalib-e2e-empty-"));
  process.env.FW_E2E_EMPTY_ROOT = root;
  return root;
}
const EMPTY_ROOT = emptyRoot();
const EMPTY_PORT = cachedPort("FW_E2E_EMPTY_PORT");
const EMPTY_URL = `http://127.0.0.1:${EMPTY_PORT}`;
process.env.FW_E2E_EMPTY_URL = EMPTY_URL;

  // A third backend on a third empty root: `first-run.spec.ts` already owns
  // EMPTY_ROOT and initializes it, and onboarding is one-shot — a root with a
  // config can never go back to having none.
function onboardingRoot(): string {
  const existing = process.env.FW_E2E_ONBOARDING_ROOT;
  if (existing) return existing;
  const root = mkdtempSync(path.join(tmpdir(), "datalib-e2e-onboarding-"));
  process.env.FW_E2E_ONBOARDING_ROOT = root;
  return root;
}
const ONBOARDING_ROOT = onboardingRoot();
const ONBOARDING_PORT = cachedPort("FW_E2E_ONBOARDING_PORT");
const ONBOARDING_URL = `http://127.0.0.1:${ONBOARDING_PORT}`;
process.env.FW_E2E_ONBOARDING_URL = ONBOARDING_URL;

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
  const dir = mkdtempSync(path.join(tmpdir(), "datalib-e2e-pdfs-"));
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
  const dir = mkdtempSync(path.join(tmpdir(), "datalib-e2e-signal-"));
  execFileSync(SIGNAL_MAKE_FIXTURE, [SIGNAL_SPEC, dir], { stdio: "pipe" });
  process.env.FW_E2E_SIGNAL_BACKUP_DIR = dir;
  return dir;
}
signalBackupDir();

// ── the config-mutating specs, one data root each ────────────────────
type Sandbox = { spec: string; root: string; port: number; url: string };

/// Cached in env like the ports and the fixture root: worker
/// subprocesses re-import this config and must attach to what the
/// parent minted rather than minting their own.
function sandboxes(): Sandbox[] {
  const existing = process.env.FW_E2E_SANDBOXES;
  if (existing) return JSON.parse(existing) as Sandbox[];
  const made = CONFIG_MUTATING.map((spec) => {
    const port = freePort();
    return {
      spec,
      root: materializeRoot(`datalib-e2e-${spec}-`),
      port,
      url: `http://127.0.0.1:${port}`,
    };
  });
  process.env.FW_E2E_SANDBOXES = JSON.stringify(made);
  return made;
}
const SANDBOXES = sandboxes();

  // The backend requires its API token on every route, so pin one via
  // DATALIB_TOKEN rather than reading back a random one. `use.extraHTTPHeaders`
  // then authenticates the `request` fixture and every navigation the browser
  // context issues, so the specs stay unaware that auth exists. Cached in env
  // because each worker subprocess re-imports this file.
function cachedToken(): string {
  const existing = process.env.DATALIB_TOKEN;
  if (existing) return existing;
  const token = `e2e-${BACKEND_PORT}-${process.pid}`;
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
        // /sources2 — the Manager2 Pipeline table.
        /manager2-grid\.spec\.ts/,
      ],
    },
  ],
  webServer: [
    {
        // The data root is the only positional arg; the bind address comes
        // from DATALIB_BIND so each run claims its own port. `--no-open` keeps
        // a second browser tab from fighting Playwright's for focus.
      command: `${JSON.stringify(backendBin)} ${JSON.stringify(fixtureRoot)} --no-open`,
      // Playwright's own readiness probe doesn't go through
      // `use.extraHTTPHeaders`, so the token rides the query string here.
      url: `${BACKEND_URL}/api/health?token=${API_TOKEN}`,
      reuseExistingServer: false,
      timeout: 30_000,
      env: {
        DATALIB_BIND: `127.0.0.1:${BACKEND_PORT}`,
        DATALIB_TOKEN: API_TOKEN,
          // The sync worker shells out to `datalib-dag`, which under bazel
          // lives in the runfiles rather than beside the server binary, so the
          // worker's own fallbacks both miss it. run_e2e.sh resolves it.
        ...(process.env.DATALIB_DAG_BIN
          ? { DATALIB_DAG_BIN: process.env.DATALIB_DAG_BIN }
          : {}),
      },
    },
    {
      // The empty-root backend behind `first-run.spec.ts`. Same binary,
      // same token (so `use.extraHTTPHeaders` authenticates both), a
      // data root the backend creates on demand and never populates.
      command: `${JSON.stringify(backendBin)} ${JSON.stringify(EMPTY_ROOT)} --no-open`,
      url: `${EMPTY_URL}/api/health?token=${API_TOKEN}`,
      reuseExistingServer: false,
      timeout: 30_000,
      env: {
        DATALIB_BIND: `127.0.0.1:${EMPTY_PORT}`,
        DATALIB_TOKEN: API_TOKEN,
      },
    },
    {
        // The onboarding backend: a third empty root, and the one server here
        // whose PATH carries the dash-named binaries. The scaffold config names
        // `datalib-dag`, its steps and the `unified_index` applet bare, so PATH
        // is how all three are found — the installed-user arrangement, which
        // the other two servers never exercise.
      command: `${JSON.stringify(backendBin)} ${JSON.stringify(ONBOARDING_ROOT)} --no-open`,
      url: `${ONBOARDING_URL}/api/health?token=${API_TOKEN}`,
      reuseExistingServer: false,
      timeout: 30_000,
      env: {
        DATALIB_BIND: `127.0.0.1:${ONBOARDING_PORT}`,
        DATALIB_TOKEN: API_TOKEN,
        PATH: `${binDir}${path.delimiter}${process.env.PATH ?? ""}`,
        // Signal's download step reads its passphrase from the
        // environment — the wizard writes the *name* of the variable,
        // never the secret. The step inherits this from the runner,
        // which inherits it from this server, which is the same chain a
        // real install has from the user's shell.
        SIGNAL_BACKUP_PASSPHRASE: FIXTURE_SIGNAL_AEP,
      },
    },
    // A backend apiece for the config-mutating specs. Same binary and
    // same token as the shared one; the data root is the only thing
    // that differs, which is the whole point.
    ...SANDBOXES.map((s) => ({
      command: `${JSON.stringify(backendBin)} ${JSON.stringify(s.root)} --no-open`,
      url: `${s.url}/api/health?token=${API_TOKEN}`,
      reuseExistingServer: false,
      timeout: 30_000,
      env: {
        DATALIB_BIND: `127.0.0.1:${s.port}`,
        DATALIB_TOKEN: API_TOKEN,
        ...(process.env.DATALIB_DAG_BIN
          ? { DATALIB_DAG_BIN: process.env.DATALIB_DAG_BIN }
          : {}),
      },
    })),
  ],
});
