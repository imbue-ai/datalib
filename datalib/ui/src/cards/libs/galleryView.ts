// Builtin view: the new-card gallery — the way every new card starts,
// in and out of edit mode. It lists, each with a short description and
// its icon: the composites (views/composites.ts — the Dashboard, the
// app's front door, first), then every parameter-less builtin
// cards/catalog.ts offers, then every titled component in the frontend
// store, then the "build a component with an agent" entry (handoff.ts),
// which mints a fresh component and walks the user through handing it
// to a coding agent. An entry whose metadata says `devTool` — the logs,
// the config, the pipeline graph, the agent entry — is listed after
// the rest under "Developer tools", a section with a heading and a
// shaded ground of its own; a building block of a composite (a
// Dashboard section) is one of them. Each group is in alphabetical
// order by title, whatever kind an entry is, with the agent entry last;
// a custom component carries a small mark saying it is one. Picking an
// entry REPLACES this card — with the chosen component via
// ctx.host.setSource, or with a copy of the composite via
// ctx.host.becomeComposite — so the gallery is a transient "what should
// this card be?" step, not a lingering column.
import { ref, watch } from "vue";
import type { CardRender } from "../types";
import { ensureFrontend, frontendManifest, gallerySource } from "../frontendRegistry";
import { createComponentWithAgent } from "@/handoff";
import { editMode } from "@/editMode";
import { byAudience, galleryBuiltins, type CardMeta } from "../catalog";
import { resolveIcon } from "../icons";
import { galleryComposites, loadComposites, savedComposites } from "@/views/composites";

// One row of the gallery, whatever it picks.
type GalleryRow = CardMeta & {
  // The card source the pick expands to, shown in edit mode; null for a
  // composite, which is copied rather than called.
  source: string | null;
  // A component from the frontend store, not one that ships with the app.
  custom?: boolean;
  pick: () => void;
};

function iconElement(token: string | null, cls = "gv-icon"): Element {
  const icon = resolveIcon(token);
  if (icon.kind === "image") {
    const img = document.createElement("img");
    img.className = cls;
    img.src = icon.url;
    img.alt = "";
    return img;
  }
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("class", cls);
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("aria-hidden", "true");
  const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute("fill", "currentColor");
  path.setAttribute("d", icon.path);
  svg.appendChild(path);
  return svg;
}

export function galleryView(): CardRender {
  return (root, ctx) => {
    ctx.setTitle("New card");
    const style = document.createElement("style");
    style.textContent = `
      :host { display: block; height: 100%; position: relative; }
      /* The host clips; the list scrolls in a box pinned to it, the way
         vueCard pins a Vue card's root. */
      .gv { position: absolute; inset: 0; overflow-y: auto; font: var(--datalib-font-size, 13px)/1.5 var(--datalib-font, system-ui, sans-serif); color: var(--datalib-fg, inherit); }
      .gv-head { display: flex; align-items: center; gap: 12px; padding: 8px 12px; border-bottom: 1px solid var(--datalib-border, #8884); }
      .gv-head-text { flex: 1 1 auto; opacity: .6; }
      .gv-row { display: flex; gap: 10px; align-items: flex-start; padding: 8px 12px; cursor: pointer; border-bottom: 1px solid var(--datalib-border, #8882); }
      .gv-icon { flex: 0 0 auto; width: 18px; height: 18px; margin-top: 1px; color: var(--datalib-accent); }
      .gv-text { flex: 1 1 auto; min-width: 0; }
      .gv-row:hover { background: var(--datalib-hover, rgba(127,127,127,.12)); }
      /* Title line: the dev-mode source shares the title's line while
         it fits (baseline-aligned flex) and wraps under it when the
         column is narrow — minimal layout shift vs non-dev. */
      .gv-head-line { display: flex; flex-wrap: wrap; align-items: baseline; column-gap: 10px; }
      .gv-title { font-weight: 600; }
      /* The mark of a custom component, after its title. */
      .gv-custom { align-self: center; width: 12px; height: 12px; color: var(--datalib-muted, #777); }
      .gv-desc { opacity: .65; }
      .gv-src { font: 11px/1.4 ui-monospace, Menlo, monospace; opacity: .5; }
      .gv-foot { padding: 8px 12px; opacity: .55; font-size: 12px; }
      /* The developer tools: one shaded block under its own heading, so
         it reads as a different kind of thing from the views above. */
      .gv-dev { background: color-mix(in srgb, var(--datalib-fg, #000) 6%, transparent); border-top: 1px solid var(--datalib-border, #8884); }
      .gv-dev-head { display: flex; align-items: baseline; gap: 8px; padding: 10px 12px 6px; font-weight: 600; }
      .gv-dev-note { font-weight: 400; opacity: .65; }
    `;
    root.appendChild(style);

    const wrap = document.createElement("div");
    wrap.className = "gv";
    root.appendChild(wrap);

    function paint([manifest, dev]: [
      Map<string, Map<string, import("@/api").Meta>>,
      boolean,
      unknown,
    ]) {
      wrap.replaceChildren();
      const head = document.createElement("div");
      head.className = "gv-head";
      const headText = document.createElement("span");
      headText.className = "gv-head-text";
      headText.textContent = "pick what this card should show";
      head.append(headText);
      wrap.appendChild(head);

      // One row per component in every namespace, with its own stored
      // arguments baked into the source the row expands to. Nothing
      // here knows or cares which namespace an applet wrote — `user`
      // and `slack_work` are read the same way.
      const rows: GalleryRow[] = [];
      for (const c of galleryComposites()) {
        rows.push({
          title: c.name,
          description: c.description,
          icon: c.icon,
          source: null,
          pick: () => ctx.host.becomeComposite(c.name),
        });
      }
      for (const entry of galleryBuiltins()) {
        rows.push({ ...entry, pick: () => ctx.host.setSource(entry.source) });
      }
      for (const [ns, entries] of manifest.entries()) {
        for (const [name, meta] of entries.entries()) {
          // A tombstone is a redirect, not something to offer.
          if ("renamed_to" in meta) continue;
          // An untitled component is one nobody meant to advertise.
          if (!meta.title.trim()) continue;
          const source = gallerySource(ns, name, meta.component_args);
          rows.push({
            source,
            title: meta.title,
            description: meta.description,
            icon: meta.icon ?? null,
            devTool: meta.dev_tool === true,
            custom: true,
            pick: () => ctx.host.setSource(source),
          });
        }
      }

      function addRow(into: Element, entry: GalleryRow) {
        const row = document.createElement("div");
        row.className = "gv-row";
        row.addEventListener("click", entry.pick);

        row.appendChild(iconElement(entry.icon));
        const text = document.createElement("div");
        text.className = "gv-text";
        const headLine = document.createElement("div");
        headLine.className = "gv-head-line";
        const titleEl = document.createElement("span");
        titleEl.className = "gv-title";
        titleEl.textContent = entry.title;
        headLine.appendChild(titleEl);
        if (entry.custom) {
          const mark = iconElement("component", "gv-custom");
          mark.setAttribute("role", "img");
          mark.setAttribute("aria-label", "custom component");
          mark.removeAttribute("aria-hidden");
          const tip = document.createElementNS("http://www.w3.org/2000/svg", "title");
          tip.textContent = "A custom component, stored in this library";
          mark.appendChild(tip);
          headLine.appendChild(mark);
        }
        // Edit mode: show what the pick expands to, teaching the
        // source-expression model row by row. Same line as the title
        // while it fits (see .gv-head-line).
        if (dev && entry.source !== null) {
          const code = document.createElement("span");
          code.className = "gv-src";
          code.textContent = entry.source;
          headLine.appendChild(code);
        }
        const desc = document.createElement("div");
        desc.className = "gv-desc";
        desc.textContent = entry.description;
        text.append(headLine, desc);
        row.appendChild(text);
        into.appendChild(row);
      }

      const { views, devTools } = byAudience(rows);
      for (const entry of views) addRow(wrap, entry);

      // The tools for working on the library itself, apart from the
      // views of its data.
      const section = document.createElement("section");
      section.className = "gv-dev";
      section.setAttribute("aria-label", "Developer tools");
      const heading = document.createElement("div");
      heading.className = "gv-dev-head";
      const note = document.createElement("span");
      note.className = "gv-dev-note";
      note.textContent = "logs, the config, the pipeline, components, building blocks";
      heading.append("Developer tools", note);
      section.appendChild(heading);
      for (const entry of devTools) addRow(section, entry);
      // Last, out of the alphabet: the escape hatch for when nothing
      // above fits. No source line in edit mode — the component name is
      // minted on pick.
      addRow(section, {
        title: "New component, built by an agent",
        description: "Create a fresh component and hand it to a coding agent to build.",
        icon: "component",
        source: null,
        pick: () => void createComponentWithAgent(ctx.host),
      });
      wrap.appendChild(section);

      if (dev) {
        const foot = document.createElement("div");
        foot.className = "gv-foot";
        foot.textContent =
          "edit mode: every card is a JS expression — you can also type " +
          "source directly into the box above and press Enter.";
        wrap.appendChild(foot);
      }
    }

    void ensureFrontend();
    void loadComposites();
    const stop = watch([frontendManifest, editMode, savedComposites], paint, {
      immediate: true,
    });
    return () => stop();
  };
}
