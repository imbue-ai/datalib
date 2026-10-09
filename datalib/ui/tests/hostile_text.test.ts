import { describe, expect, it } from "vitest";
import { renderDocument } from "../src/cards/renderDocument";

/// Upstream text that is plain where it came from has to read on the page
/// exactly as typed, whatever HTML or markdown it holds: a sender named
/// `[x](https://evil)` must not become a link, nor a subject holding a
/// blank line end the HTML block it sits in. The corpus is written by
/// `//datalib/backend/etl/hostile_samples:render_hostile_samples` (every
/// shared renderer, and the escape helpers over generated strings), and
/// judged here by the app's own markdown-it and sanitizer.
///
/// Bazel puts the corpus beside this file; outside Bazel it is missing and
/// this suite fails rather than passing on nothing.

type Expect = { field: string; typed: string };
type Doc = { name: string; md: string; expect: Expect[] };
type Escape = { helper: string; input: string; md: string };
type Corpus = { documents: Doc[]; escapes: Escape[] };

const corpusPath = "./hostile_samples.json";
const corpus: Corpus = (await import(/* @vite-ignore */ corpusPath)).default;

const squash = (s: string) => s.replace(/\s+/g, " ").trim();

/// The page's HTML goes through the parser twice — markdown-it's output
/// into the sanitizer, the sanitizer's back into the page — and the
/// parser reads a carriage return as a line feed.
const asParsed = (s: string) => s.replace(/\r\n?/g, "\n");

/// The rendered page. A `<br>` is a line break in what a reader sees, so
/// it stands in the text as one.
function page(md: string): HTMLElement {
  const div = document.createElement("div");
  div.innerHTML = renderDocument(md, null).html;
  for (const br of div.querySelectorAll("br")) br.replaceWith("\n");
  return div;
}

const SLOW = 60_000;

/// The app strips front matter the same way: a `---` line opens it and the
/// next line that is `---` by itself closes it.
function splitFrontMatter(md: string): { front: string[]; body: string } {
  if (!md.startsWith("---\n")) return { front: [], body: md };
  const lines = md.split("\n");
  const close = lines.indexOf("---", 1);
  if (close < 0) return { front: [], body: md };
  return { front: lines.slice(1, close), body: lines.slice(close + 1).join("\n") };
}

/// Elements a renderer's own layout makes. Anything else came from text.
const LAYOUT = new Set([
  "DIV",
  "P",
  "H1",
  "H2",
  "H3",
  "SPAN",
  "TIME",
  "SMALL",
  "EM",
  "STRONG",
  "UL",
  "LI",
  "TABLE",
  "THEAD",
  "TBODY",
  "TR",
  "TH",
  "TD",
  "CODE",
  "A",
  "BR",
  "HR",
  "DETAILS",
  "SUMMARY",
  "BLOCKQUOTE",
]);

/// A link is honest when it shows where it goes, as linkify's do.
function disguisedLinks(root: HTMLElement): string[] {
  return [...root.querySelectorAll("a")]
    .filter((a) => /e\.test/.test(a.getAttribute("href") ?? ""))
    .filter((a) => squash(a.textContent ?? "") !== a.getAttribute("href"))
    .map((a) => a.outerHTML);
}

describe("hostile text in every shared renderer", () => {
  it("has a corpus", () => {
    expect(corpus.documents.length).toBeGreaterThan(5);
    expect(corpus.escapes.length).toBeGreaterThan(1000);
  });

  for (const doc of corpus.documents) {
    describe(doc.name, () => {
      const { front, body } = splitFrontMatter(doc.md);

      it("keeps every front-matter value on its own line", () => {
        for (const line of front) expect(line).toMatch(/^[a-z][a-z0-9_]*: \S/);
      });

      const root = page(body);
      const text = squash(root.textContent ?? "");

      for (const { field, typed } of doc.expect) {
        it(`shows the ${field} as typed`, () => {
          expect(text).toContain(squash(typed));
        });
      }

      it("makes no image and no link that hides where it goes", () => {
        expect(root.querySelectorAll("img").length).toBe(0);
        expect(disguisedLinks(root)).toEqual([]);
      });

      it("makes no element its layout does not", () => {
        const foreign = [...root.querySelectorAll("*")]
          .filter((el) => !LAYOUT.has(el.tagName))
          .map((el) => el.outerHTML);
        expect(foreign).toEqual([]);
      });
    });
  }
});

describe("escape helpers over generated strings", () => {
  const fails = (helper: string, check: (e: Escape, root: HTMLElement) => boolean) =>
    corpus.escapes
      .filter((e) => e.helper === helper)
      .filter((e) => !check(e, page(e.md)))
      .map((e) => ({ input: e.input, md: e.md }));

  const only = (root: HTMLElement, tags: string[]) =>
    [...root.querySelectorAll("*")].every((el) => tags.includes(el.tagName));

  it(
    "escape_md_inline: one line of text, as typed",
    () => {
      expect(
        fails(
          "escape_md_inline",
          (e, root) =>
            squash(root.textContent ?? "") === squash(`p: ${e.input}`) && only(root, ["P"]),
        ),
      ).toEqual([]);
    },
    SLOW,
  );

  it(
    "escape_md_block: lines of text, as typed",
    () => {
      expect(
        fails(
          "escape_md_block",
          (e, root) => squash(root.textContent ?? "") === squash(e.input) && only(root, ["P"]),
        ),
      ).toEqual([]);
    },
    SLOW,
  );

  it(
    "escape_text: the text of an HTML block, exactly",
    () => {
      expect(
        fails("escape_text", (e, root) => {
          const div = root.querySelector("div.t");
          return div !== null && div.textContent === asParsed(e.input) && only(root, ["DIV"]);
        }),
      ).toEqual([]);
    },
    SLOW,
  );

  // The sanitizer trims an attribute's value, and drops one holding
  // what could close a comment or a style or title element.
  const DROPPED_ATTR = /((--!?|\])>)|<\/(style|title)/i;
  it(
    "escape_attr: an attribute, exactly",
    () => {
      expect(
        fails("escape_attr", (e, root) => {
          const time = root.querySelector("time");
          const want = DROPPED_ATTR.test(e.input) ? null : asParsed(e.input).trim();
          return time !== null && time.getAttribute("title") === want;
        }),
      ).toEqual([]);
    },
    SLOW,
  );

  it(
    "md_code_span: a code span, as typed on one line",
    () => {
      expect(
        fails("md_code_span", (e, root) => {
          const code = root.querySelector("code");
          return code !== null && code.textContent === e.input.replace(/[\r\n]/g, " ");
        }),
      ).toEqual([]);
    },
    SLOW,
  );
});
