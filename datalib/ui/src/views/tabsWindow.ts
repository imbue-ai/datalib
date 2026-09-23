// Where the tabs layout keeps its tree, per window. Every window has a
// tree of its own, so a popped-out card is a new stack rather than a
// second view of the first window's. The first window open is the main
// one: it also saves its tree where the next launch restores it from.
import { parseStored, serialize, type Stored } from "@/views/tabTree";

const WINDOW_PREFIX = "datalib-w-";
const SAVED_KEY = "datalib-tabs";
const LOCK = "datalib-tabs-main-window";

// Kept in window.name because a reload keeps it and a window opened
// from this one does not inherit it (sessionStorage may be copied into
// a new window, so it cannot say which window it belongs to).
const windowId: string = (() => {
  if (window.name.startsWith(WINDOW_PREFIX)) return window.name;
  const id = WINDOW_PREFIX + Date.now().toString(36) + Math.random().toString(36).slice(2, 8);
  window.name = id;
  return id;
})();

const ownKey = `${SAVED_KEY}:${windowId}`;

let main: Promise<boolean> | null = null;

// Held until the page goes away, so it is claimed once per page, not
// per mount of the layout. Without Web Locks (a page not served from a
// secure context) every window is main, and the last one to save wins.
export function isMainWindow(): Promise<boolean> {
  main ??= new Promise((resolve) => {
    if (!navigator.locks) {
      resolve(true);
      return;
    }
    void navigator.locks.request(LOCK, { ifAvailable: true }, (lock) => {
      resolve(lock !== null);
      return lock ? new Promise<void>(() => {}) : undefined;
    });
  });
  return main;
}

function read(storage: () => Storage, key: string): Stored | null {
  try {
    return parseStored(storage().getItem(key));
  } catch {
    return null;
  }
}

export function readTrees(): { own: Stored | null; saved: Stored | null } {
  return {
    own: read(() => sessionStorage, ownKey),
    saved: read(() => localStorage, SAVED_KEY),
  };
}

export function writeTree(tree: Stored, mainWindow: boolean) {
  const text = serialize(tree);
  try {
    sessionStorage.setItem(ownKey, text);
    if (mainWindow) localStorage.setItem(SAVED_KEY, text);
  } catch {
    // Private window or blocked storage: the tabs last as long as the page.
  }
}
