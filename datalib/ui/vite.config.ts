import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";
import path from "node:path";

const BACKEND = process.env.DATALIB_BACKEND ?? "http://127.0.0.1:8731";

// The backend requires its per-process API token on every route (see
// datalib/backend/http/src/auth.rs). In dev the browser loads the app
// from *this* origin, so it never receives the backend's session
// cookie; instead the proxy below — which runs in node, server-side,
// where no page can reach it — stamps the token onto every forwarded
// request. `datalib/dev.sh` mints one token and exports it to both
// processes; if you start Vite by hand, export the same DATALIB_TOKEN
// you gave the backend.
const TOKEN = process.env.DATALIB_TOKEN;
const proxyHeaders = TOKEN ? { authorization: `Bearer ${TOKEN}` } : undefined;

// Warn once, and only when a dev server actually starts — `configure`
// runs at proxy-creation time, so a plain `vite build` stays quiet.
// vitest spins up its own vite server and would trip it too; it never
// proxies anywhere, so skip it.
let warned = false;
function warnIfUnauthenticated() {
  if (TOKEN || warned || process.env.VITEST) return;
  warned = true;
  console.warn(
    "[datalib] DATALIB_TOKEN is unset — /api requests will come back 401.\n" +
      "          Run the backend via `bazelisk run //datalib:dev`, or export\n" +
      "          the same DATALIB_TOKEN for both processes.",
  );
}

// `import.meta.dirname` rather than `__dirname`: Vite 8 warns that
// `__dirname` is unsupported by `configLoader: 'native'`, which is
// slated to become the default.
const CONFIG_DIR = import.meta.dirname;

// Vite 8 resolves a module to its realpath before loading it; Vite 7 did
// not. Everything Bazel hands us is a symlink into bazel-out, so that
// broke this package from both ends at once — and, annoyingly, the two
// ends want opposite fixes.
//
//   * `bazel build` (js_run_binary, chdir into the sandbox): the staged
//     `index.html` resolves through to the real output base while the
//     cwd is a sandbox path. Vite names an emitted asset by relativizing
//     it against `root`, so `index.html` came out with ten `../` in it,
//     and rolldown — Vite 8's bundler — rejects any emitted name that is
//     absolute or relative. Fix: pin `root` to the config's own resolved
//     directory, putting `root` on the same side as the file.
//
//   * `bazel test` (vitest, runfiles): the runfiles symlinks point *out
//     of* the sandbox, so every spec resolved to `/@fs/<real execroot>/…`
//     — a path that exists but that the sandbox will not let the test
//     read. All 16 suites failed with "Cannot find module". Fix:
//     `preserveSymlinks`, so resolution stays inside the runfiles tree.
//
// Applying either fix to both modes breaks the other one, which is why
// this is keyed on `command`. Neither is needed for a host `pnpm build`
// / `pnpm test`, where nothing is a symlink and both branches are inert.
export default defineConfig(({ command }) => ({
  // Build half of the note above. `outDir` has to be pinned to the CWD
  // alongside it: `outDir` is resolved against `root`, so moving `root`
  // to the resolved path also moves the output there — outside the
  // sandbox Bazel is watching. That failed *silently*, which is the
  // worst version of this bug: vite printed "built in 626ms", the
  // `dist` action succeeded with an empty declared output, and the
  // first thing to notice was 60 e2e tests reporting "UI bundle not
  // embedded in this binary".
  ...(command === "build"
    ? { root: CONFIG_DIR, build: { outDir: path.join(process.cwd(), "dist") } }
    : {}),
  plugins: [vue()],
  resolve: {
    alias: {
      "@": path.resolve(CONFIG_DIR, "src"),
    },
    // Under aspect_rules_js, npm deps land at virtual paths like
    // `node_modules/.aspect_rules_js/<pkg>@<ver>/node_modules/<pkg>`
    // *and* at `node_modules/<pkg>` (symlinked back to the same place).
    // Vite's resolver, walking from different importers, hits both
    // paths and bundles vue-router twice. Each copy then declares its
    // own `const routeLocationKey = Symbol()`, so `useRoute()`'s
    // `inject(...)` and the router's `app.provide(...)` key on
    // *different* symbols and `useRoute()` returns undefined.
    //
    // `dedupe` tells Vite to collapse multiple resolutions of these
    // packages to a single instance. Host `pnpm install` doesn't need
    // this because its node_modules layout doesn't expose the double
    // path; the issue is specific to the aspect_rules_js virtual tree.
    dedupe: ["vue", "vue-router", "pinia"],
    // Test/serve half of the note above.
    ...(command === "build" ? {} : { preserveSymlinks: true }),
  },
  server: {
    host: "127.0.0.1",
    port: 5173,
    strictPort: true,
    proxy: {
      "/api": {
        target: BACKEND,
        changeOrigin: false,
        headers: proxyHeaders,
        configure: warnIfUnauthenticated,
      },
      // The agent onboarding docs the wayfinder snippets reference —
      // served by the backend, so they resolve on the dev origin too.
      // Prefix match covers /agent/cards.md, /agent/config.md, and the
      // legacy /agent.md redirect.
      "/agent": {
        target: BACKEND,
        changeOrigin: false,
        headers: proxyHeaders,
      },
    },
  },
  test: {
    environment: "jsdom",
    globals: true,
    // Playwright owns tests/e2e/*.spec.ts; without this exclusion vitest
    // grabs them via its default `**/*.spec.ts` glob and crashes on
    // Playwright's `test.describe` (different test runner).
    exclude: ["**/node_modules/**", "**/dist/**", "tests/e2e/**"],
  },
}));
