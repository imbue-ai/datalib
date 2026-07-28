import { createApp } from "vue";
import { createPinia } from "pinia";
import App from "./App.vue";
import router from "./router";

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

const app = createApp(App);
app.use(createPinia());
app.use(router);
app.mount("#app");
