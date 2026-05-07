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
  },
});
