// The person card's rules: what it is about, which sections it shows,
// and in what order.

import { describe, expect, it } from "vitest";

import type { NormalizedContact, Who } from "./contacts";
import type { PageAnswer } from "./contactPages";
import {
  linkableHandles,
  linkState,
  offerNote,
  offers,
  parseSubject,
  personHandles,
  personModel,
  recordHandles,
  sectionLines,
  shownHandles,
  type Answers,
  type Subject,
} from "./person";

function contact(
  source_id: string,
  key: string,
  names: string[],
  handles: string[] = [],
): NormalizedContact {
  return {
    source_id,
    key,
    kind: "person",
    names,
    handles: handles.map((h) => ({
      medium: h.startsWith("tel:") ? "phone" : h.startsWith("email:") ? "email" : "other",
      label: null,
      value: h.slice(h.indexOf(":") + 1),
      handle: h,
      stopped_working_by: null,
    })),
    org: null,
    title: null,
    seen: null,
  };
}

const TEL = "tel:+15550100";
const EMAIL = "email:riker@enterprise.org";
const whatsapp = contact("whatsapp", "jid-1", ["Riker"], [TEL]);
const sms = contact("sms_backup", "15550100", ["Number One"], [TEL]);
const slack = contact("slack", "T1/U2", ["Will Riker"], ["slack:T1/U2", EMAIL]);
const mine = contact("datalib_contacts", "c-1", ["William Riker"], [TEL, EMAIL]);

const answers = (
  byHandle: Record<string, Who>,
  pages: Record<string, PageAnswer> = {},
  contacts: Record<string, NormalizedContact> = {},
): Answers => ({
  who: (h) => byHandle[h],
  page: (p) => pages[p],
  contact: (id) => contacts[id],
});
const by = (handle: string): Subject => ({ kind: "handle", handle });

describe("personModel", () => {
  it("an unlinked handle is the card's subject, with each source's record of it", () => {
    const m = personModel(
      by(TEL),
      null,
      answers({ [TEL]: { mine: null, sourceContacts: [whatsapp, sms] } }),
    );
    expect(m.title).toBe("Riker");
    expect(m.mine).toBeNull();
    expect(m.handles).toEqual([TEL]);
    expect(m.sections.map((s) => s.sourceId)).toEqual(["whatsapp", "sms_backup"]);
  });

  /** Opened from a chip in the SMS backup, the card lands on what that
   *  source says, even where `/people` ranked another first. */
  it("puts the source the chip was seen in first, and marks it", () => {
    const m = personModel(
      by(TEL),
      "sms_backup",
      answers({ [TEL]: { mine: null, sourceContacts: [whatsapp, sms] } }),
    );
    expect(m.sections.map((s) => [s.sourceId, s.seenHere])).toEqual([
      ["sms_backup", true],
      ["whatsapp", false],
    ]);
    expect(m.title, "named as the source it was seen in names it").toBe("Number One");
  });

  it("a linked handle opens its contact: every handle of it, and their records once each", () => {
    const m = personModel(
      by(EMAIL),
      null,
      answers({
        [EMAIL]: { mine, sourceContacts: [slack] },
        [TEL]: { mine, sourceContacts: [whatsapp, sms] },
      }),
    );
    expect(m.title).toBe("William Riker");
    expect(m.mine).toBe(mine);
    expect(m.handles).toEqual([EMAIL, TEL]);
    expect(m.sections.map((s) => s.sourceId)).toEqual(["slack", "whatsapp", "sms_backup"]);
  });

  it("a record that holds two of the person's handles shows once", () => {
    const m = personModel(
      by(EMAIL),
      null,
      answers({
        [EMAIL]: {
          mine: { ...mine, handles: [...mine.handles, ...slack.handles] },
          sourceContacts: [slack],
        },
        "slack:T1/U2": { mine, sourceContacts: [slack] },
      }),
    );
    expect(m.sections.map((s) => s.sourceId)).toEqual(["slack"]);
  });

  it("knows nothing yet before the answer lands", () => {
    const m = personModel(by(TEL), "whatsapp", answers({}));
    expect(m).toEqual({
      title: "+15550100",
      mine: null,
      handles: [TEL],
      sections: [],
      fullNumbers: {},
    });
  });
});

describe("personHandles", () => {
  it("leads with the opened handle and skips a link the rules no longer read", () => {
    const old = { ...mine, handles: [...mine.handles, { ...mine.handles[0], handle: null }] };
    expect(personHandles(old, TEL)).toEqual([TEL, EMAIL]);
    expect(personHandles(old, null)).toEqual([TEL, EMAIL]);
    expect(personHandles(null, TEL)).toEqual([TEL]);
  });
});

describe("sectionLines", () => {
  it("says the other names, the role, the details, the note and how much was seen", () => {
    const c: NormalizedContact = {
      ...contact("whatsapp", "jid-1", ["Riker", "Will"]),
      title: "First officer",
      org: "Starfleet",
      details: [{ label: "Birthday", value: "2335-08-19" }],
      note: "  plays trombone ",
      seen: { items: 1, last_at: "2364-03-01T09:00:00Z" },
    };
    expect(sectionLines(c)).toEqual([
      { label: "Also called", value: "Will" },
      { label: "Role", value: "First officer, Starfleet" },
      { label: "Birthday", value: "2335-08-19" },
      { label: "Note", value: "plays trombone" },
      { label: "Seen", value: "1 item, last 2364-03-01T09:00:00Z" },
    ]);
    expect(sectionLines(c, () => "Mar 1")).toContainEqual({
      label: "Seen",
      value: "1 item, last Mar 1",
    });
  });

  it("says nothing it does not have", () => {
    expect(sectionLines(contact("sms_backup", "1", ["Number One"]))).toEqual([]);
  });
});

/** The case that read as a puzzle: only the address is linked, and the
 *  Slack profile is on the card because it lists the same address. The
 *  card says so, and offers to link the Slack id too. */
describe("why a source is on the card, and what is linked", () => {
  const linkedByEmail = contact("datalib_contacts", "c-1", ["William Riker"], [EMAIL]);
  const other = contact("datalib_contacts", "c-2", ["Thomas Riker"], ["slack:T1/U9"]);
  const all = answers({
    [EMAIL]: { mine: linkedByEmail, sourceContacts: [slack] },
    "slack:T1/U2": { mine: null, sourceContacts: [slack] },
    "slack:T1/U9": { mine: other, sourceContacts: [] },
  });

  const whoOf = all.who;
  const nothing = new Set<string>();

  it("names the card's handle each section was found by", () => {
    const m = personModel(by(EMAIL), null, all);
    expect(m.sections.map((s) => [s.sourceId, s.via])).toEqual([["slack", [EMAIL]]]);
  });

  it("tells linked to this contact from linked to another and from nobody's", () => {
    expect(linkState(EMAIL, linkedByEmail, whoOf)).toBe("this");
    expect(linkState("slack:T1/U9", linkedByEmail, whoOf)).toBe("other");
    expect(linkState("slack:T1/U2", linkedByEmail, whoOf)).toBe("none");
    expect(linkState(EMAIL, null, whoOf)).toBe("other");
  });

  it("offers to add only what nobody holds, and only to a contact", () => {
    const m = personModel(by(EMAIL), null, all);
    const [section] = m.sections;
    expect(linkableHandles(section, m, whoOf, nothing)).toEqual(["slack:T1/U2"]);
    expect(linkableHandles(section, { ...m, mine: null }, whoOf, nothing)).toEqual([]);
    const withTaken = {
      ...section,
      contact: contact("slack", "T1/U9", ["Tom"], ["slack:T1/U9", EMAIL]),
    };
    expect(linkableHandles(withTaken, m, whoOf, nothing)).toEqual([]);
  });

  /** "Not theirs" is remembered: the record still holds the handle, and
   *  the page does not offer it again. */
  it("does not offer again what was said not to be theirs", () => {
    const m = personModel(by(EMAIL), null, all);
    const taken = new Set(["slack:T1/U2"]);
    expect(linkableHandles(m.sections[0], m, whoOf, taken)).toEqual([]);
  });

  it("asks about every handle any section holds", () => {
    expect(shownHandles(personModel(by(EMAIL), null, all)).sort()).toEqual(
      [EMAIL, "slack:T1/U2"].sort(),
    );
  });
});

/** A card whose number is written as it is dialled at home. */
function withHomeNumber(c: NormalizedContact, value: string): NormalizedContact {
  const home = {
    medium: "phone" as const,
    label: "cell",
    value,
    handle: null,
    stopped_working_by: null,
  };
  return { ...c, handles: [...c.handles, home] };
}

const FULL = "tel:+12025550147";
const HOME = "(202) 555-0147";

describe("the handles adding someone takes", () => {
  const SLACK = "slack:T1/U2";
  const unfiled = { mine: null, sourceContacts: [] };
  const offered = (subject: Subject, a: Answers) =>
    offers(subject, personModel(subject, null, a), a);
  const line = (o: { handle: string; ticked: boolean }) => [o.handle, o.ticked];

  it("is the one opened, then every other on a record that holds it, all ticked", () => {
    const book = contact("book", "riker", ["William Riker"], [EMAIL, TEL]);
    const a = answers({
      [EMAIL]: { mine: null, sourceContacts: [book, slack] },
      [TEL]: unfiled,
      [SLACK]: unfiled,
    });
    const got = offered(by(EMAIL), a);
    expect(got.map(line)).toEqual([
      [EMAIL, true],
      [TEL, true],
      [SLACK, true],
    ]);
    expect(got.map((o) => o.sources)).toEqual([["book", "slack"], ["book"], ["slack"]]);
  });

  /** One hop only: a handle's own records are not followed further. */
  it("does not follow a handle on to further records", () => {
    const book = contact("book", "riker", ["William Riker"], [EMAIL, TEL]);
    const a = answers({
      [EMAIL]: { mine: null, sourceContacts: [slack] },
      [SLACK]: { mine: null, sourceContacts: [slack, book] },
    });
    expect(offered(by(EMAIL), a).map((o) => o.handle)).toEqual([EMAIL, SLACK]);
  });

  it("offers one another contact has unticked, saying whose it is", () => {
    const worf = contact("datalib_contacts", "c-9", ["Worf"], [SLACK]);
    const a = answers({
      [EMAIL]: { mine: null, sourceContacts: [slack] },
      [SLACK]: { mine: worf, sourceContacts: [slack] },
    });
    const got = offered(by(EMAIL), a);
    expect(got.map(line)).toEqual([
      [EMAIL, true],
      [SLACK, false],
    ]);
    expect(offerNote(got[1], (id) => id)).toBe("on Worf");
  });

  it("leaves out a group's addresses, the library's own stand-in and a model", () => {
    const crew = { ...contact("book", "crew", ["Crew"], [EMAIL, TEL]), kind: "group" as const };
    const owner = contact("sms", "me", ["Me"], [EMAIL, "me:self", "ai_model:lcars"]);
    const a = answers({ [EMAIL]: { mine: null, sourceContacts: [crew, owner] } });
    expect(offered(by(EMAIL), a).map((o) => o.handle)).toEqual([EMAIL]);
  });

  it("offers nothing for someone who is in your contacts", () => {
    const a = answers({ [EMAIL]: { mine, sourceContacts: [slack] } });
    expect(offered(by(EMAIL), a)).toEqual([]);
  });

  /** From the card to the number: the library knows one number with its
   *  digits, so it is ticked; with two, in two countries, neither is. */
  it("takes a card's number with no country code as the full one it must be", () => {
    const geordi = withHomeNumber(contact("book", "g", ["Geordi"], [EMAIL]), HOME);
    const asked = (numbers: string[]) =>
      answers({
        [EMAIL]: { mine: null, sourceContacts: [geordi], fullNumbers: { [HOME]: numbers } },
      });
    const one = offered(by(EMAIL), asked([FULL]));
    expect(one.map(line)).toEqual([
      [EMAIL, true],
      [FULL, true],
    ]);
    expect(offerNote(one[1], (id) => id)).toBe(`written ${HOME} · book`);
    expect(offered(by(EMAIL), asked([FULL, "tel:+442025550147"])).map(line)).toEqual([
      [EMAIL, true],
      [FULL, false],
      ["tel:+442025550147", false],
    ]);
    expect(offered(by(EMAIL), asked([])).map(line)).toEqual([[EMAIL, true]]);
  });

  /** And from the number to the card, which holds no handle of it. */
  it("finds a card by its digits, with its other handles", () => {
    const gomez = withHomeNumber(contact("book", "s", ["Sonya Gomez"], [EMAIL]), HOME);
    const caller = contact("voice", FULL, ["+12025550147"], [FULL]);
    const asked = (numbers: string[]) =>
      answers({
        [FULL]: {
          mine: null,
          sourceContacts: [caller],
          sameDigits: [gomez],
          fullNumbers: { [HOME]: numbers },
        },
      });
    const m = personModel(by(FULL), null, asked([FULL]));
    expect(m.sections.map((s) => [s.sourceId, s.sameDigits])).toEqual([
      ["voice", []],
      ["book", [FULL]],
    ]);
    const got = offered(by(FULL), asked([FULL]));
    expect(got.map(line)).toEqual([
      [FULL, true],
      [EMAIL, true],
    ]);
    expect(got[0].written, "said beside the number").toBe(HOME);
    expect(offered(by(FULL), asked([FULL, "tel:+442025550147"])).map(line)).toEqual([
      [FULL, true],
      [EMAIL, false],
      ["tel:+442025550147", false],
    ]);
  });

  it("names at most three sources", () => {
    const o = offered(by(EMAIL), answers({ [EMAIL]: unfiled }))[0];
    const note = (sources: string[]) => offerNote({ ...o, sources }, (id) => id.toUpperCase());
    expect(note([])).toBe("");
    expect(note(["a", "b", "c"])).toBe("A, B, C");
    expect(note(["a", "b", "c", "d"])).toBe("A, B, 2 more");
  });
});

describe("a card with no handle", () => {
  const PAGE: Subject = { kind: "page", page: "p-1" };
  const gomez = withHomeNumber(contact("book", "s", ["Sonya Gomez"]), HOME);
  const page = (numbers: string[]): Record<string, PageAnswer> => ({
    "p-1": { contact: gomez, fullNumbers: numbers.length ? { [HOME]: numbers } : {} },
  });

  it("is headed by its own name, and offers its number where the library knows it", () => {
    const a = answers({ [FULL]: { mine: null, sourceContacts: [] } }, page([FULL]));
    const m = personModel(PAGE, null, a);
    expect([m.title, m.mine, m.sections.length]).toEqual(["Sonya Gomez", null, 1]);
    expect(offers(PAGE, m, a).map((o) => o.handle)).toEqual([FULL]);
  });

  it("offers no handle where the library knows no such number", () => {
    const a = answers({}, page([]));
    expect(offers(PAGE, personModel(PAGE, null, a), a)).toEqual([]);
  });

  /** Nothing on the card is on the contact but a number it may not be, so
   *  one of two numbers on a contact does not make the card theirs. */
  it("is its contact once its one number is on one", () => {
    const sonya = contact("datalib_contacts", "c-3", ["Sonya"], [FULL]);
    const who = { [FULL]: { mine: sonya, sourceContacts: [] } };
    expect(personModel(PAGE, null, answers(who, page([FULL]))).mine).toBe(sonya);
    const two = answers(who, page([FULL, "tel:+442025550147"]));
    expect(personModel(PAGE, null, two).mine).toBeNull();
  });
});

describe("a contact by its id", () => {
  const subject: Subject = { kind: "contact", id: "c-1" };

  it("has a page whether or not it has a handle", () => {
    const bare = contact("datalib_contacts", "c-1", ["Guinan"]);
    const m = personModel(subject, null, answers({}, {}, { "c-1": bare }));
    expect([m.title, m.mine, m.handles, m.sections]).toEqual(["Guinan", bare, [], []]);
  });

  it("shows every source that knows its handles", () => {
    const a = answers(
      { [TEL]: { mine, sourceContacts: [sms] }, [EMAIL]: { mine, sourceContacts: [slack] } },
      {},
      { "c-1": mine },
    );
    const m = personModel(subject, null, a);
    expect(m.sections.map((s) => s.sourceId)).toEqual(["sms_backup", "slack"]);
  });

  it("is nobody once deleted", () => {
    const m = personModel(subject, null, answers({}));
    expect([m.title, m.mine, m.handles]).toEqual(["", null, []]);
  });
});

describe("parseSubject", () => {
  it("reads a handle, a contact and a card's page", () => {
    expect(parseSubject(EMAIL)).toEqual(by(EMAIL));
    expect(parseSubject("contact:c-1")).toEqual({ kind: "contact", id: "c-1" });
    expect(parseSubject("page:p-1")).toEqual({ kind: "page", page: "p-1" });
  });
});

describe("recordHandles", () => {
  it("says each handle once, the full one before a number that may be it", () => {
    const c = withHomeNumber(contact("book", "s", ["Sonya"], [FULL]), HOME);
    expect(recordHandles(c, { [HOME]: [FULL] }).map((h) => [h.handle, h.written])).toEqual([
      [FULL, null],
    ]);
  });
});
