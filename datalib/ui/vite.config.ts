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

export default defineConfig({
  plugins: [vue()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "src"),
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
});
