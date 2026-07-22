// Builtin view: the new-card gallery — the way every new card starts,
// in both dev and non-dev mode. It lists every parameter-less
// component with a short description: a hardcoded builtin list first
// (gridView leading, since it's the app's front door), then any
// user-defined alias that carries a gallery description (the extra
// field on the `/api/lib` store — see aliasRegistry.aliasDescriptions),
// then the "build a component with an agent" entry (handoff.ts),
// which mints a fresh component and walks the user through handing it
// to a coding agent. Picking an entry REPLACES this card with the
// chosen component via ctx.host.setSource, so the gallery is a
// transient "what should this card be?" step, not a lingering column.
//
// Dev mode additionally shows each entry's card source and a footer
// reminding that source can be typed straight into the chrome bar.
//
// Components that need arguments don't belong here; they register a
// parameter-less picker instead (documentView → documentPickerView).
import { watch } from "vue";
import type { CardRender } from "../types";
import { aliasDescriptions, aliasTitles, ensureManifest } from "../aliasRegistry";
import { createComponentWithAgent } from "@/handoff";
import { devMode } from "@/devMode";

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
      /* Title line: the dev-mode source shares the title's line while
         it fits (baseline-aligned flex) and wraps under it when the
         column is narrow — minimal layout shift vs non-dev. */
      .gv-head-line { display: flex; flex-wrap: wrap; align-items: baseline; column-gap: 10px; }
      .gv-title { font-weight: 600; }
      .gv-desc { opacity: .65; }
      .gv-src { font: 11px/1.4 ui-monospace, Menlo, monospace; opacity: .5; }
      .gv-foot { padding: 8px 12px; opacity: .55; font-size: 12px; }
    `;
    root.appendChild(style);

    const wrap = document.createElement("div");
    wrap.className = "gv";
    root.appendChild(wrap);

    function paint([descs, titles, dev]: [
      Map<string, string>,
      Map<string, string>,
      boolean,
    ]) {
      wrap.replaceChildren();
      const head = document.createElement("div");
      head.className = "gv-head";
      head.textContent = "pick what this card should show";
      wrap.appendChild(head);

      // A described alias must work with no arguments (that's the
      // contract of having a description), so `name()` is safe. The
      // display title is the alias' stored `title` when it carries
      // one, else the bare name.
      const aliasEntries: GalleryEntry[] = [...descs.entries()]
        .sort((a, b) => a[0].localeCompare(b[0]))
        .map(([name, description]) => ({
          source: `${name}()`,
          title: titles.get(name) ?? name,
          description,
        }));

      function addRow(
        title: string,
        description: string,
        src: string | null,
        onPick: () => void,
      ) {
        const row = document.createElement("div");
        row.className = "gv-row";
        row.addEventListener("click", onPick);

        const headLine = document.createElement("div");
        headLine.className = "gv-head-line";
        const titleEl = document.createElement("span");
        titleEl.className = "gv-title";
        titleEl.textContent = title;
        headLine.appendChild(titleEl);
        // Dev mode: show what the pick expands to, teaching the
        // source-expression model row by row. Same line as the title
        // while it fits (see .gv-head-line).
        if (dev && src !== null) {
          const code = document.createElement("span");
          code.className = "gv-src";
          code.textContent = src;
          headLine.appendChild(code);
        }
        const desc = document.createElement("div");
        desc.className = "gv-desc";
        desc.textContent = description;
        row.append(headLine, desc);
        wrap.appendChild(row);
      }

      for (const entry of [...BUILTIN_GALLERY, ...aliasEntries]) {
        addRow(entry.title, entry.description, entry.source, () =>
          ctx.host.setSource(entry.source),
        );
      }
      // Last, after even the user's own components: the escape hatch
      // for when nothing above fits. No source line in dev mode — the
      // component name is minted on pick.
      addRow(
        "🤖 New component, built by an agent",
        "Create a fresh component and hand it to a coding agent to build.",
        null,
        () => void createComponentWithAgent(ctx.host),
      );

      if (dev) {
        const foot = document.createElement("div");
        foot.className = "gv-foot";
        foot.textContent =
          "dev mode: every card is a JS expression — you can also type " +
          "source directly into the box above and press Enter.";
        wrap.appendChild(foot);
      }
    }

    void ensureManifest();
    const stop = watch([aliasDescriptions, aliasTitles, devMode], paint, {
      immediate: true,
    });
    return () => stop();
  };
}
