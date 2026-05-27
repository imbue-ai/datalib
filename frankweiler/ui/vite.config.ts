import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";
import path from "node:path";

const BACKEND = process.env.FRANKWEILER_BACKEND ?? "http://127.0.0.1:8731";

export default defineConfig({
  // Pin both root and outDir to the config file's directory rather
  // than letting them default to CWD. Under `bazel build //frankweiler/ui:dist`
  // aspect_rules_js chdirs into the staged bazel-bin copy; vite-8 +
  // rolldown otherwise compute a "../../../../execroot/..." outDir
  // that resolves to a sandbox-external path and silently produces
  // zero files.
  root: __dirname,
  build: {
    outDir: path.resolve(__dirname, "dist"),
    emptyOutDir: true,
  },
  plugins: [vue()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "src"),
    },
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
