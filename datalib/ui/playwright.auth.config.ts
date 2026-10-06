// The sign-in suite (tests/e2e_auth/). Every test starts its own backend
// and latchkey store, so nothing is shared and the files run in
// parallel. Run it through `bazel run //datalib/ui:e2e_auth`, which
// stages what harness.mjs reads from the environment.
import { defineConfig } from "@playwright/test";
import { API_TOKEN } from "./tests/e2e_auth/harness.mjs";

const out = process.env.DATALIB_TEST_AUTH_ARTIFACTS ?? "test-results/e2e_auth";

export default defineConfig({
  testDir: "./tests/e2e_auth",
  testMatch: /.*\.spec\.ts$/,
  fullyParallel: true,
  workers: 4,
  // A world costs a backend start; a browser login, a Chromium start;
  // a Check account, a `datalib-step` and a latchkey run per request.
  timeout: 120_000,
  expect: { timeout: 45_000 },
  outputDir: `${out}/results`,
  reporter: [["list"], ["html", { outputFolder: `${out}/report`, open: "never" }]],
  use: {
    browserName: "chromium",
    headless: true,
    trace: "retain-on-failure",
    extraHTTPHeaders: { authorization: `Bearer ${API_TOKEN}` },
  },
});
