// Detect clicks on internal `/chat/<uuid>` anchor links in rendered
// markdown bodies. The single caller today is `DocColumn`, but the
// helper lives in its own module so any future consumer (e.g. a
// non-Miller standalone view) can share the modifier-click bailout
// and `<a>` traversal without re-deriving them.

const CHAT_HREF_RE = /^#?\/chat\/([^/?#]+)/;

/**
 * Return the markdown UUID this click should navigate to, or null if
 * the click should fall through to the browser. Returns null when:
 *   - the target isn't (or isn't inside) an `<a>`,
 *   - the href doesn't match `/chat/<uuid>` (or `#/chat/<uuid>`),
 *   - the user held a modifier or used a non-primary button — those
 *     are how a user opens links in a new tab/window, and we don't
 *     want to swallow that affordance.
 *
 * The caller is expected to `ev.preventDefault()` only when a UUID
 * is returned.
 */
export function chatHrefFromClick(ev: MouseEvent): string | null {
  const t = ev.target;
  if (!(t instanceof Element)) return null;
  const a = t.closest("a");
  if (!a) return null;
  // Plain-text href (NOT the resolved absolute URL) — the renderer
  // emits `/chat/<uuid>` and we want that exact form.
  const href = a.getAttribute("href") ?? "";
  const m = CHAT_HREF_RE.exec(href);
  if (!m) return null;
  if (ev.metaKey || ev.ctrlKey || ev.shiftKey || ev.button !== 0) return null;
  return m[1];
}
