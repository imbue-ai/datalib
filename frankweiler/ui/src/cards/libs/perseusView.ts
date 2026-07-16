// `perseusView()` in card source returns a CardRender for a lightweight
// scaife-like reading experience over the Perseus corpus (today:
// Thucydides' Histories). The card is a *control panel*, not a reader:
// down the left it shows a togglable list of "versions" — one per
// published edition/translation (`grc2`, `eng1`, `eng6`, `fre1`,
// `ger2`, …) — and a collapsible hierarchy of locators (book → chapter
// → section). Clicking any locator opens one reader panel per enabled
// version via `ctx.host.openCards(…)` — in the miller layout that lands
// the panels as columns to the right of this one (and re-clicking swaps
// them out), which is the scaife "open the same passage side-by-side in
// every version" gesture.
//
// All of the hierarchy + the version list is derived from one
// structured search (`source:Perseus`). Each grid row carries an
// `external_id` locator path — `"1"` (book), `"1.2"` (chapter),
// `"1.2.3"` (section) — plus a `kind` naming the edition
// (`"Chapter (perseus-grc2)"`, `"Section (1st1K-eng1)"`, …) and a
// `conversation_name` of the form `"<b>.<c> <edition-title>"`. A chapter
// row's `uuid`/`markdown_uuid` is the rendered chapter doc for that
// edition; a section row's `markdown_uuid` is the same chapter doc and
// its `uuid` is the anchor the reader scrolls to. So one fetch is
// enough to build the whole tree, list the editions, and know exactly
// what `documentView(...)` source each (locator, version) opens.
//
// Plain-DOM (no Vue), same shape as aliasView: paint once, then re-paint
// the parts that change on toggle/expand. Persisted state is the set of
// enabled versions (an opaque JSON string round-tripped through
// ctx.setState); expansion is transient in-memory UI only.
import { fetchSearch } from "@/api";
import type { CardRender } from "../types";
import { titled } from "../title";

// A "version" is one published edition/translation, keyed by its
// edition id (e.g. "perseus-grc2", "1st1K-eng1"). The id is parsed out
// of the grid row `kind` ("Chapter (perseus-grc2)"); the short toggle
// label is its suffix after the last "-" ("grc2", "eng1"); the readable
// title (tooltip) comes from `conversation_name`, which the renderer
// formats as "<b>.<c> <edition-title>", so we strip the leading
// locator. `lang` (the short's alphabetic prefix) only drives ordering
// — Greek editions sort first.
type Version = { id: string; short: string; title: string; lang: string };

type SectionNode = {
  n: string;
  // edition id -> (chapter doc uuid, section/sentence anchor uuid).
  // A missing id means that edition doesn't cover the section.
  byVer: Record<string, { md: string; anchor: string }>;
};
type ChapterNode = {
  n: string;
  // edition id -> rendered chapter doc uuid.
  byVer: Record<string, string>;
  sections: Map<string, SectionNode>;
};
type BookNode = {
  n: string;
  // The book index doc (edition-agnostic — one doc per book).
  md: string | null;
  chapters: Map<string, ChapterNode>;
};

// `kind` is "Chapter (<edition-id>)" / "Section (<edition-id>)" /
// "Book". Returns the edition id, or null for book rows.
function versionOfKind(kind: string): string | null {
  const m = kind.match(/\(([^)]+)\)\s*$/);
  return m ? m[1] : null;
}

// "perseus-grc2" -> "grc2"; the short, unique-per-work toggle label.
function shortOf(id: string): string {
  const i = id.lastIndexOf("-");
  return i >= 0 ? id.slice(i + 1) : id;
}

// "grc2" -> "grc": the alphabetic prefix, used only for ordering.
function langOf(short: string): string {
  return short.replace(/\d+$/, "");
}

// Strip the renderer's leading "<b>.<c> " locator from a row's
// conversation_name to recover the bare edition title.
function editionTitle(conversationName: string, short: string): string {
  const t = conversationName.replace(/^[\d.]+\s+/, "").trim();
  return t || short;
}

// Greek first, then by language, then by numeric suffix.
function cmpVersions(a: Version, b: Version): number {
  const ga = a.lang === "grc" ? 0 : 1;
  const gb = b.lang === "grc" ? 0 : 1;
  const na = Number.parseInt(a.short.replace(/\D+/g, ""), 10) || 0;
  const nb = Number.parseInt(b.short.replace(/\D+/g, ""), 10) || 0;
  return ga - gb || a.lang.localeCompare(b.lang) || na - nb;
}

function numCompare(a: string, b: string): number {
  const na = Number(a);
  const nb = Number(b);
  if (Number.isFinite(na) && Number.isFinite(nb) && na !== nb) return na - nb;
  return a.localeCompare(b);
}

// `documentView("md")` / `documentView("md", "anchor")` card source.
function docSource(md: string, anchor: string | null): string {
  const args = anchor === null ? [md] : [md, anchor];
  return `documentView(${args.map((a) => JSON.stringify(a)).join(", ")})`;
}

export function perseusView(): CardRender {
  return titled("Perseus reader", (root, ctx) => {
    const style = document.createElement("style");
    style.textContent = `
      .sv { font: 13px/1.5 ui-monospace, Menlo, monospace; color: var(--fw-fg, inherit); height: 100%; overflow: auto; }
      .sv-head { padding: 8px 12px; opacity: .7; border-bottom: 1px solid var(--fw-border, #8884); font-weight: 600; }
      .sv-versions { display: flex; gap: .4rem; padding: 8px 12px; border-bottom: 1px solid var(--fw-border, #8884); flex-wrap: wrap; }
      .sv-ver { border: 1px solid var(--fw-border, #8886); border-radius: 999px; padding: 2px 10px; cursor: pointer; background: transparent; color: inherit; font: inherit; opacity: .55; }
      .sv-ver[aria-pressed="true"] { opacity: 1; background: var(--fw-hover, rgba(99,102,241,.18)); border-color: rgba(99,102,241,.6); }
      .sv-tree { padding: 4px 0 16px; }
      .sv-row { display: flex; align-items: baseline; gap: .35rem; padding: 3px 12px; cursor: pointer; white-space: nowrap; }
      .sv-row:hover { background: var(--fw-hover, rgba(127,127,127,.12)); }
      .sv-tw { flex: 0 0 1.1em; opacity: .5; text-align: center; border-radius: 3px; }
      .sv-tw.leaf { opacity: 0; }
      .sv-tw.toggle { cursor: pointer; }
      .sv-tw.toggle:hover { opacity: 1; background: var(--fw-hover, rgba(127,127,127,.2)); }
      .sv-label { overflow: hidden; text-overflow: ellipsis; }
      .sv-book > .sv-label { font-weight: 600; }
      .sv-empty { padding: 16px 12px; opacity: .5; white-space: normal; }
    `;
    root.appendChild(style);

    const wrap = document.createElement("div");
    wrap.className = "sv";
    root.appendChild(wrap);

    // --- persisted + transient state ---
    // Enabled edition ids. Seeded from saved state if any; ingest then
    // prunes ids the corpus no longer has and, if nothing survives,
    // picks defaults (Greek + first English). Pruning-then-defaulting
    // also recovers from stale saved state written before edition ids
    // existed (e.g. the old `["grc","eng"]`).
    const enabled = new Set<string>();
    try {
      const saved = JSON.parse(ctx.initialState || "{}") as { versions?: string[] };
      if (Array.isArray(saved.versions)) {
        for (const v of saved.versions) enabled.add(v);
      }
    } catch {
      // first load / malformed.
    }
    function persist() {
      ctx.host.setState(JSON.stringify({ versions: [...enabled] }));
    }

    // edition id -> Version, populated during ingest; `sortedVersions`
    // is the toggle order.
    const versions = new Map<string, Version>();
    let sortedVersions: Version[] = [];

    // Expansion is keyed by locator path ("1", "1.2"); transient.
    const expanded = new Set<string>();
    let books: BookNode[] = [];

    // --- data ---
    function noteVersion(id: string, conversationName: string) {
      if (versions.has(id)) return;
      const short = shortOf(id);
      versions.set(id, {
        id,
        short,
        lang: langOf(short),
        title: editionTitle(conversationName, short),
      });
    }

    function ingest(
      rows: {
        kind: string;
        external_id: string;
        uuid: string;
        markdown_uuid: string | null;
        conversation_name: string;
      }[],
    ) {
      const byNum = new Map<string, BookNode>();
      const book = (n: string): BookNode => {
        let b = byNum.get(n);
        if (!b) {
          b = { n, md: null, chapters: new Map() };
          byNum.set(n, b);
        }
        return b;
      };
      const chapter = (b: BookNode, n: string): ChapterNode => {
        let c = b.chapters.get(n);
        if (!c) {
          c = { n, byVer: {}, sections: new Map() };
          b.chapters.set(n, c);
        }
        return c;
      };
      for (const r of rows) {
        const parts = r.external_id.split(".");
        // Skip TEI front/back matter (non-numeric locators like
        // "front"/"back") that some editions carry — they render as a
        // phantom "Book 0" with no content in most editions. Real
        // Thucydides locators are all numeric.
        if (!parts.every((p) => /^\d+$/.test(p))) continue;
        const ver = versionOfKind(r.kind);
        if (parts.length === 1) {
          // Book index doc — edition-agnostic.
          book(parts[0]).md = r.markdown_uuid ?? r.uuid;
        } else if (parts.length === 2 && ver) {
          chapter(book(parts[0]), parts[1]).byVer[ver] = r.markdown_uuid ?? r.uuid;
          noteVersion(ver, r.conversation_name);
        } else if (parts.length === 3 && ver) {
          const c = chapter(book(parts[0]), parts[1]);
          let s = c.sections.get(parts[2]);
          if (!s) {
            s = { n: parts[2], byVer: {} };
            c.sections.set(parts[2], s);
          }
          s.byVer[ver] = { md: r.markdown_uuid ?? r.uuid, anchor: r.uuid };
          noteVersion(ver, r.conversation_name);
        }
      }
      books = [...byNum.values()].sort((a, b) => numCompare(a.n, b.n));
      sortedVersions = [...versions.values()].sort(cmpVersions);
      // Drop persisted ids no longer present in the corpus.
      for (const id of [...enabled]) if (!versions.has(id)) enabled.delete(id);
      // If nothing is enabled (fresh card, or saved ids were all stale),
      // default to the Greek original plus the first English translation.
      if (enabled.size === 0 && sortedVersions.length) {
        const grc = sortedVersions.find((v) => v.lang === "grc");
        const eng = sortedVersions.find((v) => v.lang === "eng");
        if (grc) enabled.add(grc.id);
        if (eng) enabled.add(eng.id);
        // else: no grc/eng (e.g. only fre/ger) — leave the first one on.
        if (enabled.size === 0) enabled.add(sortedVersions[0].id);
      }
    }

    // --- opening panels ---
    const enabledVersions = (): Version[] =>
      sortedVersions.filter((v) => enabled.has(v.id));

    function openChapter(c: ChapterNode) {
      const sources = enabledVersions()
        .map((v) => c.byVer[v.id])
        .filter((md): md is string => !!md)
        .map((md) => docSource(md, null));
      if (sources.length) ctx.host.openCards(...sources);
    }
    function openSection(s: SectionNode) {
      const sources = enabledVersions()
        .map((v) => s.byVer[v.id])
        .filter((x): x is { md: string; anchor: string } => !!x)
        .map((x) => docSource(x.md, x.anchor));
      if (sources.length) ctx.host.openCards(...sources);
    }
    function openBook(b: BookNode) {
      if (b.md) ctx.host.openCards(docSource(b.md, null));
    }

    function toggle(path: string) {
      if (expanded.has(path)) expanded.delete(path);
      else expanded.add(path);
      paint();
    }

    // --- rendering ---
    // The twiddle and the label are separate click targets: clicking
    // the twiddle only expands/collapses (it stops propagation), and
    // clicking anywhere else on the row only opens the locator. So you
    // can drill into a book/chapter's children without opening it, and
    // open it without expanding.
    function row(opts: {
      cls: string;
      indent: number;
      twiddle: "open" | "closed" | "leaf";
      label: string;
      onOpen: () => void;
      onToggle?: () => void;
    }): HTMLElement {
      const el = document.createElement("div");
      el.className = `sv-row ${opts.cls}`;
      el.style.paddingLeft = `${12 + opts.indent * 14}px`;
      const tw = document.createElement("span");
      const interactive = opts.twiddle !== "leaf" && !!opts.onToggle;
      tw.className =
        "sv-tw" +
        (opts.twiddle === "leaf" ? " leaf" : "") +
        (interactive ? " toggle" : "");
      tw.textContent =
        opts.twiddle === "open" ? "▾" : opts.twiddle === "closed" ? "▸" : "•";
      if (interactive) {
        tw.addEventListener("click", (e) => {
          e.stopPropagation();
          opts.onToggle!();
        });
      }
      const lbl = document.createElement("span");
      lbl.className = "sv-label";
      lbl.textContent = opts.label;
      el.append(tw, lbl);
      el.addEventListener("click", opts.onOpen);
      return el;
    }

    function paintVersions(): HTMLElement {
      const bar = document.createElement("div");
      bar.className = "sv-versions";
      for (const v of sortedVersions) {
        const btn = document.createElement("button");
        btn.className = "sv-ver";
        btn.type = "button";
        btn.textContent = v.short;
        // Full edition title (translator etc.) on hover.
        btn.title = v.title;
        btn.setAttribute("aria-pressed", String(enabled.has(v.id)));
        btn.addEventListener("click", () => {
          if (enabled.has(v.id)) enabled.delete(v.id);
          else enabled.add(v.id);
          persist();
          paint();
        });
        bar.appendChild(btn);
      }
      return bar;
    }

    // A locator is shown only if at least one *enabled* edition covers
    // it — editions don't all share the same structure, so the union of
    // every edition's locators would list chapters/sections that the
    // selected editions can't open. These recompute on every toggle.
    const chapterShown = (c: ChapterNode): boolean =>
      enabledVersions().some((v) => c.byVer[v.id] !== undefined);
    const sectionShown = (s: SectionNode): boolean =>
      enabledVersions().some((v) => s.byVer[v.id] !== undefined);
    const bookShown = (b: BookNode): boolean =>
      [...b.chapters.values()].some(chapterShown);

    function paintTree(): HTMLElement {
      const tree = document.createElement("div");
      tree.className = "sv-tree";
      const shownBooks = books.filter(bookShown);
      if (shownBooks.length === 0) {
        const empty = document.createElement("div");
        empty.className = "sv-empty";
        empty.textContent = books.length
          ? "no locators in the selected versions — enable a version above."
          : "no Perseus data in this root — add a `perseus` source and sync.";
        tree.appendChild(empty);
        return tree;
      }
      for (const b of shownBooks) {
        const bPath = b.n;
        const bOpen = expanded.has(bPath);
        const shownChapters = [...b.chapters.values()]
          .filter(chapterShown)
          .sort((x, y) => numCompare(x.n, y.n));
        tree.appendChild(
          row({
            cls: "sv-book",
            indent: 0,
            twiddle: shownChapters.length ? (bOpen ? "open" : "closed") : "leaf",
            label: `Book ${b.n}`,
            onOpen: () => openBook(b),
            onToggle: shownChapters.length ? () => toggle(bPath) : undefined,
          }),
        );
        if (!bOpen) continue;
        for (const c of shownChapters) {
          const cPath = `${b.n}.${c.n}`;
          const cOpen = expanded.has(cPath);
          const shownSections = [...c.sections.values()]
            .filter(sectionShown)
            .sort((x, y) => numCompare(x.n, y.n));
          tree.appendChild(
            row({
              cls: "sv-chapter",
              indent: 1,
              twiddle: shownSections.length ? (cOpen ? "open" : "closed") : "leaf",
              label: `Chapter ${c.n}`,
              onOpen: () => openChapter(c),
              onToggle: shownSections.length ? () => toggle(cPath) : undefined,
            }),
          );
          if (!cOpen) continue;
          for (const s of shownSections) {
            tree.appendChild(
              row({
                cls: "sv-section",
                indent: 2,
                twiddle: "leaf",
                label: `§ ${s.n}`,
                onOpen: () => openSection(s),
              }),
            );
          }
        }
      }
      return tree;
    }

    let loading = true;
    function paint() {
      wrap.replaceChildren();
      const head = document.createElement("div");
      head.className = "sv-head";
      head.textContent = "Perseus";
      wrap.append(head, paintVersions());
      if (loading) {
        const l = document.createElement("div");
        l.className = "sv-empty";
        l.textContent = "loading…";
        wrap.appendChild(l);
        return;
      }
      wrap.appendChild(paintTree());
    }

    paint();

    // One structured search returns every Perseus row; limit is set
    // high enough for the full Histories across all editions (8 books ×
    // chapters × sections × ~13 editions). The free-text portion is
    // empty, so this routes through the SQL filter path, not qmd ranking.
    const ac = new AbortController();
    void fetchSearch("source:Perseus", 200000, ac.signal)
      .then((resp) => {
        ingest(resp.rows);
        loading = false;
        paint();
      })
      .catch(() => {
        // fetchSearch already surfaces a toast; just drop the spinner.
        loading = false;
        paint();
      });

    return () => ac.abort();
  });
}
