// Builtin view: a live listing of the user-defined component library
// (the `/api/lib` alias store). Each row shows a component's name and a
// short content hash; clicking opens a column that renders that
// component (`name()`), so you can eyeball any stored component.
//
// Plain-DOM (no Vue): it just paints a list and re-paints when the
// reactive manifest changes. `vue`'s `watch` works fine outside a
// component as long as we dispose it in the teardown.
import { watch } from "vue";
import type { CardRender } from "../types";
import { aliasManifest, ensureManifest } from "../aliasRegistry";

export function aliasView(): CardRender {
  return (root, ctx) => {
    ctx.setTitle("Component library");
    const style = document.createElement("style");
    style.textContent = `
      .av { font: 13px/1.5 ui-monospace, Menlo, monospace; color: var(--fw-fg, inherit); }
      .av-head { padding: 8px 12px; opacity: .6; border-bottom: 1px solid var(--fw-border, #8884); }
      .av-row { display: flex; align-items: baseline; gap: .6rem; padding: 6px 12px; cursor: pointer; border-bottom: 1px solid var(--fw-border, #8882); }
      .av-row:hover { background: var(--fw-hover, rgba(127,127,127,.12)); }
      .av-name { flex: 1 1 auto; min-width: 0; overflow: hidden; text-overflow: ellipsis; }
      .av-hash { flex: 0 0 auto; opacity: .45; }
      .av-empty { padding: 16px 12px; opacity: .5; }
    `;
    root.appendChild(style);

    const wrap = document.createElement("div");
    wrap.className = "av";
    root.appendChild(wrap);

    function paint(m: Map<string, string>) {
      wrap.replaceChildren();
      const head = document.createElement("div");
      head.className = "av-head";
      head.textContent = `components (${m.size})`;
      wrap.appendChild(head);

      if (m.size === 0) {
        const empty = document.createElement("div");
        empty.className = "av-empty";
        empty.textContent =
          "no components yet — use the 🤖 button on a card to create one";
        wrap.appendChild(empty);
        return;
      }

      for (const [name, hash] of [...m.entries()].sort((a, b) =>
        a[0].localeCompare(b[0]),
      )) {
        const row = document.createElement("div");
        row.className = "av-row";
        row.title = `open ${name}()`;
        row.addEventListener("click", () => ctx.host.openCards(`${name}()`));

        const nm = document.createElement("span");
        nm.className = "av-name";
        nm.textContent = name;
        const hs = document.createElement("span");
        hs.className = "av-hash";
        hs.textContent = hash.slice(0, 8);
        row.append(nm, hs);
        wrap.appendChild(row);
      }
    }

    void ensureManifest();
    const stop = watch(aliasManifest, paint, { immediate: true });
    return () => stop();
  };
}
