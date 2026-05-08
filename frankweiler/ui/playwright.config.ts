import { defineConfig } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

// Materialize the bazel-built fixture once, before any worker starts.
// Tests share the resulting data root via FRANKWEILER_ROOT.
const here = path.dirname(fileURLToPath(import.meta.url));
const fixtureRoot = mkdtempSync(path.join(tmpdir(), "fw-e2e-"));
execFileSync(
  "node",
  [path.join(here, "tests/e2e/prepare-fixture.cjs"), fixtureRoot],
  { stdio: "inherit" },
);

// Distinct ports from the dev defaults (5173 / 8731) so a running dev
// server doesn't collide with the test stack.
const BACKEND_PORT = 8741;
const VITE_PORT = 5183;
const BACKEND_URL = `http://127.0.0.1:${BACKEND_PORT}`;
const VITE_URL = `http://127.0.0.1:${VITE_PORT}`;

// Locate the bazel-built http binary. Built via:
//   bazelisk build //frankweiler/backend/http:frankweiler_http_bin
const workspace = path.resolve(here, "../..");
const backendBin = path.join(
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
      // Backend reads FRANKWEILER_ROOT for its data path. We override the
      // bind via a tiny config file pointing at our chosen port.
      command: `${JSON.stringify(backendBin)}`,
      url: `${BACKEND_URL}/api/health`,
      reuseExistingServer: false,
      timeout: 30_000,
      env: {
        FRANKWEILER_ROOT: fixtureRoot,
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
