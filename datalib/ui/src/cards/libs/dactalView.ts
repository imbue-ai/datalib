// `dactalView()` in card source returns a CardRender for the DACTAL
// explorer — query your grid_rows with DACTAL's query language and table
// UI (https://dactal.org). It sits alongside gridView/documentView as a
// view the user can open in any card; it does not touch the default grid.
//
// The page runs in a sandboxed iframe: `sandbox="allow-scripts"` with no
// `allow-same-origin`, so its origin is opaque — no cookie, no `/api/*`,
// no reach into this window. It gets its rows from this card over
// `postMessage`, and this card does the fetching with the session it
// holds. The frame side is `public/dactal/bridge.js`; the message shapes
// are shared between the two files and nowhere else.
import { fetchSearch } from "../../api";
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

// The most rows a frame may ask for in one search. The frame is data,
// not the app: it gets what a person could load through the grid.
const MAX_LIMIT = 2000;

type FrameMessage =
  | { type: "dactal:ready" }
  | { type: "dactal:search"; id: number; q: string; limit?: number };

function isFrameMessage(data: unknown): data is FrameMessage {
  if (!data || typeof data !== "object") return false;
  const m = data as { type?: unknown; id?: unknown; q?: unknown };
  if (m.type === "dactal:ready") return true;
  return (
    m.type === "dactal:search" &&
    typeof m.id === "number" &&
    typeof m.q === "string"
  );
}

export function dactalView(opts?: { load?: string; q?: string }): CardRender {
  return (root: ShadowRoot, ctx: CardCtx): (() => void) => {
    ctx.setTitle(opts?.q ? `DACTAL: ${opts.q}` : "DACTAL explorer");

    const frame = document.createElement("iframe");
    // `allow-scripts` alone. Adding `allow-same-origin` would give the
    // page back this origin — and with it the session — which is the
    // whole thing the sandbox is for.
    frame.setAttribute("sandbox", "allow-scripts");
    frame.src = DACTAL_PAGE;
    frame.style.cssText =
      "width:100%;height:100%;border:0;display:block;background:#fff";
    root.appendChild(frame);

    // "*" because the frame's origin is opaque and cannot be named. The
    // check that matters is `source`: only this card's own frame is
    // answered, and only it is sent anything.
    const reply = (msg: unknown) => frame.contentWindow?.postMessage(msg, "*");

    const onMessage = (e: MessageEvent) => {
      if (e.source !== frame.contentWindow) return;
      if (!isFrameMessage(e.data)) return;
      const msg = e.data;
      if (msg.type === "dactal:ready") {
        reply({ type: "dactal:init", load: opts?.load ?? "", dq: opts?.q ?? "" });
        return;
      }
      const limit = Math.min(
        Math.max(1, Math.floor(msg.limit ?? 500)),
        MAX_LIMIT,
      );
      fetchSearch(msg.q, limit).then(
        (resp) => reply({ type: "dactal:rows", id: msg.id, rows: resp.rows }),
        (err: unknown) =>
          reply({
            type: "dactal:error",
            id: msg.id,
            message: err instanceof Error ? err.message : String(err),
          }),
      );
    };
    window.addEventListener("message", onMessage);

    // Future: a `dactal:open` message so a DACTAL row can open a
    // Datalib document card via ctx.host.openCards(`documentView("<uuid>")`).
    return () => {
      window.removeEventListener("message", onMessage);
      frame.remove();
    };
  };
}
