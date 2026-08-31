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
 * `@tauri-apps/plugin-dialog` is here on the same terms, for the same
 * reason: it owns `plugin:dialog|open` and that command's argument
 * shape, so an upstream rename is a dependency bump rather than a
 * button that silently stops working.
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

import { open as openDialog } from "@tauri-apps/plugin-dialog";
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

/**
 * What a picker invocation did. Three outcomes, not two: canceling and
 * being denied look identical to a caller that only gets `string |
 * null`, and they need opposite responses — cancel must leave the
 * field alone and say nothing, denial has to be visible.
 *
 * Denial is a real failure mode here rather than a theoretical one.
 * The webview reaches this command only through
 * `capabilities/pick-local-paths.json`, and an unlisted or
 * no-longer-matching remote URL pattern makes Tauri refuse the call —
 * with no user-visible signal of its own.
 */
export type PathPick =
  | { outcome: "picked"; path: string }
  | { outcome: "canceled" }
  | { outcome: "unavailable"; reason: string };

/** What kind of thing the dialog should let the user choose. */
export interface PathPickRequest {
  picks: "file" | "dir";
  /** Dialog title. Name the thing being chosen, not the widget. */
  title: string;
  /** Where to open. Ignored unless it looks like a real path (below). */
  startAt?: string;
  /** For `picks: "file"`, extensions to filter on, without the dot. */
  extensions?: string[];
}

/**
 * Open the OS file/folder picker and return what the user chose.
 *
 * Returns `unavailable` in a plain browser: this is a genuinely
 * desktop-only capability, and there is no browser equivalent to fall
 * back to. `<input type="file">` cannot stand in — the browser hands
 * back a sandboxed `File` and never a filesystem path, and the path
 * this feeds is one on the machine running the *backend*, which in the
 * browser-served case need not even be the user's machine. Callers
 * should keep their typed input working and hide the button when
 * `isDesktopApp()` is false. See `docs/dev/wizard_file_pickers.md`.
 */
export async function pickPath(req: PathPickRequest): Promise<PathPick> {
  if (!isDesktopApp()) {
    return { outcome: "unavailable", reason: "not running in the desktop app" };
  }
  try {
    const chosen = await openDialog({
      title: req.title,
      directory: req.picks === "dir",
      multiple: false,
      defaultPath: startDirectory(req.startAt),
      filters:
        req.picks === "file" && req.extensions?.length
          ? [{ name: "Supported files", extensions: req.extensions }]
          : undefined,
    });
    // `multiple: false` makes this `string | null`; the array arm of
    // the plugin's return type is unreachable, and narrowing rather
    // than casting keeps that true if the option ever changes.
    const path = Array.isArray(chosen) ? chosen[0] : chosen;
    return path ? { outcome: "picked", path } : { outcome: "canceled" };
  } catch (e) {
    // Reachable with the bridge present but the command unauthorized.
    console.warn("dialog open failed", e);
    return { outcome: "unavailable", reason: String(e) };
  }
}

/**
 * Sanitize a typed path into something worth opening the dialog at.
 *
 * The field this comes from is free text, so it may hold a half-typed
 * path or a `~` prefix. Tauri passes `defaultPath` to the platform
 * dialog verbatim — no shell is involved, so nothing expands `~`, and
 * a literal `~/backups` is a *relative* path that resolves against the
 * process's working directory. Handing that over opens the dialog
 * somewhere arbitrary, which is worse than not asking for a start
 * directory at all.
 */
function startDirectory(typed: string | undefined): string | undefined {
  const t = typed?.trim();
  if (!t || t.startsWith("~")) return undefined;
  // POSIX absolute, or a Windows drive/UNC path.
  return t.startsWith("/") || /^[A-Za-z]:[\\/]/.test(t) || t.startsWith("\\\\")
    ? t
    : undefined;
}
