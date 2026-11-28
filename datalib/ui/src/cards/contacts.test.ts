// The chip rules: which links count, what a chip and its tooltip say,
// what a copy carries, and the name a new contact is offered.

import { afterEach, describe, expect, it, vi } from "vitest";

import chipCss from "./chip.css?inline";

import {
  chipLook,
  identifierLook,
  chipMenu,
  composeUri,
  copyText,
  drawChip,
  chipTooltip,
  isAbsent,
  type NormalizedContact,
  type PeopleAnswer,
  addedSummary,
  rewriteChipsForCopy,
  suggestedName,
  todayPartialDate,
  chipAnchors,
  searchQueryFor,
  movesPeople,
  draftOf,
  saveDraft,
  setContactPhoto,
  people,
  linkHandle,
  unlinkHandle,
  whoFrom,
  peopleTitle,
  contactLook,
  decorateHandles,
  chipCell,
} from "./contacts";
import { chipMenuOf, chipName, chipOf, everythingWith } from "./chipActions";
import type { BodyPiece } from "@/api";
import { renderDocument } from "./renderDocument";

function dom(html: string): HTMLElement {
  const div = document.createElement("div");
  div.innerHTML = html;
  return div;
}

const chip = (handle: string, name: string) =>
  `<a class="chip" href="x" data-handle="${handle}">${name}</a>`;

/** What `POST /people` answers, with nothing but these handles' records. */
function indexAnswer(people: Record<string, NormalizedContact[]>): PeopleAnswer {
  return { people, same_digits: {}, pages: {}, full_numbers: {} };
}

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
const NOBODY = { mine: [], sourceContacts: [] };

describe("chipLook", () => {
  it("shows the source's name without the address, and the kind's mark, when nobody knows them", () => {
    const look = chipLook(
      "email:riker@enterprise.org",
      "Will Riker <riker@enterprise.org>",
      NOBODY,
    );
    expect(look.text).toBe("Will Riker");
    expect(look.icon).toBe("email");
    expect(look.classes).toContain("handle-unresolved");
    expect(chipLook(TEL, "+15550123456", NOBODY).text).toBe("+15550123456");
  });

  /** A link a sender wrote can name anyone it likes; the chip says who
   *  the handle really is. This is what lets a chip sit anywhere in a
   *  body (docs/dev/chips.md § Trust). */
  it("shows the resolved name, never the link's text", () => {
    const who = {
      mine: [],
      sourceContacts: [contact("slack", "Q", [["email:q@continuum.org", null]])],
    };
    expect(chipLook("email:q@continuum.org", "Captain Picard", who).text).toBe("Q");
    const mine = contact("datalib_contacts", "Q", [["email:q@continuum.org", null]]);
    expect(
      chipLook("email:q@continuum.org", "Captain Picard", { mine: [mine], sourceContacts: [] })
        .text,
    ).toBe("Q");
    expect(
      chipTooltip("email:q@continuum.org", "Captain Picard", { mine: [mine], sourceContacts: [] }),
    ).toContain("Shown here as “Captain Picard”");
  });

  it("names a handle on no contact after the best source contact", () => {
    const who = {
      mine: [],
      sourceContacts: [contact("address_book", "Deanna Troi", [[TEL, null]])],
    };
    const look = chipLook(TEL, "+15550123456", who);
    expect(look.text).toBe("Deanna Troi");
    expect(look.classes).toContain("handle-unresolved");
  });

  it("shows the contact's name when linked, faded when the handle stopped working", () => {
    const mine = contact("datalib_contacts", "Will Riker", [[TEL, "2019"]]);
    const look = chipLook(TEL, "+1 555 012 3456", { mine: [mine], sourceContacts: [] });
    expect(look.text).toBe("Will Riker");
    expect(look.initial).toBe("W");
    expect(look.classes).toContain("handle-stale");
  });
});

describe("chipMenu", () => {
  /** The person's card leads, as the double-click does; an email address
   *  can be written to; the copies, the search and, with a contacts app,
   *  adding someone not in it follow. */
  it("opens the card first, then compose, the copies, the search, and adding", () => {
    const ids = (entries: { id: string }[]) => entries.map((e) => e.id);
    const unlinked = chipMenu("email:riker@enterprise.org", "Will Riker", NOBODY, true);
    expect(ids(unlinked)).toEqual([
      "open",
      "compose",
      "copy-id",
      "copy-name",
      "copy-both",
      "search",
      "add",
    ]);
    expect(unlinked.map((e) => e.label)).toEqual([
      "Open",
      "Compose mail to riker@enterprise.org",
      "Copy riker@enterprise.org",
      "Copy “Will Riker”",
      "Copy “Will Riker <riker@enterprise.org>”",
      "Everything with Will Riker",
      "Add to contacts…",
    ]);
    expect(unlinked.filter((e) => e.separator).map((e) => e.id)).toEqual([
      "compose",
      "search",
      "add",
    ]);
    const mine = contact("datalib_contacts", "Will Riker", [[TEL, null]]);
    const inContacts = chipMenu(TEL, "+1 555", { mine: [mine], sourceContacts: [] }, true);
    expect(inContacts[0].label).toBe("Open contact");
    expect(ids(inContacts)).not.toContain("add");
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
    expect(searchQueryFor("email:riker@enterprise.org")).toBe("with:email:riker@enterprise.org");
    expect(searchQueryFor(TEL)).toBe(`with:${TEL}`);
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
      `/data/beeper/ingest/blob/${"ab".repeat(32)}`,
    );
    expect(chipLook(TEL, "+1 555", { mine: [mine], sourceContacts: [slack] }).photo).toBe(
      "/applet/datalib_contacts/photo/c1",
    );
    expect(chipLook(TEL, "+1 555", { mine: [], sourceContacts: [slack] }).photo).toBe(
      `/data/beeper/ingest/blob/${"ab".repeat(32)}`,
    );
    expect(chipLook(TEL, "+1 555", NOBODY).photo).toBeNull();
    const el = dom(`<a class="chip" data-handle="${TEL}">+1 555</a>`)
      .firstElementChild as HTMLElement;
    drawChip(el, chipLook(TEL, "+1 555", { mine: [mine], sourceContacts: [] }));
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
    expect(
      chipTooltip(TEL, "+1 555 012 3456", { mine: [mine], sourceContacts }).split("\n"),
    ).toEqual([
      "Will Riker",
      "+15550123456",
      "Stopped working by 2019",
      "Shown here as “+1 555 012 3456”",
      "William T. Riker in tng_contacts",
      "Will in whatsapp · 1 item",
      "Also riker@enterprise.org",
      "Double-click to open",
    ]);
  });

  /// The hover card that used to say this was a large panel with a
  /// photo, popping over the text on every pass of the pointer; a
  /// person asked for a tooltip instead.
  it("is the drawn chip's title", () => {
    const sourceContacts = [contact("slack", "Worf", [[TEL, null]], 6894)];
    const el = dom(chip(TEL, "Worf")).querySelector<HTMLElement>("a")!;
    drawChip(el, chipLook(TEL, "Worf", { mine: [], sourceContacts }));
    expect(el.title).toBe("Worf\n+15550123456\nWorf in slack · 6894 items\nDouble-click to open");
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
    expect(copyText("email:riker@enterprise.org", "Will Riker <Riker@Enterprise.org>")).toBe(
      "Will Riker <Riker@Enterprise.org>",
    );
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
    drawChip(el, chipLook(TEL, "Jean-Luc Picard", NOBODY));
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

/** Your contact marked *This is me* carries a small "me" on every chip of
 *  it, in place of the list's old Me column; the mark is no part of the
 *  name, so a copy and the label's name leave it out, and a list of the
 *  person's own handles leaves it off. */
describe("the me mark", () => {
  it("marks every chip of the contact that is you, and nothing else", () => {
    const me = { ...contact("datalib_contacts", "Jean-Luc Picard", [[TEL, null]]), account: true };
    const who = { mine: [me], sourceContacts: [] };
    const look = chipLook(TEL, "Picard", who);
    expect(look.me).toBe(true);
    expect(look.title).toContain("This is you");
    expect(contactLook("", me).me).toBe(true);
    expect(contactLook("", { ...me, account: false }).me).toBe(false);
    expect(identifierLook(TEL, "", who).me).toBe(false);
    expect(chipLook(TEL, "Picard", NOBODY).me).toBeFalsy();

    const el = dom(`<a class="chip" data-handle="${TEL}">Picard</a>`)
      .firstElementChild as HTMLElement;
    drawChip(el, look);
    expect(el.querySelector(".chip-me")?.textContent).toBe("me");
    expect(el.querySelector(".chip-me")?.getAttribute("aria-hidden")).toBe("true");
    expect(el.dataset.label).toBe("Jean-Luc Picard");
    drawChip(
      el,
      chipLook(TEL, "Picard", { mine: [{ ...me, account: false }], sourceContacts: [] }),
    );
    expect(el.querySelector(".chip-me")).toBeNull();
  });
});

/** A list of one person's handles (the person card, a contact page's
 *  fields) shows each handle, not the name again, in the same colours. */
describe("identifierLook", () => {
  it("shows the identifier, with the kind's mark, blue only when on a contact", () => {
    const mine = contact("datalib_contacts", "Will Riker", [["email:riker@enterprise.org", null]]);
    const linked = identifierLook("email:riker@enterprise.org", "Riker@Enterprise.org", {
      mine: [mine],
      sourceContacts: [],
    });
    expect(linked.text).toBe("Riker@Enterprise.org");
    expect(linked.classes).toContain("handle-resolved");
    expect([linked.initial, linked.photo, linked.icon]).toEqual([null, null, "email"]);
    const loose = identifierLook(TEL, "", NOBODY);
    expect(loose.text).toBe("+15550123456");
    expect(loose.classes).toContain("handle-unresolved");
  });

  it("names an id a service minted as that service does, keeping the id in the tooltip", () => {
    const SLACK = "slack:T1701D/U0WORF";
    const mine = contact("datalib_contacts", "Worf", [[SLACK, null]]);
    const slack = contact("slack", "worf.of.mogh", [[SLACK, null]]);
    const look = identifierLook(SLACK, "T1701D/U0WORF", { mine: [mine], sourceContacts: [slack] });
    expect([look.text, look.icon]).toEqual(["worf.of.mogh", "slack"]);
    expect(look.title).toContain("T1701D/U0WORF");
    expect(identifierLook("facebook:name/Worf", "name/Worf", NOBODY).text).toBe("Worf");
    expect(identifierLook("discord:170100000000000005", "", NOBODY).text).toBe(
      "170100000000000005",
    );
  });
});

describe("the card's edit calls", () => {
  type Sent = { url: string; method: string; type: string | null; body: unknown };
  function answer(status: number, body: unknown): Sent[] {
    const sent: Sent[] = [];
    vi.stubGlobal("fetch", async (url: string, init?: RequestInit) => {
      const headers = (init?.headers ?? {}) as Record<string, string>;
      sent.push({
        url,
        method: init?.method ?? "GET",
        type: headers["content-type"] ?? null,
        body: init?.body,
      });
      return new Response(JSON.stringify(body), { status });
    });
    return sent;
  }
  afterEach(() => vi.unstubAllGlobals());

  it("reads a contact with no draft as none, not as a failure", async () => {
    answer(404, { error: "c1 has no draft" });
    expect(await draftOf("c1")).toBeNull();
  });

  it("names the published commit the card showed when it saves, and what is whose", async () => {
    const sent = answer(200, { outcome: "saved", commit: "abc" });
    const held = { "tel:+15550123456": "share" as const };
    expect(await saveDraft("c 1", "seen-hash", held)).toEqual({ outcome: "saved", commit: "abc" });
    expect(sent).toEqual([
      {
        url: "/applet/datalib_contacts/contact/c%201/draft/save",
        method: "POST",
        type: "application/json",
        body: JSON.stringify({ seen: "seen-hash", held }),
      },
    ]);
  });

  it("links a handle with the label and the card it came from", async () => {
    const sent = answer(200, { ok: true });
    const from = { label: "work", copied_from: { source_id: "contacts", key: "riker.vcf" } };
    await linkHandle("email:riker@enterprise.test", "c1", undefined, from);
    expect(JSON.parse(sent[0]!.body as string)).toEqual({
      handle: "email:riker@enterprise.test",
      contact_id: "c1",
      ...from,
    });
  });

  it("sends a photo as its own bytes and type, not as JSON", async () => {
    const sent = answer(200, { photo_url: "/applet/datalib_contacts/photo/c1?v=1" });
    const photo = new Blob(["png"], { type: "image/png" });
    await setContactPhoto("c1", photo);
    expect(sent[0]).toMatchObject({ url: "/applet/datalib_contacts/photo/c1", method: "PUT" });
    expect(sent[0]!.type).toBe("image/png");
    expect(sent[0]!.body).toBe(photo);
  });
});

describe("an edit to a link", () => {
  /// What the live updates need to exist; nothing here reads them.
  class QuietEventSource {
    static CLOSED = 2;
    readyState = 0;
    addEventListener() {}
    close() {}
  }
  afterEach(() => vi.unstubAllGlobals());

  /** Linking riker@ dropped only riker@ from the cache, so a card drawn
   *  from wriker@ kept the contact's old list of handles until a live
   *  update came, seconds later. */
  it("forgets every handle of the contact it joins or leaves", async () => {
    const riker = contact("datalib_contacts", "Will Riker", [
      ["email:wriker@enterprise.test", null],
      ["email:will@alaska.test", null],
    ]);
    vi.stubGlobal("EventSource", QuietEventSource);
    vi.stubGlobal("fetch", async (url: string) => {
      const body = url.endsWith("/resolve")
        ? {
            resolved: {
              "email:wriker@enterprise.test": [riker],
              "email:will@alaska.test": [riker],
            },
          }
        : url.endsWith("/people")
          ? indexAnswer({})
          : {};
      return new Response(JSON.stringify(body), { status: 200 });
    });
    const shown = ["email:wriker@enterprise.test", "email:will@alaska.test", "email:worf@k.test"];
    await people.ask(shown);
    expect(people.get("email:wriker@enterprise.test")?.mine[0]?.key).toBe(riker.key);

    await linkHandle("email:riker@enterprise.test", riker.key);
    expect(people.get("email:wriker@enterprise.test")).toBeUndefined();
    expect(people.get("email:worf@k.test")).toBeDefined();

    await people.ask(shown);
    await unlinkHandle("email:will@alaska.test", riker.key);
    expect(people.get("email:wriker@enterprise.test")).toBeUndefined();
    expect(people.get("email:worf@k.test")).toBeDefined();
  });
});

describe("a conversation titled by its people", () => {
  /** A chat named after its people was titled with the text the source
   *  showed, so a WhatsApp stranger read as a raw id and a group as a
   *  comma list nobody could click. */
  it("is one person's chip, or for several their photos together and each name a chip", () => {
    const one = peopleTitle([
      { handle: "whatsapp:100000000000108", shownAs: "WhatsApp user …0108" },
    ]);
    expect(one.matches("a.chip[data-handle='whatsapp:100000000000108']")).toBe(true);
    expect(one.textContent).toBe("WhatsApp user …0108");

    const crew = peopleTitle([
      { handle: "tel:+17015550101", shownAs: "William Riker" },
      { handle: "tel:+17015550102", shownAs: "Data" },
      { handle: "tel:+17015550103", shownAs: "Geordi La Forge" },
    ]);
    expect(crew.classList.contains("chip-group")).toBe(true);
    expect(crew.querySelectorAll(".chip-group-faces > *")).toHaveLength(3);
    const names = [...crew.querySelectorAll<HTMLElement>("a.chip[data-handle]")];
    expect(names.map((a) => a.dataset.handle)).toEqual([
      "tel:+17015550101",
      "tel:+17015550102",
      "tel:+17015550103",
    ]);
    expect(names.every((a) => !a.querySelector(".handle-photo, .handle-initial"))).toBe(true);
    expect(crew.textContent).toContain("William Riker, Data, Geordi La Forge");
  });
});

describe("a contact's chip", () => {
  /** A contact you made has no photo of its own yet, so its chip showed
   *  an initial where every one of its handles' chips showed a face. */
  it("takes a photo from its handles' records until it has its own", () => {
    const picard = contact("datalib_contacts", "Jean-Luc Picard", [
      ["email:jlpicard@enterprise.starfleet", null],
      ["email:picard@enterprise.starfleet", null],
    ]);
    const lookup = vi.spyOn(people, "lookup").mockImplementation((handle) =>
      handle === "email:picard@enterprise.starfleet"
        ? {
            mine: [],
            sourceContacts: [{ ...picard, source_id: "tng_contacts", photo_url: "/p/picard.jpg" }],
          }
        : undefined,
    );
    expect(contactLook("", picard).photo).toBe("/p/picard.jpg");
    expect(contactLook("", { ...picard, photo_url: "/p/own.jpg" }).photo).toBe("/p/own.jpg");
    lookup.mockRestore();
  });
});

describe("a name that is someone else's handle", () => {
  /** A stranger whose mail says `"picard@enterprise.org" <q@continuum.test>`
   *  was drawn as the account owner on every message they sent. */
  it("is never drawn: the chip shows its own identifier instead", () => {
    const q = "email:q@continuum.test";
    expect(chipLook(q, "picard@enterprise.org", NOBODY).text).toBe("q@continuum.test");
    expect(chipLook(q, "mailto:picard@enterprise.org", NOBODY).text).toBe("q@continuum.test");
    expect(chipLook(q, "+1 202 555 0101", NOBODY).text).toBe("q@continuum.test");
    const told = { mine: [], sourceContacts: [contact("mail", "picard@enterprise.org", [])] };
    expect(chipLook(q, "Q", told).text).toBe("q@continuum.test");
    expect(chipTooltip(q, "Q", told).split("\n")[0]).toBe("q@continuum.test");
    expect(chipLook(q, "picard@enterprise.org", told, true).text).toBe("q@continuum.test");
  });

  it("is a name when it is the handle itself, one the person holds, or no handle at all", () => {
    const q = "email:q@continuum.test";
    expect(chipLook(q, "Q@Continuum.test", NOBODY).text).toBe("Q@Continuum.test");
    const book = {
      mine: [],
      sourceContacts: [
        contact("book", "q@q.test", [
          [q, null],
          ["email:q@q.test", null],
        ]),
      ],
    };
    expect(chipLook(q, "", book).text).toBe("q@q.test");
    expect(chipLook("slack:T1/U2", "@data", NOBODY).text).toBe("@data");
  });
});

describe("a message's own header", () => {
  afterEach(() => vi.unstubAllGlobals());

  /** A message, as the document route sends one of chat-common's: in
   *  its box, the header line `MessageHeader::render` writes, then the
   *  body. */
  function message(uuid: string, author: string, handle: string, body: string): BodyPiece {
    const addr = handle.slice("email:".length);
    const md =
      `## [${author}](mailto:${addr} "${author} <${addr}>") ` +
      `<time class="msg-ts" datetime="2369-04-15T10:05:00+02:00">Tue Apr 15th, 2369 at 10:05</time>\n\n` +
      `${body}\n\n`;
    return { uuid, class: "msg msg--email", md };
  }

  /** One forged message showed its name on every message from the same
   *  address. Its header shows what it said; the sender's chip anywhere
   *  else, and a heading the body wrote, show what the sources know. */
  it("shows the name that message gave, and only there", async () => {
    const riker = "email:riker@enterprise.test";
    vi.stubGlobal("fetch", async (url: string) => {
      const body = url.endsWith("/people")
        ? indexAnswer({ [riker]: [contact("mail", "William Riker", [[riker, null]], 10)] })
        : {};
      return new Response(JSON.stringify(body), { status: 200 });
    });
    const pieces = [
      message("u1", "William Riker", riker, "Status report."),
      message(
        "u2",
        "PAY INVOICE NOW - Riker",
        riker,
        `Ask [Number One](mailto:riker@enterprise.test).\n\n## [The Captain](mailto:riker@enterprise.test) says pay`,
      ),
    ];
    const { html } = renderDocument(pieces);
    const root = document.body.appendChild(dom(html));
    await decorateHandles(root);
    root.remove();
    const label = (sel: string) =>
      [...root.querySelectorAll<HTMLElement>(sel)].map((a) => a.dataset.label);
    expect(label("#m-u1 > h2 > a.chip")).toEqual(["William Riker"]);
    expect(label("#m-u2 > h2 > a.chip")).toEqual(["PAY INVOICE NOW - Riker", "William Riker"]);
    expect(label("#m-u2 > p a.chip")).toEqual(["William Riker"]);
  });

  /** A body cannot claim to be a header by writing the attribute. */
  it("is not a body's link that names itself one", () => {
    const piece = message(
      "u3",
      "Q",
      "email:q@continuum.test",
      `<a class="chip" data-chip-header data-handle="email:q@continuum.test" href="mailto:q@continuum.test">Picard</a>`,
    );
    const { html } = renderDocument([piece]);
    const marked = [...dom(html).querySelectorAll<HTMLElement>("[data-chip-header]")];
    expect(marked.map((a) => a.textContent)).toEqual(["Q"]);
  });
});

describe("your contact's name", () => {
  afterEach(() => vi.unstubAllGlobals());

  /** A forged name, in a message header or as the sources' best, never
   *  shows over the name a person gave their contact: not in the header,
   *  the grid's Author cell, the chip's menu or its tooltip. */
  it("wins over every source's name on every surface that draws the chip", async () => {
    const riker = "email:riker@enterprise.example";
    const mine = contact("datalib_contacts", "Will Riker", [[riker, null]]);
    vi.stubGlobal("fetch", async (url: string) => {
      const body = url.endsWith("/resolve")
        ? { resolved: { [riker]: [mine] } }
        : url.endsWith("/people")
          ? indexAnswer({
              [riker]: [contact("mail", "PAY INVOICE NOW - Riker", [[riker, null]], 9)],
            })
          : {};
      return new Response(JSON.stringify(body), { status: 200 });
    });
    const forged = "PAY INVOICE NOW - Riker";
    const md =
      `## [${forged}](mailto:riker@enterprise.example "${forged} <riker@enterprise.example>") ` +
      `<time class="msg-ts" datetime="2369-04-15T10:05:00+02:00">x</time>\n\nPay.\n\n`;
    const { html } = renderDocument([{ uuid: "y1", class: "msg msg--email", md }]);
    const root = document.body.appendChild(dom(html));
    await decorateHandles(root);
    root.remove();
    const header = root.querySelector<HTMLElement>("#m-y1 > h2 > a.chip")!;
    expect(header.dataset.chipHeader).toBeDefined();
    expect(header.dataset.label).toBe("Will Riker");
    expect(header.title.split("\n")[0]).toBe("Will Riker");

    const cell = chipCell(riker, forged, people.get(riker));
    expect(cell.dataset.label).toBe("Will Riker");

    for (const el of [header, cell]) {
      const chip = chipOf(el)!;
      expect(chipName(chip)).toBe("Will Riker");
      const labels = chipMenuOf(chip).map((e) => e.label);
      expect(labels).toContain("Copy “Will Riker”");
      expect(labels.join("\n")).not.toContain("PAY INVOICE");
    }
  });
});

/** An answer about one handle that held another's numbers would change with
 *  whatever chips happened to be drawn beside it. */
describe("whoFrom", () => {
  it("takes only what the answer says of its handle", () => {
    const gomez = { ...contact("book", "Sonya Gomez", []), handles: [] };
    const written = (value: string): NormalizedContact => ({
      ...gomez,
      handles: [{ medium: "phone", label: null, value, handle: null, stopped_working_by: null }],
    });
    const answer: PeopleAnswer = {
      people: { "email:a@b.org": [written("555-0147")] },
      same_digits: { "tel:+12025550199": [written("555-0199")] },
      pages: {},
      full_numbers: { "555-0147": ["tel:+12025550147"], "555-0199": ["tel:+12025550199"] },
    };
    expect(whoFrom(answer, "email:a@b.org", [])).toEqual({
      mine: [],
      sourceContacts: [written("555-0147")],
      fullNumbers: { "555-0147": ["tel:+12025550147"] },
    });
    expect(whoFrom(answer, "tel:+12025550199", [])).toEqual({
      mine: [],
      sourceContacts: [],
      sameDigits: [written("555-0199")],
      fullNumbers: { "555-0199": ["tel:+12025550199"] },
    });
    expect(whoFrom(answer, "email:nobody@b.org", [])).toEqual({ mine: [], sourceContacts: [] });
  });
});

describe("addedSummary", () => {
  const none = { created: 0, added_to: 0, already: 0, skipped: [] };
  it("says what came of everyone who was selected", () => {
    expect(addedSummary({ ...none, created: 1 })).toBe("Added 1 person to contacts");
    expect(
      addedSummary({
        created: 12,
        added_to: 2,
        already: 3,
        skipped: [{ what: "The Captains", why: "their handles are on Riker and Picard" }],
      }),
    ).toBe(
      "Added 12 people to contacts; added handles to 2 contacts; 3 already there; " +
        "The Captains skipped: their handles are on Riker and Picard",
    );
    expect(addedSummary({ ...none, added_to: 1 })).toBe("Added handles to 1 contact");
    expect(addedSummary({ ...none, already: 4 })).toBe(
      "Added 0 people to contacts; 4 already there",
    );
  });
});

/** An address two of your contacts share is one chip, named for both,
 *  led by its kind's mark: no one face is right. */
describe("a shared address", () => {
  const VINEYARD = "email:vineyard@labarre.earth";
  const marie = contact("datalib_contacts", "Marie Picard", [[VINEYARD, null]]);
  const robert = contact("datalib_contacts", "Robert Picard", [[VINEYARD, "2370"]]);
  const rene = contact("datalib_contacts", "René Picard", [[VINEYARD, null]]);
  const book = contact("tng_contacts", "Robert Picard", [[VINEYARD, null]]);

  it("draws one chip with both names, no photo, and the mark for its kind", () => {
    const look = chipLook(VINEYARD, "Marie", { mine: [marie, robert], sourceContacts: [book] });
    expect(look.text).toBe("Marie Picard & Robert Picard");
    expect(look.classes).toContain("handle-resolved");
    expect(look.classes).not.toContain("handle-stale");
    expect([look.photo, look.initial, look.icon]).toEqual([null, null, "email"]);
    expect(chipLook(VINEYARD, "", { mine: [marie, robert, rene], sourceContacts: [] }).text).toBe(
      "Marie Picard & 2 others",
    );
  });

  it("lists everyone who has it on hover, and opens the address rather than one of them", () => {
    const who = { mine: [marie, robert], sourceContacts: [] };
    expect(chipTooltip(VINEYARD, "", who).split("\n").slice(0, 3)).toEqual([
      "Marie Picard & Robert Picard",
      "vineyard@labarre.earth",
      "Shared by Marie Picard, Robert Picard",
    ]);
    const menu = chipMenu(VINEYARD, "", who, true);
    expect(menu[0].label).toBe("Open");
    expect(menu.map((e) => e.id)).not.toContain("add");
    expect(everythingWith(VINEYARD, who.mine)).toBe(searchQueryFor(VINEYARD));
    expect(everythingWith(VINEYARD, [marie])).toContain(`contact:${marie.key}`);
  });
});
