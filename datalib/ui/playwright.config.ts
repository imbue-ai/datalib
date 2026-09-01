import { defineConfig } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

// Ask the kernel for a free ephemeral port. Shells out to a tiny Node
// one-liner so we stay synchronous (Playwright's config module isn't
// async). There's a small race between close() here and the real
// listener binding, but it's the standard ephemeral-port pattern and
// lets `bazel test --runs_per_test=N` (and parallel local runs) coexist
// without colliding on fixed dev ports.
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
//
// The materializer is the same script `bazelisk run
// //datalib:dev_tng` uses, so this test and that command produce
// byte-identical data roots. Under `bazel test` run_e2e.sh resolves the
// runfiles path and passes it via FW_E2E_MATERIALIZE_TNG_ROOT;
// interactive `pnpm exec playwright test` falls back to the source-tree
// bazel-bin symlink.
const here = path.dirname(fileURLToPath(import.meta.url));
function ensureFixtureRoot(): string {
  const existing = process.env.FW_E2E_FIXTURE_ROOT;
  if (existing) return existing;
  const workspace = path.resolve(here, "../..");
  const materializer =
    process.env.FW_E2E_MATERIALIZE_TNG_ROOT ||
    path.join(workspace, "bazel-bin/tests/fixtures/materialize_tng_root");
  const root = mkdtempSync(path.join(tmpdir(), "datalib-e2e-"));
  execFileSync(materializer, [root], { stdio: "inherit" });
  process.env.FW_E2E_FIXTURE_ROOT = root;
  return root;
}
const fixtureRoot = ensureFixtureRoot();

// Ephemeral port so concurrent runs (`bazel test --runs_per_test=N`,
// two devs on one machine) don't collide on a fixed port.
//
// The Vite dev server used to be a second webServer here, with the UI
// loaded from source and `/api/*` proxied to the backend. Now the
// backend binary embeds the Vite-built SPA (see
// datalib/backend/http/src/embed.rs) and serves it at `/`, so
// Playwright drives the *packaged artifact* against the same origin —
// closer to what users run.
const BACKEND_PORT = cachedPort("FW_E2E_BACKEND_PORT");
const BACKEND_URL = `http://127.0.0.1:${BACKEND_PORT}`;

// A second backend, on an empty data root, for the first-run
// onboarding spec. It has to be its own server: the onboarding screen
// is gated on the data root having no `config.toml`, and the fixture
// root above has one — there is no way to reach the empty-root state
// from a server already pointed at a populated one.
//
// Fresh `mkdtemp` per config load, so the spec that initializes it
// still sees an uninitialized root on the next run (including
// `--runs_per_test=N`, where bazel re-launches this process per run).
// Cached in env for the same reason the ports are: worker subprocesses
// re-import this file and must not mint a second directory.
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

// The backend requires its API token on every route (see
// datalib/backend/http/src/auth.rs). Pin one via DATALIB_TOKEN rather
// than letting the binary mint a random one we'd have to read back out
// of the data root. `use.extraHTTPHeaders` then authenticates the
// `request` fixture *and* every navigation and subresource the browser
// context issues, so the specs stay unaware that auth exists. Cached in
// env for the same reason the ports are: this config is re-imported in
// each worker subprocess.
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
//
// DATALIB_HTTP_BIN is set by datalib/ui/run_e2e.sh (the sh_test
// wrapper used by `bazel test //datalib/ui:e2e_test` and
// `bazel run //datalib/ui:e2e`). That wrapper resolves the binary
// out of the test's runfiles via `rlocation` — the only stable way to
// find it under `bazel test`, since the runfiles path isn't computable
// from this file. The fallback to the source-workspace `bazel-bin`
// symlink is for interactive use outside bazel (plain `pnpm exec
// playwright test`), where the developer is expected to have run
// `bazelisk build //datalib/backend/http:datalib_http_bin`
// beforehand. We avoid the symlink under bazel because it isn't a
// declared input of e2e_test and races with parallel actions under
// `bazel test //...`.
const workspace = path.resolve(here, "../..");
const backendBin =
  process.env.DATALIB_HTTP_BIN ||
  path.join(
    workspace,
    "bazel-bin/datalib/backend/http/datalib_http_bin",
  );

export default defineConfig({
  testDir: "tests/e2e",
  testMatch: /.*\.spec\.ts$/,
  fullyParallel: false,
  workers: 1,
  reporter: [["list"]],
  // Drop Playwright's default `-{projectName}-{platform}` suffix on
  // snapshot filenames. Our snapshots today are text dumps of API
  // payloads (see grid-fixture-golden.spec.ts) — identical on every
  // OS, so the platform suffix is meaningless churn. If we ever
  // add screenshot snapshots (which legitimately differ per
  // platform because of font hinting + anti-aliasing), opt those
  // back in by passing `snapshotPathTemplate` per
  // `toMatchSnapshot()` call.
  //
  // Playwright's default template is
  // `{snapshotDir}/{testFileDir}/{testFileName}-snapshots/{arg}{-projectName}{-platform}{ext}`;
  // we drop the last two `-…` segments. The `{snapshotDir}` anchor
  // is required — without it `{testFileDir}` evaluates to a bare
  // relative path that mkdir interprets as absolute (rooted at `/`).
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
      name: "chromium",
      use: { browserName: "chromium" },
    },
    {
      // The desktop app runs in a WKWebView, not Chromium, and WebKit's
      // layout differs in ways that have twice shipped an invisible AG
      // Grid: it resolves a child's percentage `height` against the
      // parent's *specified* height, so `height: 100%` under a
      // flex-sized parent with no `height` of its own computes to
      // `auto` and the grid collapses to its border. Rows and headers
      // stay in the DOM, so every locator-and-count assertion in this
      // suite passes while nothing is painted (see `expectGridPainted`
      // in tests/e2e/grid-helpers.ts, which is the assertion shape that
      // does catch it).
      //
      // Only the grid-bearing specs are re-run here — this project
      // exists to cover layout the engines disagree about, not to
      // double-run application logic that is engine-independent.
      // `first-run.spec.ts` is excluded for a second reason: it
      // initializes the empty data root, which is minted once per
      // config load, so it cannot run twice in one session.
      name: "webkit",
      use: { browserName: "webkit" },
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
      // Backend takes the data root as its only positional arg; bind
      // address comes from DATALIB_BIND so each test run claims its
      // own ephemeral port. The fixture root produced by
      // materialize_tng_root.sh IS the data root.
      // `--no-open` skips the default browser auto-open; Playwright
      // drives chromium itself, we don't want a second tab fighting
      // for focus every test run.
      command: `${JSON.stringify(backendBin)} ${JSON.stringify(fixtureRoot)} --no-open`,
      // Playwright's own readiness probe doesn't go through
      // `use.extraHTTPHeaders`, so the token rides the query string here.
      url: `${BACKEND_URL}/api/health?token=${API_TOKEN}`,
      reuseExistingServer: false,
      timeout: 30_000,
      env: {
        DATALIB_BIND: `127.0.0.1:${BACKEND_PORT}`,
        DATALIB_TOKEN: API_TOKEN,
        // The sync worker shells out to `datalib-dag`. Under bazel it
        // lives in the runfiles, not beside the server binary, so the
        // worker's directory-then-PATH fallbacks both miss it and every
        // sync dies at startup with "datalib-dag binary not found".
        // run_e2e.sh resolves it; passed through here because this
        // `env` block is what the child actually gets.
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
  ],
});
