// Tiny module-level toast store.

import { reactive } from "vue";
import { onSessionEnded, sessionEnded } from "./session";
import { track } from "./telemetry";

export type ToastLevel = "error" | "warn" | "info";

export type Toast = {
  id: number;
  level: ToastLevel;
  message: string;
  // ms after which the toast auto-dismisses. `null` = sticky (user
  // must click ×). Errors default to sticky so a one-shot blip isn't
  // missed; info/warn auto-dismiss.
  timeoutMs: number | null;
};

// Exported so <ToastStack/> can iterate. Don't mutate from outside —
// use `pushToast` / `dismissToast`.
export const toasts = reactive<Toast[]>([]);

let nextId = 1;
// De-dupe identical-message toasts within this window. The search box
// re-runs on every keystroke, so a persistent backend error would
// otherwise paint a fresh toast each character.
const DEDUPE_WINDOW_MS = 5_000;
const recent = new Map<string, number>();

export function pushToast(
  message: string,
  level: ToastLevel = "error",
  timeoutMs: number | null = level === "error" ? null : 4_000,
): number | null {
  if (sessionEnded()) return null;
  const now = Date.now();
  const key = `${level}|${message}`;
  const last = recent.get(key);
  if (last !== undefined && now - last < DEDUPE_WINDOW_MS) return null;
  recent.set(key, now);

  const id = nextId++;
  toasts.push({ id, level, message, timeoutMs });
  // Gone from the screen in seconds; the log is where it can be read again.
  track("toast", undefined, { level, msg: message });
  if (timeoutMs !== null) {
    window.setTimeout(() => dismissToast(id), timeoutMs);
  }
  return id;
}

export function dismissToast(id: number): void {
  const i = toasts.findIndex((t) => t.id === id);
  if (i >= 0) toasts.splice(i, 1);
}

// Once the server has refused this tab, every request fails the same way
// and each failure would be a toast of its own. One says why, in place of
// them all; the server will not take the page's events any more either.
onSessionEnded(() => {
  toasts.splice(0, toasts.length, {
    id: nextId++,
    level: "error",
    message:
      "Datalib was restarted, so this tab can no longer reach it. Open the address it printed when it started, or reopen the app.",
    timeoutMs: null,
  });
});
