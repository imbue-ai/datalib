import { describe, expect, it } from "vitest";
import type { BodyNode, BodyPiece as Piece } from "../src/api";
import { sectionEl } from "../src/cards/chatSections.js";
import { bodyOf, renderDocument } from "../src/cards/renderDocument";

/// A message is rendered as a fragment of its own, in a box the viewer
/// draws, so markup one message leaves open cannot hide or replace the
/// next, and a copy of another message's id inside it never answers in
/// that message's place (docs/dev/data_architecture_parse_and_render.md
/// §"One section cannot reach the next"). Each case is checked twice: as
/// pieces, where the next message must come through whole, and joined
/// into one string with each box drawn, where the same text does reach it
/// — so a green run is the pieces at work, not a body too tame to do harm.

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

function page(pieces: BodyNode[], asPieces: boolean): HTMLElement {
  const root = document.createElement("div");
  const body = asPieces ? pieces : bodyOf({ pieces });
  root.innerHTML = renderDocument(body).html;
  return root;
}

/// What the reader is shown of the message `uuid`: the element that
/// answers to it, what it says, and whether something hides it.
function shown(root: HTMLElement, uuid: string) {
  const el = sectionEl(root, uuid);
  const hidden = !!el?.parentElement?.closest('[style*="display"]');
  return { text: el?.textContent?.trim() ?? null, hidden };
}

const WHOLE = { text: "Picard\nMake it so.", hidden: false };

/// The element naming `uuid` that is not the one answering to it.
function copyOf(root: HTMLElement, uuid: string): Element | undefined {
  const answering = sectionEl(root, uuid);
  return [...root.querySelectorAll(`[data-section-uuid="${uuid}"]`)].find((e) => e !== answering);
}

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
    // The copy is still there to read, inside Quark's own box, and the
    // box Picard's id finds is the one the viewer drew.
    expect(copyOf(root, "u2")?.parentElement?.closest("[data-section-uuid]")).toBe(
      sectionEl(root, "u1"),
    );
    const top = [...root.querySelectorAll(":scope > .msg[data-section-uuid]")];
    expect(top.map((e) => e.getAttribute("data-section-uuid"))).toEqual(["u1", "u2"]);
  });

  it("control: the same body joined into one string hides the next message", () => {
    const got = shown(page([TITLE, unclosed, PICARD], false), "u2");
    expect(got.text, "the copy answers to Picard's id").toBe("Picard\nPay Quark double.");
    expect(got.hidden, "it sits in the hidden div").toBe(true);
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
    expect(sectionEl(root, "u1")?.textContent).toContain("Pay Quark double.");
  });

  it("control: joined, the early close lets the copy pose as the next message", () => {
    const root = page([TITLE, closesEarly, PICARD], false);
    expect(shown(root, "u2").text, "a row click on Picard lands here").toContain(
      "Pay Quark double.",
    );
  });

  it("a fold is drawn around its pieces, and what one holds cannot reach past it", () => {
    const step = boxed("u3", "Data", "Running a level-3 diagnostic.");
    const root = page(
      [TITLE, { fold: "tool-group", summary: "🛠 2 tool steps", pieces: [unclosed, step] }, PICARD],
      true,
    );
    const fold = root.querySelector(":scope > details.tool-group")!;
    expect(fold.querySelector(":scope > summary")?.textContent).toBe("🛠 2 tool steps");
    expect(fold.querySelector(':scope > [data-section-uuid="u3"]')).toBe(sectionEl(root, "u3"));
    expect(shown(root, "u2")).toEqual(WHOLE);
    expect(root.querySelector(':scope > [data-section-uuid="u2"]')).toBe(sectionEl(root, "u2"));
  });

  it("a fold's summary is sanitized like any other text", () => {
    const root = page(
      [{ fold: "tool-group", summary: '<img src=x onerror="alert(1)">steps', pieces: [PICARD] }],
      true,
    );
    expect(root.querySelector("summary")?.innerHTML).not.toContain("onerror");
  });

  it("a piece between messages cannot hide the next either", () => {
    const between: Piece = { uuid: null, md: '<div style="display:none">\n\n' };
    const root = page([TITLE, boxed("u1", "Quark", "Latinum is up."), between, PICARD], true);
    expect(shown(root, "u2")).toEqual(WHOLE);
  });

  it("a piece in no box is still a fragment of its own", () => {
    const bare: Piece = { uuid: "u1", md: 'Latinum is up.\n\n<div style="display:none">\n\n' };
    const root = page([TITLE, bare, PICARD], true);
    expect(shown(root, "u2")).toEqual(WHOLE);
  });

  it("a piece in no box answers to its own wrapper, and copies inside it to nothing", () => {
    const drawnByItself: Piece = {
      uuid: "u1",
      md:
        '<div class="diff-added"><div id="m-u1" data-section-uuid="u1" class="msg">\n\n' +
        'Latinum is up.\n\n<div id="m-u2" data-section-uuid="u2">A copy.</div>\n\n</div></div>\n\n',
    };
    const root = page([TITLE, drawnByItself, PICARD], true);
    expect(sectionEl(root, "u1")?.classList.contains("msg")).toBe(true);
    expect(shown(root, "u2")).toEqual(WHOLE);
  });

  it("an id named only inside a message, as a reaction is, answers there", () => {
    const reacted = boxed(
      "u1",
      "Quark",
      'Latinum is up.\n\n- <span id="m-r1" data-section-uuid="r1">👍 Picard</span>',
    );
    const root = page([TITLE, reacted, PICARD], true);
    expect(sectionEl(root, "r1")?.textContent).toBe("👍 Picard");
  });
});
