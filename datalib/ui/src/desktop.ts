/**
 * Desktop-app capabilities, feature-detected at runtime.
 *
 * The same bundle is served to two very different hosts: the Tauri
 * desktop app, and any browser pointed at `datalib-http`. Nothing here
 * may assume Tauri is present — every export degrades to "not
 * available" in a plain browser.
 *
 * ## The split with `@tauri-apps/plugin-opener`
 *
 * The plugin package owns the part that can drift upstream: the IPC
 * command name (`plugin:opener|reveal_item_in_dir`) and its argument
 * shape. A Tauri major that renames either is then a `pnpm update`
 * rather than a silent no-op — hand-rolling the `invoke` looked
 * appealing (the package is four lines deep over
 * `window.__TAURI_INTERNALS__.invoke`) but left exactly that hole.
 *
 * This module owns the part the package cannot help with: knowing
 * whether we are in the app at all. The package's own `isTauri()`
 * checks `globalThis.isTauri`, a *different* global from the
 * `__TAURI_INTERNALS__` its `invoke` then calls through; detecting on
 * the object we actually use keeps the check and the call in
 * agreement. That matters here more than in a normal Tauri app,
 * because this bundle is genuinely served to plain browsers, where
 * `invoke` throws a bare `TypeError` rather than degrading.
 */

import { revealItemInDir } from "@tauri-apps/plugin-opener";

/** Tauri's IPC bridge, injected only into windows it trusts. */
interface TauriInternals {
  invoke: (cmd: string, args?: unknown) => Promise<unknown>;
}

function internals(): TauriInternals | null {
  const w = window as unknown as { __TAURI_INTERNALS__?: TauriInternals };
  const t = w.__TAURI_INTERNALS__;
  return t && typeof t.invoke === "function" ? t : null;
}

/**
 * True when running inside the Tauri app *with* IPC reachable.
 *
 * Note this is stricter than "is the desktop app": the app loads the UI
 * from localhost as an external URL, and Tauri withholds IPC from a
 * remote origin unless a capability lists it (see
 * `datalib/tauri/capabilities/reveal-local-files.json`). If that
 * capability is missing or its URL patterns stop matching, this returns
 * false and the UI falls back to browser behavior — which is the
 * failure mode we want: a missing menu item, not a broken one.
 */
export function isDesktopApp(): boolean {
  return internals() !== null;
}

/**
 * Reveal a local file in Finder / Explorer / the platform file manager.
 *
 * Returns false when not in the app, or when the IPC call fails (an
 * absent capability, a path that no longer exists). Callers should
 * treat false as "tell the user", not "throw".
 */
export async function revealInFileManager(path: string): Promise<boolean> {
  // Guard before calling: in a browser the plugin's `invoke` reaches
  // for `window.__TAURI_INTERNALS__.invoke` and throws a bare
  // TypeError, which is not a useful thing to surface from a menu
  // click.
  if (!isDesktopApp()) return false;
  try {
    await revealItemInDir(path);
    return true;
  } catch (e) {
    // Reachable with the bridge present but the command unauthorized —
    // a capability whose URL patterns stopped matching, say — or a
    // path that has since moved.
    console.warn("revealItemInDir failed", e);
    return false;
  }
}

/**
 * The platform's name for "show this file where it lives", so the menu
 * item reads the way the OS does.
 *
 * Uses the user-agent rather than `@tauri-apps/plugin-os` to avoid a
 * second plugin, a second permission, and a second version pin for one
 * string.
 */
export function revealActionLabel(): string {
  const ua = navigator.userAgent;
  if (/Mac OS X|Macintosh/.test(ua)) return "Reveal in Finder";
  if (/Windows/.test(ua)) return "Show in Explorer";
  return "Show in File Manager";
}

/**
 * Decode a `file://` URL back to a filesystem path, or null if `url`
 * isn't one.
 *
 * The round trip matters: the backend builds these with Rust's
 * `Url::from_file_path`, which percent-encodes spaces, `#`, and
 * non-ASCII. Handing the raw URL to the reveal IPC would look for a
 * file literally named `...%20...`.
 */
export function filePathFromUrl(url: string): string | null {
  if (!url.startsWith("file://")) return null;
  try {
    const u = new URL(url);
    // `pathname` keeps the percent-encoding; decodeURIComponent undoes
    // exactly what Url::from_file_path applied.
    const p = decodeURIComponent(u.pathname);
    return p || null;
  } catch {
    return null;
  }
}
