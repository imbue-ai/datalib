import { describe, expect, it } from "vitest";
import { injectCopyLinkButtons } from "../src/cards/chatSections.js";
import { renderDocument } from "../src/cards/renderDocument";

/// A message's controls sit together at its header's end, in one
/// `.msg-actions` the viewer's CSS shows while the pointer is over the
/// message: the copy-link button, then the ↗ the render wrote, last.

const LINK =
  '<a class="source-link" href="https://example.invalid/m/1" target="_blank" ' +
  'rel="noopener noreferrer">↗</a>';

function message(header: string): HTMLElement {
  const root = document.createElement("div");
  root.innerHTML = renderDocument([
    {
      uuid: "u1",
      class: "msg msg--email",
      md:
        `## <span class="msg-author">Riker</span> ` +
        `<time class="msg-ts" datetime="2364-04-11T09:00:00Z">Apr 11</time>${header}\n\n` +
        "Engage.\n\n",
    },
  ]).html;
  injectCopyLinkButtons(root);
  return root;
}

describe("a message's header actions", () => {
  it("hold the copy button, then the source link", () => {
    const h2 = message(` ${LINK}`).querySelector("h2")!;
    const actions = h2.querySelector(":scope > .msg-actions")!;
    expect(h2.lastElementChild).toBe(actions);
    expect([...actions.children].map((c) => c.className)).toEqual(["copy-link", "source-link"]);
    expect(h2.querySelector(":scope > .source-link")).toBeNull();
  });

  it("hold the copy button alone where there is no source link", () => {
    const root = message("");
    const actions = root.querySelector("h2 > .msg-actions")!;
    expect([...actions.children].map((c) => c.className)).toEqual(["copy-link"]);
    injectCopyLinkButtons(root);
    expect(root.querySelectorAll("button.copy-link")).toHaveLength(1);
  });
});
