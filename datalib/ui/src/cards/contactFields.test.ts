// Your contact's fields beside the rest of the card: where a field's
// handle stands, and copying what a source says into the contact.

import { describe, expect, it } from "vitest";

import type { ContactEdit, Field } from "./contactEdit";
import {
  copyAll,
  copyInto,
  drift,
  holds,
  linkedNotFields,
  sourceValues,
  standing,
  valueText,
} from "./contactFields";
import type { NormalizedContact, Who } from "./contacts";

function record(over: Partial<NormalizedContact> = {}): NormalizedContact {
  return {
    source_id: "tng_contacts",
    key: "picard.vcf",
    kind: "person",
    names: ["Jean-Luc Picard"],
    handles: [
      {
        medium: "email",
        label: "work",
        value: "picard@enterprise.starfleet",
        handle: "email:picard@enterprise.starfleet",
        stopped_working_by: null,
      },
      {
        medium: "phone",
        label: "cell",
        value: "+1-202-555-0101",
        handle: "tel:+12025550101",
        stopped_working_by: null,
      },
      {
        medium: "other",
        label: null,
        value: "T_NCC1701D/U_PICARD",
        handle: "slack:T_NCC1701D/U_PICARD",
        stopped_working_by: null,
      },
    ],
    org: "Starfleet",
    title: "Captain",
    note: "Make it so.",
    details: [
      { label: "Address (work)", value: "Ready Room, Deck 1" },
      { label: "Birthday", value: "2305-07-13" },
      { label: "Friends since", value: "2364-01-05" },
    ],
    seen: null,
    ...over,
  };
}

const EMPTY: ContactEdit = { name: "Jean-Luc", note: null, fields: [] };
const FROM = { source_id: "tng_contacts", key: "picard.vcf" };
function ids() {
  let n = 0;
  return () => `f${++n}`;
}

describe("standing", () => {
  const mine = (key: string): Who => ({ mine: { ...record(), key }, sourceContacts: [] });
  const who: Record<string, Who> = {
    "email:a@b.test": mine("c1"),
    "email:troi@b.test": mine("troi"),
  };
  const whoOf = (h: string) => who[h];

  it("says whether a field's handle is this contact's, nobody's or someone else's", () => {
    expect(standing("email:a@b.test", "c1", whoOf)).toEqual({ state: "this" });
    expect(standing("email:new@b.test", "c1", whoOf)).toEqual({
      state: "none",
      handle: "email:new@b.test",
    });
    expect(standing("email:troi@b.test", "c1", whoOf)).toMatchObject({
      state: "other",
      owner: { key: "troi" },
    });
  });

  it("says nothing for a field that is no handle", () => {
    expect(standing(null, "c1", whoOf)).toBeNull();
  });
});

describe("linkedNotFields", () => {
  it("offers each working address and number no field holds, never a Slack id", () => {
    const linked = record().handles;
    expect(linkedNotFields(linked, ["email:picard@enterprise.starfleet", null])).toEqual([
      { handle: "tel:+12025550101", kind: "phone", value: "+1-202-555-0101", label: "cell" },
    ]);
  });

  it("leaves out a handle that stopped working", () => {
    const linked = record().handles.map((h) => ({ ...h, stopped_working_by: "2370" }));
    expect(linkedNotFields(linked, [])).toEqual([]);
  });
});

describe("sourceValues", () => {
  it("lists what a source says as a contact would hold it", () => {
    expect(sourceValues(record()).map(valueText)).toEqual([
      "Name “Jean-Luc Picard”",
      "Email (work) “picard@enterprise.starfleet”",
      "Phone (cell) “+1-202-555-0101”",
      "Title “Captain”",
      "Organization “Starfleet”",
      "Address (work) “Ready Room, Deck 1”",
      "Birthday “2305-07-13”",
      "Other (Friends since) “2364-01-05”",
      "Note “Make it so.”",
    ]);
  });
});

describe("copying", () => {
  it("adds a field that remembers its source, and copies a value once", () => {
    const title = sourceValues(record()).find((v) => v.part === "field" && v.kind === "title")!;
    const once = copyInto(EMPTY, title, FROM, ids());
    expect(once.fields).toEqual([
      { field_id: "f1", kind: "title", label: null, value: "Captain", copied_from: FROM },
    ]);
    expect(copyInto(once, title, FROM, ids())).toBe(once);
  });

  it("takes the name in place, and adds to a note rather than replacing it", () => {
    const [name] = sourceValues(record());
    expect(copyInto(EMPTY, name!, FROM, ids()).name).toBe("Jean-Luc Picard");
    const note = { part: "note" as const, value: "Make it so." };
    expect(copyInto({ ...EMPTY, note: "Earl Grey" }, note, FROM, ids()).note).toBe(
      "Earl Grey\n\nMake it so.",
    );
  });

  it("knows a number or an address written differently is the same", () => {
    const edit: ContactEdit = {
      ...EMPTY,
      fields: [
        {
          field_id: "x",
          kind: "phone",
          label: null,
          value: "+1 (202) 555-0101",
          copied_from: null,
        },
        {
          field_id: "y",
          kind: "email",
          label: null,
          value: "Picard@Enterprise.Starfleet",
          copied_from: null,
        },
      ],
    };
    const values = sourceValues(record()).filter((v) => v.part === "field");
    expect(values.slice(0, 2).map((v) => holds(edit, v))).toEqual([true, true]);
  });

  it("copies everything but the name with Copy all", () => {
    const all = copyAll(EMPTY, sourceValues(record()), FROM, ids());
    expect(all.name).toBe("Jean-Luc");
    expect(all.note).toBe("Make it so.");
    expect(all.fields.map((f) => f.kind)).toEqual([
      "email",
      "phone",
      "title",
      "organization",
      "address",
      "birthday",
      "other",
    ]);
  });
});

describe("drift", () => {
  const copied = (value: string): Field => ({
    field_id: "t",
    kind: "title",
    label: null,
    value,
    copied_from: FROM,
  });

  it("is nothing while the source still says the copied value", () => {
    expect(drift(copied("captain"), record())).toBeNull();
  });

  it("offers the source's one new value of that kind", () => {
    expect(drift(copied("Commander"), record())).toEqual({ now: "Captain" });
  });

  it("says the source no longer says it when there is no one value to take", () => {
    expect(drift(copied("Captain"), record({ title: null }))).toEqual({ now: null });
  });

  it("is nothing for a field typed by hand or copied from another source", () => {
    expect(drift({ ...copied("Ensign"), copied_from: null }, record())).toBeNull();
    expect(drift({ ...copied("Ensign"), copied_from: { ...FROM, key: "other" } }, record())).toBe(
      null,
    );
  });
});
