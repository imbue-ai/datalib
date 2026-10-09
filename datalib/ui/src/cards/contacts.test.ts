// The chip rules: which links count, what a chip and its tooltip say,
// what a copy carries, and the name a new contact is offered.

import { describe, expect, it } from "vitest";

import chipCss from "./chip.css?inline";

import {
  chipLook,
  chipMenu,
  composeUri,
  copyText,
  drawChip,
  chipTooltip,
  isAbsent,
  type NormalizedContact,
  rewriteChipsForCopy,
  suggestedName,
  todayPartialDate,
  chipAnchors,
  searchQueryFor,
  movesPeople,
} from "./contacts";

function dom(html: string): HTMLElement {
  const div = document.createElement("div");
  div.innerHTML = html;
  return div;
}

const chip = (handle: string, name: string) =>
  `<a class="chip" href="x" data-handle="${handle}">${name}</a>`;

describe("chipAnchors", () => {
  /** A chip may sit anywhere: the header, the To line, a mention in the
   *  body. What makes it one is the mark `chipLinks.js` put on a link
   *  whose href names a handle, and nothing else. */
  it("takes every marked chip link, wherever it is, and no other link", () => {
    const root = dom(
      `<div id="m-1" data-section-uuid="1" class="msg"><h2>${chip("email:riker@enterprise.org", "Will Riker")} <time class="msg-ts">x</time></h2>` +
        `<p><span class="msg-recipients">To ${chip("email:troi@enterprise.org", "Deanna")}</span></p>` +
        `<p>Ask ${chip("slack:T1/U2", "@data")}, or <a href="mailto:bare@enterprise.org">this</a>, ` +
        `or <span data-handle="email:forged@enterprise.org">me</span>.</p></div>`,
    );
    expect(chipAnchors(root).map((a) => a.dataset.handle)).toEqual([
      "email:riker@enterprise.org",
      "email:troi@enterprise.org",
      "slack:T1/U2",
    ]);
  });
});

function contact(
  source_id: string,
  name: string,
  handles: [string, string | null][],
  seen: number | null = null,
): NormalizedContact {
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
const NOBODY = { mine: null, sourceContacts: [] };

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

  /** A link a sender wrote can name anyone it likes; the chip says who
   *  the handle really is. This is what lets a chip sit anywhere in a
   *  body (docs/dev/chips.md § Trust). */
  it("shows the resolved name, never the link's text", () => {
    const who = {
      mine: null,
      sourceContacts: [contact("slack", "Q", [["email:q@continuum.org", null]])],
    };
    expect(chipLook("email:q@continuum.org", "Captain Picard", who, true).text).toBe("Q");
    const mine = contact("datalib_contacts", "Q", [["email:q@continuum.org", null]]);
    expect(
      chipLook("email:q@continuum.org", "Captain Picard", { mine, sourceContacts: [] }, true).text,
    ).toBe("Q");
    expect(
      chipTooltip("email:q@continuum.org", "Captain Picard", { mine, sourceContacts: [] }, true),
    ).toContain("Shown here as “Captain Picard”");
  });

  it("names an unlinked handle after the best source contact, linkable only with the app", () => {
    const who = {
      mine: null,
      sourceContacts: [contact("address_book", "Deanna Troi", [[TEL, null]])],
    };
    const look = chipLook(TEL, "+15550123456", who, false);
    expect(look.text).toBe("Deanna Troi");
    expect(look.classes).toContain("handle-unresolved");
    expect(look.classes).not.toContain("handle-linkable");
  });

  it("shows the contact's name when linked, faded when the handle stopped working", () => {
    const mine = contact("datalib_contacts", "Will Riker", [[TEL, "2019"]]);
    const look = chipLook(TEL, "+1 555 012 3456", { mine, sourceContacts: [] }, true);
    expect(look.text).toBe("Will Riker");
    expect(look.initial).toBe("W");
    expect(look.classes).toContain("handle-stale");
  });
});

describe("chipMenu", () => {
  /** The person's card leads, as the double-click does; an email address
   *  can be written to; the copies, the search and, with a contacts app,
   *  the link follow. */
  it("opens the card first, then compose, the copies, the search, and a link", () => {
    const ids = (entries: { id: string }[]) => entries.map((e) => e.id);
    const unlinked = chipMenu("email:riker@enterprise.org", "Will Riker", NOBODY, true);
    expect(ids(unlinked)).toEqual([
      "open",
      "compose",
      "copy-id",
      "copy-name",
      "copy-both",
      "search",
      "edit",
    ]);
    expect(unlinked.map((e) => e.label)).toEqual([
      "Open contact",
      "Compose mail to riker@enterprise.org",
      "Copy riker@enterprise.org",
      "Copy “Will Riker”",
      "Copy “Will Riker <riker@enterprise.org>”",
      "Everything from Will Riker",
      "Link to a contact…",
    ]);
    expect(unlinked.filter((e) => e.separator).map((e) => e.id)).toEqual([
      "compose",
      "search",
      "edit",
    ]);
    const mine = contact("datalib_contacts", "Will Riker", [[TEL, null]]);
    expect(chipMenu(TEL, "+1 555", { mine, sourceContacts: [] }, true).at(-1)?.label).toBe(
      "Edit contact link…",
    );
    const phone = chipMenu(TEL, "+15550123456", NOBODY, false);
    expect(ids(phone)).toEqual(["open", "copy-id", "search"]);
    expect(phone.filter((e) => e.separator).map((e) => e.id)).toEqual(["copy-id", "search"]);
  });

  it("composes mail only to an email address", () => {
    expect(composeUri("email:riker@enterprise.org")).toBe("mailto:riker@enterprise.org");
    expect(composeUri(TEL)).toBeNull();
    expect(composeUri("slack:T01/U02")).toBeNull();
  });

  /** By the handle, not the name shown: a name is any author holding it. */
  it("searches by the handle itself", () => {
    expect(searchQueryFor("email:riker@enterprise.org")).toBe("from:email:riker@enterprise.org");
    expect(searchQueryFor(TEL)).toBe(`from:${TEL}`);
  });
});

describe("a photo", () => {
  /// Your contact's photo leads; without one, the best a source gave; the
  /// identifier still copies, the picture does not.
  it("leads the chip when the resolver serves one", () => {
    const withPhoto = (c: NormalizedContact, url: string | null) => ({ ...c, photo_url: url });
    const mine = withPhoto(
      contact("datalib_contacts", "Will Riker", [[TEL, null]]),
      "/applet/datalib_contacts/photo/c1",
    );
    const slack = withPhoto(
      contact("slack", "Riker", [[TEL, null]]),
      "/applet/unified_index/asset/u/blobs/r.png",
    );
    expect(chipLook(TEL, "+1 555", { mine, sourceContacts: [slack] }, true).photo).toBe(
      "/applet/datalib_contacts/photo/c1",
    );
    expect(chipLook(TEL, "+1 555", { mine: null, sourceContacts: [slack] }, true).photo).toBe(
      "/applet/unified_index/asset/u/blobs/r.png",
    );
    expect(chipLook(TEL, "+1 555", NOBODY, true).photo).toBeNull();
    const el = dom(`<a class="chip" data-handle="${TEL}">+1 555</a>`)
      .firstElementChild as HTMLElement;
    drawChip(el, chipLook(TEL, "+1 555", { mine, sourceContacts: [] }, true));
    const lead = el.firstElementChild as HTMLImageElement;
    expect(lead.className).toBe("handle-photo");
    expect(lead.getAttribute("src")).toBe("/applet/datalib_contacts/photo/c1");
    expect(el.querySelector(".handle-initial")).toBeNull();
    expect(el.textContent).toBe("Will Riker");
    // A photo the browser cannot decode falls back to the initial.
    lead.dispatchEvent(new Event("error"));
    expect(el.querySelector(".handle-photo")).toBeNull();
    expect(el.querySelector(".handle-initial")?.textContent).toBe("W");
  });
});

describe("chipTooltip", () => {
  it("says who, the identifier, what each source knows, and the person's other handles", () => {
    const mine = contact("datalib_contacts", "Will Riker", [
      [TEL, "2019"],
      ["email:riker@enterprise.org", null],
    ]);
    const sourceContacts = [
      contact("tng_contacts", "William T. Riker", [[TEL, null]]),
      contact("whatsapp", "Will", [[TEL, null]], 1),
    ];
    expect(chipTooltip(TEL, "+1 555 012 3456", { mine, sourceContacts }, true).split("\n")).toEqual(
      [
        "Will Riker",
        "+15550123456",
        "Stopped working by 2019",
        "Shown here as “+1 555 012 3456”",
        "William T. Riker in tng_contacts",
        "Will in whatsapp · 1 item",
        "Also riker@enterprise.org",
        "Click to edit",
      ],
    );
  });

  it("offers to link only when there is a contacts app to link with", () => {
    const linkable = chipTooltip("email:q@continuum.org", "Q <q@continuum.org>", NOBODY, true);
    expect(linkable.split("\n")[0]).toBe("Q");
    expect(linkable.split("\n").at(-1)).toContain("Not linked");
    const without = chipTooltip("email:q@continuum.org", "Q <q@continuum.org>", NOBODY, false);
    expect(without).not.toContain("link");
  });

  /// The hover card that used to say this was a large panel with a
  /// photo, popping over the text on every pass of the pointer; a
  /// person asked for a tooltip instead.
  it("is the drawn chip's title", () => {
    const sourceContacts = [contact("slack", "Worf", [[TEL, null]], 6894)];
    const el = dom(chip(TEL, "Worf")).querySelector<HTMLElement>("a")!;
    drawChip(el, chipLook(TEL, "Worf", { mine: null, sourceContacts }, false));
    expect(el.title).toBe("Worf\n+15550123456\nWorf in slack · 6894 items");
  });
});

describe("copy", () => {
  it("carries the name and the identifier", () => {
    expect(copyText("email:riker@enterprise.org", "Will Riker")).toBe(
      "Will Riker <riker@enterprise.org>",
    );
    expect(copyText("tel:+15550123456", "Will Riker")).toBe("Will Riker (+15550123456)");
    expect(copyText("slack:T1/U2", "Data")).toBe("Data (slack:T1/U2)");
    expect(copyText("signal_aci:0195683a-d140-87f9-bdf6-234da6d6880f", "Q")).toBe(
      "Q (signal_aci:0195683a-d140-87f9-bdf6-234da6d6880f)",
    );
    expect(copyText("tel:+15550123456", "+15550123456")).toBe("+15550123456");
  });

  /** A copied chip once came out as "WWill Riker": the initial disc's
   *  letter and the name, with the address gone. */
  it("replaces each chip in a selection, disc and all, with the link it was written as", () => {
    const root = dom(
      `<p>From <a class="chip handle-chip handle-resolved" href="mailto:riker@enterprise.org" data-handle="email:riker@enterprise.org" data-label="Will Riker"><span class="handle-initial">W</span>Will Riker</a>, hello</p>`,
    );
    expect(rewriteChipsForCopy(root)).toBe(true);
    expect(root.textContent).toBe("From Will Riker <riker@enterprise.org>, hello");
    const a = root.querySelector("a");
    expect(a?.getAttribute("href")).toBe("mailto:riker@enterprise.org");
    expect(a?.getAttribute("title")).toBe("Will Riker <riker@enterprise.org>");
    expect(a?.getAttribute("data-handle")).toBe("email:riker@enterprise.org");
    expect(rewriteChipsForCopy(dom("<p>no chips</p>"))).toBe(false);
  });

  it("copies a group or step chip as its name and its URI", () => {
    const root = dom(
      `<p>In <a class="chip handle-chip entity-chip" href="datalib:group/slack" data-entity="datalib:group/slack" data-label="Work Slack"><span class="handle-mark"></span>Work Slack</a>.</p>`,
    );
    expect(rewriteChipsForCopy(root)).toBe(true);
    expect(root.textContent).toBe("In Work Slack (datalib:group/slack).");
    expect(root.querySelector("a")?.getAttribute("href")).toBe("datalib:group/slack");
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

describe("chip.css", () => {
  /// A message header is a wrapping flex row, so a chip in it shrank to
  /// its longest word and "Jean-Luc Picard" broke over two lines.
  it("keeps a chip's name on one line", () => {
    const style = document.createElement("style");
    style.textContent = chipCss;
    document.head.append(style);
    const el = dom(chip(TEL, "Jean-Luc Picard")).querySelector<HTMLElement>("a")!;
    drawChip(el, chipLook(TEL, "Jean-Luc Picard", NOBODY, false));
    document.body.append(el);
    expect(getComputedStyle(el).whiteSpace).toBe("nowrap");
    style.remove();
    el.remove();
  });
});

describe("movesPeople", () => {
  it("is a published edit to a curated store, and nothing else", () => {
    expect(movesPeople({ kind: "table_changed", table: "curated" })).toBe(true);
    expect(movesPeople({ kind: "table_changed", table: "manage.rows" })).toBe(false);
    expect(movesPeople({ kind: "index_changed" })).toBe(false);
  });
});
