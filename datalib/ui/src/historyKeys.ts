// Back and forward in the desktop app. A browser tab has buttons and
// keys for its history; the app's webview has the same history and no
// chrome around it, so the shell answers the keys a person already
// knows — Cmd+[ / Cmd+] on a mac, Alt+←/→ elsewhere — with the calls
// the buttons would make. In a browser the browser owns those keys.
import { isDesktopApp } from "./desktop";

type KeyPress = { key: string; metaKey: boolean; altKey: boolean; ctrlKey: boolean };

// -1 for back, 1 for forward, 0 for a key that is not a history key.
export function historyStep(e: KeyPress): -1 | 0 | 1 {
  if (e.metaKey && !e.altKey && !e.ctrlKey) {
    if (e.key === "[") return -1;
    if (e.key === "]") return 1;
  }
  if (e.altKey && !e.metaKey && !e.ctrlKey) {
    if (e.key === "ArrowLeft") return -1;
    if (e.key === "ArrowRight") return 1;
  }
  return 0;
}

// A field keeps its own keys: Cmd+[ is "outdent" in some editors and
// Alt+arrow moves by word.
function inEditable(target: EventTarget | null): boolean {
  const el = target instanceof HTMLElement ? target : null;
  return !!el && (el.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName));
}

export function installHistoryKeys(win: Window = window): () => void {
  if (!isDesktopApp()) return () => {};
  const onKey = (e: KeyboardEvent) => {
    const step = historyStep(e);
    if (step === 0 || inEditable(e.composedPath()[0] ?? e.target)) return;
    e.preventDefault();
    win.history.go(step);
  };
  win.addEventListener("keydown", onKey);
  return () => win.removeEventListener("keydown", onKey);
}
