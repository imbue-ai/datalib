// App-wide "dev mode" flag, toggled from the status bar (CardsView).
// Off (the default): card chrome shows each card's human-readable
// title (cards/title.ts). On: the chrome shows the card's source in an
// editable box, the technical view. Persisted per browser so the
// choice survives reloads.
import { ref, watch } from "vue";

const STORAGE_KEY = "fw-dev-mode";

export const devMode = ref(localStorage.getItem(STORAGE_KEY) === "1");

watch(devMode, (on) => {
  localStorage.setItem(STORAGE_KEY, on ? "1" : "0");
});
