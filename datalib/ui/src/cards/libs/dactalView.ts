// `dactalView()` in card source returns a CardRender for the DACTAL
// explorer — query your grid_rows with DACTAL's query language and table
// UI (https://dactal.org). It sits alongside gridView/documentView as a
// view the user can open in any card; it does not touch the default grid.
//
// Unlike gridView (a Vue custom element mounted straight into the card's
// ShadowRoot), DACTAL ships as classic scripts that attach to `window`
// globals, assume a single engine instance per page, and emit inline
// `onclick=` handlers that resolve against the top-level window. Mounting
// that into a ShadowRoot would (a) break the inline handlers and (b) cap
// us at one DACTAL card per app (they'd share globals). So we mount it in
// an iframe: each card gets its own window/engine/storage, isolating those
// globals from the Vue app and from each other.
//
// That is isolation of JS *globals*, NOT a security boundary — the frame
// has no `sandbox` attribute and is same-origin, so its scripts can call
// /api/* directly with the session cookie. What actually constrains the
// page is its own CSP (public/dactal/index.html) and the API token; see
// docs/dev/dactal.md caveats 5 and 4. Sandboxing it for real needs the
// postMessage bridge that would replace same-origin access.
//
// The page lives in public/dactal/ and calls the same /api/search the
// grid uses.
import type { CardCtx, CardRender } from "../types";

// Served verbatim from ui/public/dactal/ in dev (vite) and prod (vite
// build copies public/ into the dist root).
//
// Must be the explicit `index.html` path, NOT the bare directory `/dactal/`:
// a trailing-slash request doesn't match a public file, so vite's SPA
// fallback serves the main app's index.html instead — which then parses the
// URL as card source ("dactal") and errors. Pointing at the file bypasses
// the fallback entirely.
const DACTAL_PAGE = "/dactal/index.html";

export function dactalView(opts?: { load?: string; q?: string }): CardRender {
  return (root: ShadowRoot, ctx: CardCtx): (() => void) => {
    ctx.setTitle(opts?.q ? `DACTAL: ${opts.q}` : "DACTAL explorer");
    const params = new URLSearchParams();
    if (opts?.load) params.set("datalib", opts.load); // Datalib search → working set
    if (opts?.q) params.set("dq", opts.q); // initial DACTAL query
    const qs = params.toString();

    const frame = document.createElement("iframe");
    frame.src = qs ? `${DACTAL_PAGE}?${qs}` : DACTAL_PAGE;
    frame.style.cssText =
      "width:100%;height:100%;border:0;display:block;background:#fff";
    root.appendChild(frame);

    // Future: bridge over postMessage so opening a DACTAL row calls
    // ctx.host.openCards(`documentView("<uuid>")`), and so host search
    // state can seed the working set. Omitted to keep the view self-
    // contained.
    return () => frame.remove();
  };
}
