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
// an iframe: each card gets its own window/engine/storage and full
// isolation from the Vue app. The page lives in public/dactal/ and calls
// the same /api/search the grid uses.
import type { CardRender } from "../types";
import { titled } from "../title";

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
  const title = opts?.q ? `DACTAL: ${opts.q}` : "DACTAL explorer";
  return titled(title, (root: ShadowRoot): (() => void) => {
    const params = new URLSearchParams();
    if (opts?.load) params.set("fw", opts.load); // Frankweiler search → working set
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
  });
}
