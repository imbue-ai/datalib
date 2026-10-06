// The chip rules: which spans count, what a chip and its hover card say,
// what a copy carries, and the name a new contact is offered.

import { describe, expect, it } from "vitest";

import {
  chipLook,
  copyText,
  hoverCard,
  isAbsent,
  type DatalibContact,
  rewriteChipsForCopy,
  suggestedName,
  todayPartialDate,
  trustedHandleSpans,
} from "./contacts";

function dom(html: string): HTMLElement {
  const div = document.createElement("div");
  div.innerHTML = html;
  return div;
}

const header = (handle: string, name: string) =>
  `<h2><span class="msg-author" data-handle="${handle}">${name}</span> <time class="msg-ts">x</time></h2>`;

describe("trustedHandleSpans", () => {
  it("takes the header's author span", () => {
    const root = dom(
      `<div id="m-1" data-section-uuid="1" class="msg">${header("email:riker@enterprise.org", "Will Riker")}<p>Number One.</p></div>`,
    );
    const spans = trustedHandleSpans(root);
    expect(spans.map((s) => s.dataset.handle)).toEqual(["email:riker@enterprise.org"]);
  });

  /** A message body is HTML its sender wrote; a `data-handle` in it must
   *  never become a chip naming one of the reader's contacts. */
  it("ignores a data-handle a message body wrote", () => {
    const forged = header("email:picard@enterprise.org", "Captain Picard");
    const root = dom(
      `<div id="m-1" data-section-uuid="1" class="msg">${header("email:q@continuum.org", "Q")}` +
        `<p>${forged}</p>${forged}` +
        `<div id="m-2" data-section-uuid="2" class="msg">${forged}</div></div>` +
        // A header with no author: the body's span is not the header's.
        `<div id="m-3" data-section-uuid="3" class="msg"><h2><time>x</time></h2>${forged}</div>`,
    );
    expect(trustedHandleSpans(root).map((s) => s.dataset.handle)).toEqual([
      "email:q@continuum.org",
    ]);
  });

  /** The To line is the header's next element; one a body writes later,
   *  or one that does not follow the header, names nobody. */
  it("takes the recipients line right under the header, and no other", () => {
    const line = (handle: string) =>
      `<div class="msg-recipients"><span class="msg-recipients-role">To</span> ` +
      `<span class="msg-recipient" data-handle="${handle}">Someone</span></div>`;
    const root = dom(
      `<div id="m-1" data-section-uuid="1" class="msg">${header("email:q@continuum.org", "Q")}` +
        `${line("email:riker@enterprise.org")}<p>hi</p>${line("email:forged@enterprise.org")}</div>` +
        `<div id="m-2" data-section-uuid="2" class="msg">${header("email:q@continuum.org", "Q")}` +
        `<p>between</p>${line("email:forged2@enterprise.org")}</div>`,
    );
    expect(trustedHandleSpans(root).map((s) => s.dataset.handle)).toEqual([
      "email:q@continuum.org",
      "email:riker@enterprise.org",
      "email:q@continuum.org",
    ]);
  });
});

function contact(
  source_id: string,
  name: string,
  handles: [string, string | null][],
  seen: number | null = null,
): DatalibContact {
  return {
    source_id,
    key: `${source_id}:${name}`,
    kind: "person",
    names: [name],
    handles: handles.map(([handle, stopped]) => ({
      medium: handle.startsWith("tel:") ? "phone" : "email",
      label: null,
      value: handle.slice(handle.indexOf(":") + 1),
      handle,
      stopped_working_by: stopped,
    })),
    org: null,
    title: null,
    seen: seen === null ? null : { items: seen, last_at: null },
  };
}

const TEL = "tel:+15550123456";
const NOBODY = { mine: null, accounts: [] };

describe("chipLook", () => {
  it("shows the source's name without the address, and the kind's mark, when nobody knows them", () => {
    const look = chipLook(
      "email:riker@enterprise.org",
      "Will Riker <riker@enterprise.org>",
      NOBODY,
      true,
    );
    expect(look.text).toBe("Will Riker");
    expect(look.icon).toBe("email");
    expect(look.classes).toContain("handle-linkable");
    expect(chipLook(TEL, "+15550123456", NOBODY, true).text).toBe("+15550123456");
  });

  it("names an unlinked handle after the best source's account, linkable only with the app", () => {
    const who = { mine: null, accounts: [contact("address_book", "Deanna Troi", [[TEL, null]])] };
    const look = chipLook(TEL, "+15550123456", who, false);
    expect(look.text).toBe("Deanna Troi");
    expect(look.classes).toContain("handle-unresolved");
    expect(look.classes).not.toContain("handle-linkable");
  });

  it("shows the contact's name when linked, faded when the handle stopped working", () => {
    const mine = contact("datalib_contacts", "Will Riker", [[TEL, "2019"]]);
    const look = chipLook(TEL, "+1 555 012 3456", { mine, accounts: [] }, true);
    expect(look.text).toBe("Will Riker");
    expect(look.initial).toBe("W");
    expect(look.classes).toContain("handle-stale");
  });
});

describe("hoverCard", () => {
  it("says what each source knows, and the person's other handles", () => {
    const mine = contact("datalib_contacts", "Will Riker", [
      [TEL, "2019"],
      ["email:riker@enterprise.org", null],
    ]);
    const accounts = [
      contact("tng_contacts", "William T. Riker", [[TEL, null]]),
      contact("whatsapp", "Will", [[TEL, null]], 1),
    ];
    const card = hoverCard(TEL, "+1 555 012 3456", { mine, accounts }, true);
    expect(card.name).toBe("Will Riker");
    expect(card.value).toBe("+15550123456");
    expect(card.icon).toBe("sms");
    expect(card.lines).toEqual([
      "Stopped working by 2019",
      "Shown here as “+1 555 012 3456”",
      "William T. Riker in tng_contacts",
      "Will in whatsapp · 1 item",
      "Also riker@enterprise.org",
      "Click to edit",
    ]);
  });

  it("offers to link only when there is a contacts app to link with", () => {
    const card = hoverCard("email:q@continuum.org", "Q <q@continuum.org>", NOBODY, true);
    expect(card.name).toBe("Q");
    expect(card.lines.at(-1)).toContain("Not linked");
    const without = hoverCard("email:q@continuum.org", "Q <q@continuum.org>", NOBODY, false);
    expect(without.lines.join(" ")).not.toContain("link");
  });
});

describe("copy", () => {
  it("carries the name and the identifier", () => {
    expect(copyText("email:riker@enterprise.org", "Will Riker")).toBe(
      "Will Riker <riker@enterprise.org>",
    );
    expect(copyText("tel:+15550123456", "Will Riker")).toBe("Will Riker (+15550123456)");
    expect(copyText("slack:T1/U2", "Data")).toBe("Data (slack:T1/U2)");
    expect(copyText("tel:+15550123456", "+15550123456")).toBe("+15550123456");
  });

  /** A copied chip once came out as "WWill Riker": the initial disc's
   *  letter and the name, with the address gone. */
  it("replaces each chip in a selection, disc and all, keeping data-handle", () => {
    const root = dom(
      `<p>From <span class="msg-author handle-chip handle-resolved" data-handle="email:riker@enterprise.org" data-label="Will Riker"><span class="handle-initial">W</span>Will Riker</span>, hello</p>`,
    );
    expect(rewriteChipsForCopy(root)).toBe(true);
    expect(root.textContent).toBe("From Will Riker <riker@enterprise.org>, hello");
    expect(root.querySelector("[data-handle]")?.getAttribute("data-handle")).toBe(
      "email:riker@enterprise.org",
    );
    expect(rewriteChipsForCopy(dom("<p>no chips</p>"))).toBe(false);
  });
});

describe("suggestedName", () => {
  it("takes the display name and leaves a bare identifier blank", () => {
    expect(suggestedName("Will Riker <riker@enterprise.org>", "email:riker@enterprise.org")).toBe(
      "Will Riker",
    );
    expect(
      suggestedName('"Riker, Will" <riker@enterprise.org>', "email:riker@enterprise.org"),
    ).toBe("Riker, Will");
    expect(suggestedName("riker@enterprise.org", "email:riker@enterprise.org")).toBe("");
    expect(suggestedName("Lt. Cmdr. Data", "slack:T1/U2")).toBe("Lt. Cmdr. Data");
  });
});

describe("todayPartialDate", () => {
  it("is a full local date", () => {
    expect(todayPartialDate(new Date(2026, 0, 5))).toBe("2026-01-05");
  });
});

describe("isAbsent", () => {
  /** With no contacts app configured the gateway answers 502 in JSON, and
   *  the quotes in its message arrive escaped; reading the raw text for
   *  them took every no-app page for an error and drew no chips. */
  it("reads the gateway's JSON, not its raw text", () => {
    const body = JSON.stringify({ error: 'no applet "datalib_contacts"' });
    expect(body).toContain('\\"');
    expect(isAbsent(502, body)).toBe(true);
    expect(
      isAbsent(502, JSON.stringify({ error: 'applet "datalib_contacts": it is not running' })),
    ).toBe(false);
    expect(isAbsent(500, body)).toBe(false);
    expect(isAbsent(502, "<html>bad gateway</html>")).toBe(false);
  });
});
