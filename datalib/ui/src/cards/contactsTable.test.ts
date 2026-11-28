// The contacts app's table: what its filter keeps, which handle a row's
// chip names, how a list sorts, and who adding Unfiled rows makes.

import { describe, expect, it } from "vitest";

import type { NormalizedContact } from "./contacts";
import {
  contactSortKey,
  filterContacts,
  filterUnfiled,
  mergeInto,
  peopleToAdd,
  sortList,
  withCounts,
  type ContactRow,
  type UnfiledRow,
} from "./contactsTable";

function row(key: string, label: string, id: string, extra: Partial<ContactRow> = {}): ContactRow {
  return {
    key,
    contact_ref: { id, label },
    email: null,
    phone: null,
    handles: 0,
    note: null,
    labels: [],
    updated_at: null,
    ...extra,
  };
}

const riker = row("c1", "Will Riker", "datalib:contact/c1", {
  email: "riker@enterprise.org",
  note: "Plays the trombone",
});
const q = row("c2", "Q", "datalib:contact/c2");
const troi = row("c3", "Deanna Troi", "datalib:contact/c3", { phone: "+12025550100" });

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

describe("mergeInto", () => {
  it("merges every other row aimed at into the one clicked, and nothing for one row", () => {
    expect(mergeInto(riker, [q, riker, troi]).map((r) => r.key)).toEqual(["c2", "c3"]);
    expect(mergeInto(riker, [riker])).toEqual([]);
  });
});

describe("sortList", () => {
  const counted = withCounts([riker, q, troi], {
    c1: { items: 4, last_at: "2369-01-01T00:00:00Z" },
    c3: { items: 9, last_at: "2368-01-01T00:00:00Z" },
  });
  const names = (sort: "recent" | "most" | "name") =>
    sortList(counted, sort, contactSortKey).map((r) => r.contact_ref.label);

  it("puts the newest first, the most first, or by name; one that wrote nothing last", () => {
    expect(names("recent")).toEqual(["Will Riker", "Deanna Troi", "Q"]);
    expect(names("most")).toEqual(["Deanna Troi", "Will Riker", "Q"]);
    expect(names("name")).toEqual(["Deanna Troi", "Q", "Will Riker"]);
  });

  it("counts a contact that wrote nothing as none", () => {
    expect(counted.find((r) => r.key === "c2")).toMatchObject({ items: 0, last_at: null });
  });
});

function unfiled(handle: string, label: string): UnfiledRow {
  return { handle, person: { id: handle, label }, marks: [], items: 1, last_at: null };
}

function record(
  source: string,
  key: string,
  name: string,
  handles: [string, string | null][],
): NormalizedContact {
  return {
    source_id: source,
    key,
    kind: "person",
    names: [name],
    handles: handles.map(([handle, label]) => ({
      medium: handle.startsWith("email:") ? "email" : handle.startsWith("tel:") ? "phone" : "other",
      label,
      value: handle.slice(handle.indexOf(":") + 1),
      handle,
      stopped_working_by: null,
    })),
  } as NormalizedContact;
}

describe("filterUnfiled", () => {
  it("matches every word in the name or the handle", () => {
    const rows = [unfiled("email:worf@enterprise.org", "Worf"), unfiled("tel:+15550101", "Data")];
    expect(filterUnfiled(rows, "WORF enterprise").map((r) => r.handle)).toEqual([
      "email:worf@enterprise.org",
    ]);
  });
});

describe("peopleToAdd", () => {
  /** Worf's card ties his address and number; his Slack account is his
   *  own record. Two rows of his that meet on a handle are one person,
   *  with every handle and the card's labels; Data's row is another. */
  it("makes one person of the rows a record ties together, with its labels", () => {
    const card = record("book", "worf.vcf", "Worf", [
      ["email:worf@enterprise.org", "work"],
      ["tel:+15550104040", "cell"],
    ]);
    const records: Record<string, NormalizedContact[]> = {
      "email:worf@enterprise.org": [card],
      "tel:+15550104040": [card],
    };
    const got = peopleToAdd(
      [
        unfiled("email:worf@enterprise.org", "Lieutenant Worf"),
        unfiled("tel:+15550101", "Data"),
        unfiled("tel:+15550104040", "Worf"),
      ],
      (h) => records[h] ?? [],
    );
    expect(got).toEqual([
      {
        name: "Worf",
        handles: ["email:worf@enterprise.org", "tel:+15550104040"],
        said: {
          "email:worf@enterprise.org": {
            label: "work",
            copied_from: { source_id: "book", key: "worf.vcf" },
          },
          "tel:+15550104040": {
            label: "cell",
            copied_from: { source_id: "book", key: "worf.vcf" },
          },
        },
      },
      { name: "Data", handles: ["tel:+15550101"], said: {} },
    ]);
  });

  /** Wesley's Discord account and his number: no record ties them, and
   *  adding both at once made two Wesley Crushers. */
  it("makes one person of rows of one name, case aside", () => {
    const got = peopleToAdd(
      [unfiled("discord:9", "Wesley Crusher"), unfiled("tel:+15550142", "wesley crusher")],
      () => [],
    );
    expect(got).toEqual([
      { name: "Wesley Crusher", handles: ["discord:9", "tel:+15550142"], said: {} },
    ]);
  });
});
