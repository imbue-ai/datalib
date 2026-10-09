// How tightly the app packs things, as a step on densityScale.ts's
// scale. theme.css sets spacing from `--datalib-density`, which this sets
// on <html> before the first paint. Persisted per browser.
import { ref, watch } from "vue";
import { MAX_STEP, MIN_STEP, STEP, onScale } from "./densityScale";

const STORAGE_KEY = "datalib-density";

function stored(): number {
  try {
    return onScale(localStorage.getItem(STORAGE_KEY));
  } catch {
    return MIN_STEP;
  }
}

export const density = ref<number>(stored());

export function larger() {
  density.value = onScale(Math.min(MAX_STEP, density.value + STEP));
}

export function smaller() {
  density.value = onScale(Math.max(MIN_STEP, density.value - STEP));
}

watch(
  density,
  (step) => {
    document.documentElement.style.setProperty("--datalib-density", String(step));
    try {
      localStorage.setItem(STORAGE_KEY, String(step));
    } catch {
      // Blocked storage: the choice lasts as long as the page.
    }
  },
  { immediate: true },
);
