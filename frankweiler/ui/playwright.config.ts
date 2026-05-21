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
// //frankweiler:dev_tng` uses, so this test and that command produce
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
  const root = mkdtempSync(path.join(tmpdir(), "fw-e2e-"));
  execFileSync(materializer, [root], { stdio: "inherit" });
  process.env.FW_E2E_FIXTURE_ROOT = root;
  return root;
}
const fixtureRoot = ensureFixtureRoot();

// Ephemeral ports so concurrent runs (`bazel test --runs_per_test=N`,
// two devs on one machine) don't collide. Previously hardcoded to
// 8741 / 5183 — distinct from the dev defaults (5173 / 8731) but
// still fixed across runs.
const BACKEND_PORT = cachedPort("FW_E2E_BACKEND_PORT");
const VITE_PORT = cachedPort("FW_E2E_VITE_PORT");
const BACKEND_URL = `http://127.0.0.1:${BACKEND_PORT}`;
const VITE_URL = `http://127.0.0.1:${VITE_PORT}`;

// Locate the bazel-built http binary. Built via:
//   bazelisk build //frankweiler/backend/http:frankweiler_http_bin
//
// FRANKWEILER_HTTP_BIN is set by frankweiler/ui/run_e2e.sh (the sh_test
// wrapper used by `bazel test //frankweiler/ui:e2e_test` and
// `bazel run //frankweiler/ui:e2e`). That wrapper resolves the binary
// out of the test's runfiles via `rlocation` — the only stable way to
// find it under `bazel test`, since the runfiles path isn't computable
// from this file. The fallback to the source-workspace `bazel-bin`
// symlink is for interactive use outside bazel (plain `pnpm exec
// playwright test`), where the developer is expected to have run
// `bazelisk build //frankweiler/backend/http:frankweiler_http_bin`
// beforehand. We avoid the symlink under bazel because it isn't a
// declared input of e2e_test and races with parallel actions under
// `bazel test //...`.
const workspace = path.resolve(here, "../..");
const backendBin =
  process.env.FRANKWEILER_HTTP_BIN ||
  path.join(
    workspace,
    "bazel-bin/frankweiler/backend/http/frankweiler_http_bin",
  );

export default defineConfig({
  testDir: "tests/e2e",
  testMatch: /.*\.spec\.ts$/,
  fullyParallel: false,
  workers: 1,
  reporter: [["list"]],
  use: {
    baseURL: VITE_URL,
    headless: true,
    trace: "retain-on-failure",
  },
  projects: [
    {
      name: "chromium",
      use: { browserName: "chromium" },
    },
  ],
  webServer: [
    {
      // Backend reads its data root + dolt port from the config.yaml
      // emitted by prepare-fixture.cjs (pointed at via FRANKWEILER_CONFIG).
      // We override only the HTTP bind here so each test run claims its
      // own ephemeral port.
      command: JSON.stringify(backendBin),
      url: `${BACKEND_URL}/api/health`,
      reuseExistingServer: false,
      timeout: 30_000,
      env: {
        FRANKWEILER_CONFIG: path.join(fixtureRoot, "config.yaml"),
        FRANKWEILER_BIND: `127.0.0.1:${BACKEND_PORT}`,
      },
    },
    {
      command: `pnpm exec vite --port ${VITE_PORT} --strictPort`,
      url: VITE_URL,
      reuseExistingServer: false,
      timeout: 30_000,
      cwd: here,
      env: {
        FRANKWEILER_BACKEND: BACKEND_URL,
      },
    },
  ],
});
