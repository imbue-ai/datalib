// Builtin view: a live listing of every custom component the frontend
// store holds — `user` and each applet namespace together, since the
// store draws no distinction. Each row shows the qualified name
// (`comp.<ns>.<name>`) and a short content hash; clicking opens a card
// rendering it with its own stored arguments.
//
// Plain-DOM (no Vue): it just paints a list and re-paints when the
// reactive manifest changes. `vue`'s `watch` works fine outside a
// component as long as we dispose it in the teardown.
import { watch } from "vue";
import type { CardRender } from "../types";
import {
  ensureFrontend,
  frontendManifest,
  gallerySource,
} from "../frontendRegistry";
import type { Meta } from "@/api";

export function aliasView(): CardRender {
  return (root, ctx) => {
    ctx.setTitle("Component library");
    const style = document.createElement("style");
    style.textContent = `
      .av { font: 13px/1.5 ui-monospace, Menlo, monospace; color: var(--datalib-fg, inherit); }
      .av-head { padding: 8px 12px; opacity: .6; border-bottom: 1px solid var(--datalib-border, #8884); }
      .av-row { display: flex; align-items: baseline; gap: .6rem; padding: 6px 12px; cursor: pointer; border-bottom: 1px solid var(--datalib-border, #8882); }
      .av-row:hover { background: var(--datalib-hover, rgba(127,127,127,.12)); }
      .av-name { flex: 0 1 auto; min-width: 0; overflow: hidden; text-overflow: ellipsis; }
      .av-title { flex: 1 1 auto; min-width: 0; overflow: hidden; text-overflow: ellipsis; font-family: system-ui, sans-serif; opacity: .65; }
      .av-hash { flex: 0 0 auto; opacity: .45; margin-left: auto; }
      .av-empty { padding: 16px 12px; opacity: .5; }
    `;
    root.appendChild(style);

    const wrap = document.createElement("div");
    wrap.className = "av";
    root.appendChild(wrap);

    function paint([manifest]: [Map<string, Map<string, Meta>>]) {
      wrap.replaceChildren();
      // Flatten every namespace into one list, qualified. The store is
      // namespaced but this view is a directory of what exists, and a
      // reader wants to see `slack_work.channels` next to `user.tetris`
      // rather than hunting through per-namespace sections.
      const rows: { ns: string; name: string; meta: Meta }[] = [];
      for (const [ns, entries] of manifest) {
        for (const [name, meta] of entries) rows.push({ ns, name, meta });
      }
      rows.sort((a, b) =>
        `${a.ns}.${a.name}`.localeCompare(`${b.ns}.${b.name}`),
      );

      const head = document.createElement("div");
      head.className = "av-head";
      head.textContent = `components (${rows.length})`;
      wrap.appendChild(head);

      if (rows.length === 0) {
        const empty = document.createElement("div");
        empty.className = "av-empty";
        empty.textContent =
          "no components yet — add a card and pick “New component, built by an agent”";
        wrap.appendChild(empty);
        return;
      }

      for (const { ns, name, meta } of rows) {
        const row = document.createElement("div");
        row.className = "av-row";
        const qualified = `comp.${ns}.${name}`;

        const nm = document.createElement("span");
        nm.className = "av-name";
        nm.textContent = qualified;
        row.appendChild(nm);

        if ("renamed_to" in meta) {
          // A tombstone is not openable; say where it went instead.
          const tl = document.createElement("span");
          tl.className = "av-title";
          tl.textContent = `→ comp.${ns}.${meta.renamed_to}`;
          row.appendChild(tl);
          row.title = `renamed to ${meta.renamed_to}`;
          wrap.appendChild(row);
          continue;
        }

        const source = gallerySource(ns, name, meta.component_args);
        row.title = `open ${source}`;
        row.addEventListener("click", () => ctx.host.openCards(source));
        if (meta.title) {
          const tl = document.createElement("span");
          tl.className = "av-title";
          tl.textContent = meta.title;
          row.appendChild(tl);
        }
        const hs = document.createElement("span");
        hs.className = "av-hash";
        hs.textContent = meta.component_hash.slice(0, 8);
        row.appendChild(hs);
        wrap.appendChild(row);
      }
    }

    void ensureFrontend();
    const stop = watch([frontendManifest], paint, { immediate: true });
    return () => stop();
  };
}
