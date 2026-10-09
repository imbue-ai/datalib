// Which window of the app is the main one: the first open, which keeps
// the layout in the library. A second window (a popped-out card) is a
// scratch space that goes when it closes.
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
    void navigator.locks.request("datalib-main-window", { ifAvailable: true }, (lock) => {
      resolve(lock !== null);
      return lock ? new Promise<void>(() => {}) : undefined;
    });
  });
  return main;
}
