import { describe, expect, it } from "vitest";
import type { BodySection } from "../src/api";
import { renderDocument } from "../src/cards/renderDocument";

/// The text-edge library (`datalib/backend/etl/text_edges/`): each case's
/// text, rendered by `render_hostile_samples` as the first of two
/// messages the way its source renders it, judged here by the app's own
/// markdown-it and sanitizer, and by what search holds of it. A case
/// marked `fails_today` must still fail, so the mark goes as soon as the
/// fix lands.
///
/// Bazel puts the corpus beside this file; outside Bazel it is missing and
/// this suite fails rather than passing on nothing.

type Section = { uuid: string | null; md: string };
type Rendered = {
  file: string;
  body: string;
  sections: Section[] | null;
  grid_text: string;
  next_uuid: string;
  next_text: string;
};
type Case = {
  id: string;
  edge: string;
  enters: string;
  text: string;
  shows: string[];
  hides: string[];
  finds: string[];
  elements: Record<string, number>;
  fails_today: string | null;
  rendered: Rendered;
};
type Edges = { cases: Case[]; baselines: Record<string, Rendered> };

const corpusPath = "./hostile_samples.json";
const edges: Edges = (await import(/* @vite-ignore */ corpusPath)).default.edges;

/// The first message: the case's text. Its id is fixed by the generator.
const FIRST = "00000000-0000-8000-8000-0000000e0001";

function page(r: Rendered): HTMLElement {
  const sections: BodySection[] | null =
    r.sections?.map((s) => ({ uuid: s.uuid, len: s.md.length })) ?? null;
  const root = document.createElement("div");
  root.innerHTML = renderDocument(r.body, { sections }).html;
  return root;
}

/// What the case's own text drew: its message's wrapper, or the whole
/// page where the renderer declares no messages.
function scope(root: HTMLElement, r: Rendered): Element {
  return (r.sections && root.querySelector(`[data-section-uuid="${FIRST}"]`)) || root;
}

/// The words a reader sees, a `<br>` as a line break.
function seen(el: Element): string {
  const copy = el.cloneNode(true) as Element;
  for (const br of copy.querySelectorAll("br")) br.replaceWith(" ");
  return (copy.textContent ?? "").replace(/\s+/g, " ").trim();
}

/// Words as a keyword index splits them: letters and digits, case
/// ignored, everything else a break.
function words(s: string): string[] {
  return s
    .toLowerCase()
    .split(/[^\p{L}\p{N}]+/u)
    .filter(Boolean);
}

/// Whether a quoted search for `phrase` matches `text`: its words, in
/// order, next to each other.
function phraseIn(phrase: string, text: string): boolean {
  const want = words(phrase);
  const have = words(text);
  return have.some((_, i) => want.every((w, j) => have[i + j] === w));
}

function hidden(el: Element | null): boolean {
  for (let e = el; e; e = e.parentElement) {
    const style = (e.getAttribute("style") ?? "").replace(/\s/g, "");
    if (e.hasAttribute("hidden") || /display:none|visibility:hidden/i.test(style)) return true;
  }
  return false;
}

/// Everything wrong with a case's page and search text; empty when the
/// case holds.
function problems(c: Case, baseline: Rendered): string[] {
  const r = c.rendered;
  const out: string[] = [];
  const root = page(r);
  const mine = scope(root, r);
  const text = seen(mine);

  for (const s of c.shows) {
    if (!text.includes(s)) out.push(`does not show ${JSON.stringify(s)}: ${JSON.stringify(text)}`);
  }
  for (const s of c.hides) {
    if (text.includes(s)) out.push(`shows ${JSON.stringify(s)}: ${JSON.stringify(text)}`);
  }

  const base = page(baseline);
  const baseScope = scope(base, baseline);
  for (const [selector, n] of Object.entries(c.elements)) {
    const added =
      mine.querySelectorAll(selector).length - baseScope.querySelectorAll(selector).length;
    if (added !== n) out.push(`adds ${added} <${selector}>, not ${n}`);
  }

  for (const phrase of c.finds) {
    if (!phraseIn(phrase, r.file)) out.push(`qmd's words miss ${JSON.stringify(phrase)}`);
    if (!phraseIn(phrase, r.grid_text)) {
      out.push(`the grid's text misses ${JSON.stringify(phrase)}: ${JSON.stringify(r.grid_text)}`);
    }
  }

  out.push(...nextMessageProblems(root, r));
  if (root.querySelector("script, iframe, form")) out.push("a script, frame or form survived");
  for (const el of root.querySelectorAll("*")) {
    for (const a of el.getAttributeNames()) {
      if (a.startsWith("on")) out.push(`an ${a} attribute survived on <${el.localName}>`);
    }
  }
  return out;
}

/// The message after the case's must come through whole: shown, not in
/// code, and the only element that answers to its id.
function nextMessageProblems(root: HTMLElement, r: Rendered): string[] {
  const out: string[] = [];
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  const holders: Element[] = [];
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    if (n.textContent?.includes(r.next_text) && n.parentElement) holders.push(n.parentElement);
  }
  const shown = holders.filter((h) => !hidden(h) && !h.closest("pre, code"));
  if (shown.length === 0) {
    out.push(`the next message's ${JSON.stringify(r.next_text)} is not shown as text`);
  }
  if (r.sections) {
    const answering = root.querySelectorAll(`[data-section-uuid="${r.next_uuid}"]`);
    const ids = root.querySelectorAll(`[id="m-${r.next_uuid}"]`);
    if (answering.length !== 1 || ids.length !== 1) {
      out.push(`${answering.length} elements answer to the next message, ${ids.length} by id`);
    }
    if (!shown.some((h) => h.closest("[data-section-uuid]") === answering[0])) {
      out.push("the next message's text is not in its own wrapper");
    }
  }
  return out;
}

describe("text edges", () => {
  it("has cases, and each kind has a baseline", () => {
    expect(edges.cases.length).toBeGreaterThan(0);
    for (const c of edges.cases) expect(edges.baselines[c.enters], c.enters).toBeDefined();
  });

  it.each(edges.cases.map((c) => [c.id, c] as const))(
    "%s",
    (_, c) => {
      const r = c.rendered;
      if (r.sections) expect(r.sections.map((s) => s.md).join("")).toBe(r.body);
      const found = problems(c, edges.baselines[c.enters]);
      if (c.fails_today) {
        expect(found, `${c.id} now holds; take off its fails_today (${c.fails_today})`).not.toEqual(
          [],
        );
      } else {
        expect(found, `${c.id}: ${c.edge}\n${found.join("\n")}`).toEqual([]);
      }
    },
    60_000,
  );
});
