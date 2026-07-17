// Builtin view: the new-card gallery — the user-friendly way to create
// a card without typing source. It lists every parameter-less
// component with a short description: a hardcoded builtin list first
// (gridView leading, since it's the app's front door), then any
// user-defined alias that carries a gallery description (the extra
// field on the `/api/lib` store — see aliasRegistry.aliasDescriptions).
// Picking an entry REPLACES this card with the chosen component via
// ctx.host.setSource, so the gallery is a transient "what should this
// card be?" step, not a lingering column.
//
// Components that need arguments don't belong here; they register a
// parameter-less picker instead (documentView → documentPickerView).
import { watch } from "vue";
import type { CardRender } from "../types";
import { aliasDescriptions, ensureManifest } from "../aliasRegistry";

type GalleryEntry = {
  // Card source the entry expands to, e.g. `gridView()`.
  source: string;
  title: string;
  description: string;
};

// The builtin gallery, in display order.
const BUILTIN_GALLERY: GalleryEntry[] = [
  {
    source: "gridView()",
    title: "Search",
    description: "Search and browse everything in your library.",
  },
  {
    source: "documentPickerView()",
    title: "Document",
    description: "Pick a document from your library and read it.",
  },
  {
    source: "dactalView()",
    title: "DACTAL explorer",
    description: "Query and pivot your data with the DACTAL table UI.",
  },
  {
    source: "perseusView()",
    title: "Perseus corpus",
    description: "Browse the Perseus editions by book, chapter, and section.",
  },
  {
    source: "sourceDagView()",
    title: "Pipeline DAG",
    description:
      "See your sources' step graph and watch syncs flow through it live.",
  },
  {
    source: "aliasView()",
    title: "Component library",
    description: "List the custom components stored on this instance.",
  },
];

export function galleryView(): CardRender {
  return (root, ctx) => {
    ctx.setTitle("New card");
    const style = document.createElement("style");
    style.textContent = `
      .gv { font: 13px/1.5 system-ui, -apple-system, sans-serif; color: var(--fw-fg, inherit); }
      .gv-head { padding: 8px 12px; opacity: .6; border-bottom: 1px solid var(--fw-border, #8884); }
      .gv-row { padding: 8px 12px; cursor: pointer; border-bottom: 1px solid var(--fw-border, #8882); }
      .gv-row:hover { background: var(--fw-hover, rgba(127,127,127,.12)); }
      .gv-title { font-weight: 600; }
      .gv-desc { opacity: .65; }
    `;
    root.appendChild(style);

    const wrap = document.createElement("div");
    wrap.className = "gv";
    root.appendChild(wrap);

    function paint(descs: Map<string, string>) {
      wrap.replaceChildren();
      const head = document.createElement("div");
      head.className = "gv-head";
      head.textContent = "pick what this card should show";
      wrap.appendChild(head);

      // A described alias must work with no arguments (that's the
      // contract of having a description), so `name()` is safe.
      const aliasEntries: GalleryEntry[] = [...descs.entries()]
        .sort((a, b) => a[0].localeCompare(b[0]))
        .map(([name, description]) => ({
          source: `${name}()`,
          title: name,
          description,
        }));

      for (const entry of [...BUILTIN_GALLERY, ...aliasEntries]) {
        const row = document.createElement("div");
        row.className = "gv-row";
        row.addEventListener("click", () => ctx.host.setSource(entry.source));

        const title = document.createElement("div");
        title.className = "gv-title";
        title.textContent = entry.title;
        const desc = document.createElement("div");
        desc.className = "gv-desc";
        desc.textContent = entry.description;
        row.append(title, desc);
        wrap.appendChild(row);
      }
    }

    void ensureManifest();
    const stop = watch(aliasDescriptions, paint, { immediate: true });
    return () => stop();
  };
}
