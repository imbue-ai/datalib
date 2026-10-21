import "./bootToken";
import { createApp } from "vue";
import { createPinia } from "pinia";
import App from "./App.vue";
import router from "./router";
import { fetchHealth } from "./api";
import { ApiError } from "./apiError";
import { showTokenRequired } from "./apiToken";
import { installExternalLinkHandler } from "./externalLinks";
import { installTelemetry } from "./telemetry";
// The grid theme, for every grid that is not inside a card's shadow
// root, and for the menus the grid appends to <body> from those that
// are — a card imports it a second time, inline, for its own root.
import "@slickgrid-universal/common/dist/styles/css/slickgrid-theme-default.css";
import "./theme.css";
// Sets the size step on <html> before the first paint.
import "./density";

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

const app = createApp(App);

// Warm the health snapshot at boot: the agent hand-off (handoff.ts) needs
// the API token's path out of it and builds its text inside a synchronous
// click handler, so the fetch has to have already happened. It is also
// the first call to need the token: refused, the page says how to get one
// instead of drawing an app whose every call fails.
void fetchHealth().catch((e) => {
  if (e instanceof ApiError && e.status === 401) {
    app.unmount();
    showTokenRequired(document.getElementById("app")!);
  }
});

app.use(createPinia());
app.use(router);
// Before mount, so the first route and anything the mount throws are
// on the page's record.
installTelemetry(router, app);
app.mount("#app");
