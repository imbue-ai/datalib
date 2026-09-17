import { createApp } from "vue";
import { createPinia } from "pinia";
import App from "./App.vue";
import router from "./router";
import { fetchHealth } from "./api";
import { installExternalLinkHandler } from "./externalLinks";
// The grid theme, for every grid that is not inside a card's shadow
// root, and for the menus the grid appends to <body> from those that
// are — a card imports it a second time, inline, for its own root.
import "@slickgrid-universal/common/dist/styles/css/slickgrid-theme-default.css";

function applyThemeMode(mode: "light" | "dark") {
  document.documentElement.dataset.theme = mode;
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

// Off-origin links (a doc's `↗` outlink, any `<a>` that came out of the
// source content) go to the OS browser rather than replacing the app.
// Installed at boot, before any card can render one.
installExternalLinkHandler();

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
