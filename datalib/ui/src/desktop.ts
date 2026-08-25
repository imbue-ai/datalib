/**
 * Desktop-app capabilities, feature-detected at runtime.
 *
 * The same bundle is served to two very different hosts: the Tauri
 * desktop app, and any browser pointed at `datalib-http`. Nothing here
 * may assume Tauri is present — every export degrades to "not
 * available" in a plain browser.
 *
 * ## Why the raw IPC call rather than `@tauri-apps/plugin-opener`
 *
 * The npm package is a thin wrapper over exactly the `invoke` below.
 * Taking it as a dependency would add a *second* version pin (the JS
 * package) that has to stay in lockstep with the Rust plugin pin in
 * `datalib/tauri/Cargo.toml`. This repo has already been bitten by that
 * shape of drift once — see the history note on `DEFAULT_QMD_VERSION`
 * in `datalib/backend/core/src/qmd/mod.rs`, where two copies of one
 * version constant disagreed for six weeks.
 *
 * The coupling that remains is the command name and its argument
 * shape, both asserted in `desktop.spec.ts`. If Tauri renames them, the
 * reveal silently no-ops rather than breaking anything else — and the
 * menu item only appears in the app, where a maintainer will notice.
 */

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
  const t = internals();
  if (!t) return false;
  try {
    // `paths` is plural and an array — the singular form is silently
    // ignored by the plugin, which looks exactly like a no-op.
    await t.invoke("plugin:opener|reveal_item_in_dir", { paths: [path] });
    return true;
  } catch (e) {
    console.warn("reveal_item_in_dir failed", e);
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
