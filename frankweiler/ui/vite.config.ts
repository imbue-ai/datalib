import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";
import path from "node:path";

const BACKEND = process.env.FRANKWEILER_BACKEND ?? "http://127.0.0.1:8731";

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
