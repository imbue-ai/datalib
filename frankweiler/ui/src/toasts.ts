// Tiny module-level toast store.
//
// Any UI code can push a notice here and it pops up in <ToastStack/>
// (mounted once in App.vue). Used by `api.ts` to surface non-2xx fetches
// and backend-provided `errors[]`, so a degraded response — schema
// mismatch, qmd fallback, etc. — is visible instead of leaving the user
// staring at an empty grid.
//
// No new deps: a module-level `reactive([])` is reactive across all
// components that import `toasts`.

import { reactive } from "vue";

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
  const now = Date.now();
  const key = `${level}|${message}`;
  const last = recent.get(key);
  if (last !== undefined && now - last < DEDUPE_WINDOW_MS) return null;
  recent.set(key, now);

  const id = nextId++;
  toasts.push({ id, level, message, timeoutMs });
  if (timeoutMs !== null) {
    window.setTimeout(() => dismissToast(id), timeoutMs);
  }
  return id;
}

export function dismissToast(id: number): void {
  const i = toasts.findIndex((t) => t.id === id);
  if (i >= 0) toasts.splice(i, 1);
}

export function clearToasts(): void {
  toasts.splice(0, toasts.length);
}
