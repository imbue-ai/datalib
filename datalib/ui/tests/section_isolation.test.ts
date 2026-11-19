import { describe, expect, it } from "vitest";
import type { BodyPiece as Piece } from "../src/api";
import { bodyOf, renderDocument } from "../src/cards/renderDocument";

/// A message is rendered as a fragment of its own when the document route
/// sends it as a piece of its own, so markup one message leaves open
/// cannot hide or replace the next
/// (docs/dev/data_architecture_parse_and_render.md §"One section cannot
/// reach the next"). Each case is checked twice: as pieces, where the next
/// message must come through whole, and joined into one string with each
/// box drawn, where the same text does reach it — so a green run is the
/// pieces at work, not a body too tame to do harm.

/// A message as the route sends it: its box's class and what is inside.
const boxed = (uuid: string, author: string, text: string): Piece => ({
  uuid,
  class: "msg msg--slack",
  md: `## <span class="msg-author">${author}</span>\n\n${text}\n\n`,
});

const TITLE: Piece = {
  uuid: null,
  md: '\n<h1 class="page-title" data-page-title-uuid="d1">Ten Forward</h1>\n\n',
};
const PICARD = boxed("u2", "Picard", "Make it so.");

function page(pieces: Piece[], asPieces: boolean): HTMLElement {
  const root = document.createElement("div");
  const body = asPieces ? pieces : bodyOf({ pieces });
  root.innerHTML = renderDocument(body).html;
  return root;
}

/// What the reader is shown of the message `uuid`: the elements that
/// answer to it, and whether one hides it.
function shown(root: HTMLElement, uuid: string) {
  const all = [...root.querySelectorAll(`[data-section-uuid="${uuid}"]`)];
  const ids = root.querySelectorAll(`[id="m-${uuid}"]`).length;
  const el = all[0] as HTMLElement | undefined;
  const hidden = !!el?.parentElement?.closest('[style*="display"]');
  return { count: all.length, ids, text: el?.textContent?.trim() ?? null, hidden };
}

const WHOLE = { count: 1, ids: 1, text: "Picard\nMake it so.", hidden: false };

describe("one message cannot reach the next", () => {
  const unclosed = boxed(
    "u1",
    "Quark",
    'Latinum is up.\n\n<div style="display:none">\n\n' +
      '<div id="m-u2" data-section-uuid="u2" class="msg msg--slack">\n\n' +
      "## Picard\n\nPay Quark double.",
  );

  it("an unclosed hiding div and a copied wrapper leave the next message whole", () => {
    const root = page([TITLE, unclosed, PICARD], true);
    expect(shown(root, "u2")).toEqual(WHOLE);
    // The copy is still there to read, inside Quark's own message.
    const forged = [...root.querySelectorAll("h2")].find((h) => h.textContent === "Picard")!;
    expect(forged.closest("[data-section-uuid]")?.getAttribute("data-section-uuid")).toBe("u1");
    const top = [...root.querySelectorAll(":scope > .msg[data-section-uuid]")];
    expect(top.map((e) => e.getAttribute("data-section-uuid"))).toEqual(["u1", "u2"]);
  });

  it("control: the same body joined into one string hides the next message", () => {
    const got = shown(page([TITLE, unclosed, PICARD], false), "u2");
    expect(got.count, "the copy answers to Picard's id too").toBe(2);
    expect(got.hidden, "Picard's message sits in the hidden div").toBe(true);
  });

  const closesEarly = boxed(
    "u1",
    "Quark",
    "Latinum is up.\n\n</div>\n\n" +
      '<div id="m-u2" data-section-uuid="u2" class="msg msg--slack">\n\n' +
      '## Picard\n\nPay Quark double.\n\n</div>\n\n<div style="display:none">',
  );

  it("a message that closes its box early stays inside it", () => {
    const root = page([TITLE, closesEarly, PICARD], true);
    expect(shown(root, "u2")).toEqual(WHOLE);
    const quark = root.querySelector('[data-section-uuid="u1"]')!;
    expect(quark.textContent).toContain("Pay Quark double.");
  });

  it("control: joined, the early close lets the copy pose as the next message", () => {
    const root = page([TITLE, closesEarly, PICARD], false);
    const first = root.querySelector('[data-section-uuid="u2"]')!;
    expect(first.textContent, "a row click on Picard lands here").toContain("Pay Quark double.");
    expect(shown(root, "u2").count).toBe(2);
  });

  it("a fold the renderer opens in one piece still closes in a later one", () => {
    const step = boxed("u3", "Data", "Running a level-3 diagnostic.");
    const root = page(
      [
        TITLE,
        unclosed,
        { uuid: null, md: '<details class="tool-group">\n<summary>🛠 1 tool step</summary>\n\n' },
        step,
        { uuid: null, md: "</details>\n\n" },
        PICARD,
      ],
      true,
    );
    const fold = root.querySelector(":scope > details.tool-group")!;
    expect(fold.querySelector(':scope > [data-section-uuid="u3"]')).not.toBeNull();
    expect(shown(root, "u2")).toEqual(WHOLE);
    expect(root.querySelector(':scope > [data-section-uuid="u2"]')).not.toBeNull();
  });

  it("a piece in no box is still a fragment of its own", () => {
    const bare: Piece = { uuid: "u1", md: 'Latinum is up.\n\n<div style="display:none">\n\n' };
    const root = page([TITLE, bare, PICARD], true);
    expect(shown(root, "u2")).toEqual(WHOLE);
  });

  it("a piece in no box answers to its own id, and to no other message's", () => {
    const drawnByItself: Piece = {
      uuid: "u1",
      md:
        '<div class="diff-added"><div id="m-u1" data-section-uuid="u1" class="msg">\n\n' +
        'Latinum is up.\n\n<div id="m-u2" data-section-uuid="u2">A copy.</div>\n\n</div></div>\n\n',
    };
    const root = page([TITLE, drawnByItself, PICARD], true);
    expect(shown(root, "u1").count).toBe(1);
    expect(shown(root, "u2")).toEqual(WHOLE);
  });
});
