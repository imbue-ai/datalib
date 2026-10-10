// The contacts app's table: what its filter keeps, and which handle a
// row's chip names.

import { describe, expect, it } from "vitest";

import { filterContacts, mergeInto, rowHandle, type ContactRow } from "./contactsTable";

function row(key: string, label: string, id: string, extra: Partial<ContactRow> = {}): ContactRow {
  return {
    key,
    contact_ref: { id, label },
    email: null,
    phone: null,
    handles: 0,
    note: null,
    kind: "Contact",
    updated_at: null,
    ...extra,
  };
}

const riker = row("c1", "Will Riker", "mailto:riker@enterprise.org", {
  email: "riker@enterprise.org",
  note: "Plays the trombone",
});
const q = row("c2", "Q", "datalib:contact/c2");
const troi = row("c3", "Deanna Troi", "tel:+15550100", { phone: "+15550100" });

describe("filterContacts", () => {
  it("keeps every row for blank text", () => {
    expect(filterContacts([riker, q, troi], "  ")).toEqual([riker, q, troi]);
  });

  it("matches every word, in the name, address, number or note, ignoring case", () => {
    expect(filterContacts([riker, q, troi], "TROMBONE will")).toEqual([riker]);
    expect(filterContacts([riker, q, troi], "5550100")).toEqual([troi]);
    expect(filterContacts([riker, q, troi], "riker q")).toEqual([]);
  });
});

describe("rowHandle", () => {
  it("is the handle the chip is drawn by, or none for a contact without one", () => {
    expect(rowHandle(riker)).toBe("email:riker@enterprise.org");
    expect(rowHandle(troi)).toBe("tel:+15550100");
    expect(rowHandle(q)).toBeNull();
  });
});

describe("mergeInto", () => {
  it("merges every other row aimed at into the one clicked, and nothing for one row", () => {
    expect(mergeInto(riker, [q, riker, troi]).map((r) => r.key)).toEqual(["c2", "c3"]);
    expect(mergeInto(riker, [riker])).toEqual([]);
  });
});
