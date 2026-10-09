// The person card's rules: what it is about, which sections it shows,
// and in what order.

import { describe, expect, it } from "vitest";

import type { NormalizedContact, Who } from "./contacts";
import { personHandles, personModel, sectionLines } from "./person";

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

const answers =
  (byHandle: Record<string, Who>) =>
  (h: string): Who | undefined =>
    byHandle[h];

describe("personModel", () => {
  it("an unlinked handle is the card's subject, with each source's record of it", () => {
    const m = personModel(
      TEL,
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
      TEL,
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
      EMAIL,
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
      EMAIL,
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
    const m = personModel(TEL, "whatsapp", () => undefined);
    expect(m).toEqual({ title: "+15550100", mine: null, handles: [TEL], sections: [] });
  });
});

describe("personHandles", () => {
  it("leads with the opened handle and skips a link the rules no longer read", () => {
    const old = { ...mine, handles: [...mine.handles, { ...mine.handles[0], handle: null }] };
    expect(personHandles(TEL, { mine: old, sourceContacts: [] })).toEqual([TEL, EMAIL]);
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
