// Builtin view: pick a document to read. `documentView` needs a
// markdown UUID argument, so it can't be created from the gallery
// directly; `documentPickerView()` is its parameter-less gallery
// stand-in — a list of every rendered document (`/api/docs`), newest
// first. Picking one REPLACES this card with `documentView("<uuid>")`
// via ctx.host.setSource, so the picker acts as a transient "new card"
// step rather than a lingering column.
//
// Plain-DOM (no Vue), same pattern as aliasView: paint once from a
// fetch, nothing reactive to watch.
import type { CardRender } from "../types";
import { titled } from "../title";
import { fetchDocs } from "@/api";

export function documentPickerView(): CardRender {
  return titled("Open document", (root, ctx) => {
    const style = document.createElement("style");
    style.textContent = `
      .dp { font: 13px/1.5 system-ui, -apple-system, sans-serif; color: var(--fw-fg, inherit); }
      .dp-head { padding: 8px 12px; opacity: .6; border-bottom: 1px solid var(--fw-border, #8884); }
      .dp-row { display: flex; align-items: baseline; gap: .6rem; padding: 6px 12px; cursor: pointer; border-bottom: 1px solid var(--fw-border, #8882); }
      .dp-row:hover { background: var(--fw-hover, rgba(127,127,127,.12)); }
      .dp-title { flex: 1 1 auto; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
      .dp-title--untitled { opacity: .55; font-style: italic; }
      .dp-kind { flex: 0 0 auto; opacity: .55; font-size: 12px; }
      .dp-date { flex: 0 0 auto; opacity: .45; font-size: 12px; font-variant-numeric: tabular-nums; }
      .dp-empty { padding: 16px 12px; opacity: .5; }
    `;
    root.appendChild(style);

    const wrap = document.createElement("div");
    wrap.className = "dp";
    root.appendChild(wrap);

    const note = document.createElement("div");
    note.className = "dp-empty";
    note.textContent = "loading documents…";
    wrap.appendChild(note);

    const abort = new AbortController();
    fetchDocs(abort.signal)
      .then((docs) => {
        wrap.replaceChildren();
        const head = document.createElement("div");
        head.className = "dp-head";
        head.textContent = `documents (${docs.length})`;
        wrap.appendChild(head);

        if (docs.length === 0) {
          const empty = document.createElement("div");
          empty.className = "dp-empty";
          empty.textContent = "no documents yet — add a source and sync";
          wrap.appendChild(empty);
          return;
        }

        for (const doc of docs) {
          const row = document.createElement("div");
          row.className = "dp-row";
          row.addEventListener("click", () =>
            ctx.host.setSource(`documentView(${JSON.stringify(doc.markdown_uuid)})`),
          );

          const title = document.createElement("span");
          title.className = "dp-title";
          if (doc.title) {
            title.textContent = doc.title;
          } else {
            title.classList.add("dp-title--untitled");
            title.textContent = doc.markdown_uuid;
          }
          const kind = document.createElement("span");
          kind.className = "dp-kind";
          kind.textContent = doc.provider;
          const date = document.createElement("span");
          date.className = "dp-date";
          // ISO-ish timestamps: the date part is the first 10 chars.
          date.textContent = doc.created_at?.slice(0, 10) ?? "";
          row.append(title, kind, date);
          wrap.appendChild(row);
        }
      })
      .catch((e: unknown) => {
        if ((e as { name?: string }).name === "AbortError") return;
        note.textContent = `failed to load documents: ${(e as Error).message}`;
      });

    return () => abort.abort();
  });
}
