import { createApp } from "vue";
import { createPinia } from "pinia";
import App from "./App.vue";
import router from "./router";
import { fetchHealth } from "./api";

function applyThemeMode(mode: "light" | "dark") {
  document.documentElement.dataset.theme = mode;
  document.documentElement.setAttribute("data-ag-theme-mode", mode);
}

function setupSystemThemeSync() {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") {
    applyThemeMode("light");
    return;
  }
  const mq = window.matchMedia("(prefers-color-scheme: dark)");
  applyThemeMode(mq.matches ? "dark" : "light");
  mq.addEventListener("change", (e) => applyThemeMode(e.matches ? "dark" : "light"));
}

setupSystemThemeSync();

// Warm the health snapshot at boot: the agent hand-off (handoff.ts) needs
// the API token's path out of it and builds its text inside a synchronous
// click handler, so the fetch has to have already happened. Fire and
// forget — the one consumer degrades to a generic hint if it hasn't
// landed yet.
void fetchHealth().catch(() => {});

const app = createApp(App);
app.use(createPinia());
app.use(router);
app.mount("#app");
