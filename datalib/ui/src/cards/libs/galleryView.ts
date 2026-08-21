// Builtin view: the new-card gallery — the way every new card starts,
// in both dev and non-dev mode. It lists every parameter-less
// component with a short description: a hardcoded builtin list first
// (gridView leading, since it's the app's front door), then every
// titled component in the frontend store, then the
// "build a component with an agent" entry (handoff.ts),
// which mints a fresh component and walks the user through handing it
// to a coding agent. Picking an entry REPLACES this card with the
// chosen component via ctx.host.setSource, so the gallery is a
// transient "what should this card be?" step, not a lingering column.
//
// Dev mode additionally shows each entry's card source and a footer
// reminding that source can be typed straight into the chrome bar.
//
// Custom components come from the frontend store, whoever wrote them:
// each `<name>.json` that is a component contributes one row, and its
// stored `component_args` are what the row's card source passes. That
// is why a custom component *can* take arguments here, unlike a builtin
// — a builtin needing arguments still registers a parameter-less picker
// (documentView → documentPickerView).
import { watch } from "vue";
import type { CardRender } from "../types";
import {
  ensureFrontend,
  frontendManifest,
  gallerySource,
} from "../frontendRegistry";
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
      .gv { font: 13px/1.5 system-ui, -apple-system, sans-serif; color: var(--datalib-fg, inherit); }
      .gv-head { padding: 8px 12px; opacity: .6; border-bottom: 1px solid var(--datalib-border, #8884); }
      .gv-row { padding: 8px 12px; cursor: pointer; border-bottom: 1px solid var(--datalib-border, #8882); }
      .gv-row:hover { background: var(--datalib-hover, rgba(127,127,127,.12)); }
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

    function paint([manifest, dev]: [
      Map<string, Map<string, import("@/api").Meta>>,
      boolean,
    ]) {
      wrap.replaceChildren();
      const head = document.createElement("div");
      head.className = "gv-head";
      head.textContent = "pick what this card should show";
      wrap.appendChild(head);

      // One row per component in every namespace, with its own stored
      // arguments baked into the source the row expands to. Nothing
      // here knows or cares which namespace an applet wrote — `user`
      // and `slack_work` are read the same way.
      const custom: GalleryEntry[] = [];
      for (const [ns, entries] of [...manifest.entries()].sort((a, b) =>
        a[0].localeCompare(b[0]),
      )) {
        for (const [name, meta] of [...entries.entries()].sort((a, b) =>
          a[0].localeCompare(b[0]),
        )) {
          // A tombstone is a redirect, not something to offer.
          if ("renamed_to" in meta) continue;
          // An untitled component is one nobody meant to advertise.
          if (!meta.title.trim()) continue;
          custom.push({
            source: gallerySource(ns, name, meta.component_args),
            title: meta.title,
            description: meta.description,
          });
        }
      }

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

      for (const entry of [...BUILTIN_GALLERY, ...custom]) {
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

    void ensureFrontend();
    const stop = watch([frontendManifest, devMode], paint, { immediate: true });
    return () => stop();
  };
}
