// Editing a contact on its card: what an autosave sends, and the
// three-way rule that shows a change made elsewhere while you edit.

import { describe, expect, it } from "vitest";

import {
  conflictText,
  editLines,
  hasChanges,
  merge3,
  photoDrop,
  removedText,
  takeTheirs,
  toStore,
  type ContactEdit,
  type Field,
  type FieldKind,
} from "./contactEdit";

function field(id: string, kind: FieldKind, value: string, label: string | null = null): Field {
  return { field_id: id, kind, label, value, copied_from: null };
}

function edit(name: string, note: string | null = null, fields: Field[] = []): ContactEdit {
  return { name, note, fields };
}

const RIKER = edit("William Riker", "plays trombone", [
  field("t", "title", "First officer"),
  field("p", "phone", "+1 555 0100"),
]);

describe("toStore", () => {
  it("leaves out a field not yet filled in, and an empty note or label", () => {
    const e = edit("Will", "  ", [field("a", "phone", ""), field("b", "url", "x", " ")]);
    expect(toStore(e)).toEqual(edit("Will", null, [field("b", "url", "x", null)]));
  });
});

describe("merge3", () => {
  it("shows the draft's change where nothing was published since", () => {
    const mine = { ...RIKER, name: "Will Riker" };
    expect(merge3(RIKER, mine, RIKER)).toEqual({ edit: mine, conflicts: [] });
  });

  it("takes a change published elsewhere to a part the draft left alone", () => {
    const mine = { ...RIKER, name: "Will Riker" };
    const published = { ...RIKER, note: "plays poker" };
    const { edit: shown, conflicts } = merge3(RIKER, mine, published);
    expect(shown.name).toBe("Will Riker");
    expect(shown.note).toBe("plays poker");
    expect(conflicts).toEqual([]);
  });

  it("marks a part both sides changed differently, keeping the draft's value", () => {
    const mine = { ...RIKER, name: "Will Riker" };
    const published = { ...RIKER, name: "Number One" };
    const { edit: shown, conflicts } = merge3(RIKER, mine, published);
    expect(shown.name).toBe("Will Riker");
    expect(conflicts).toEqual([{ part: "name", theirs: "Number One" }]);
  });

  it("does not mark a part both sides changed to the same thing", () => {
    const both = { ...RIKER, name: "Number One" };
    expect(merge3(RIKER, both, both).conflicts).toEqual([]);
  });

  it("merges fields by id: added on either side, changed, removed", () => {
    const mine = edit(RIKER.name, RIKER.note, [
      field("t", "title", "Captain"),
      field("e", "email", "riker@titan.test"),
    ]);
    const published = edit(RIKER.name, RIKER.note, [
      field("t", "title", "First officer"),
      field("p", "phone", "+1 555 0100"),
      field("u", "url", "https://example.org/riker"),
    ]);
    const { edit: shown, conflicts } = merge3(RIKER, mine, published);
    expect(shown.fields.map((f) => [f.field_id, f.value])).toEqual([
      ["t", "Captain"],
      ["e", "riker@titan.test"],
      ["u", "https://example.org/riker"],
    ]);
    expect(conflicts).toEqual([]);
  });

  it("marks a field changed on one side and removed on the other", () => {
    // Changed here, removed elsewhere: kept, and marked.
    const changedHere = merge3(
      RIKER,
      edit(RIKER.name, RIKER.note, [field("t", "title", "Captain"), RIKER.fields[1]!]),
      edit(RIKER.name, RIKER.note, [RIKER.fields[1]!]),
    );
    expect(changedHere.edit.fields.map((f) => f.field_id)).toEqual(["t", "p"]);
    expect(changedHere.conflicts).toEqual([{ part: "field", fieldId: "t", theirs: null }]);

    // Removed here, changed elsewhere: stays removed, and marked.
    const elsewhere = field("t", "title", "Captain");
    const removedHere = merge3(
      RIKER,
      edit(RIKER.name, RIKER.note, [RIKER.fields[1]!]),
      edit(RIKER.name, RIKER.note, [elsewhere, RIKER.fields[1]!]),
    );
    expect(removedHere.edit.fields.map((f) => f.field_id)).toEqual(["p"]);
    expect(removedHere.conflicts).toEqual([{ part: "field", fieldId: "t", theirs: elsewhere }]);
  });

  it("drops a field removed elsewhere that the draft left alone", () => {
    const published = edit(RIKER.name, RIKER.note, [RIKER.fields[0]!]);
    const { edit: shown, conflicts } = merge3(RIKER, RIKER, published);
    expect(shown.fields.map((f) => f.field_id)).toEqual(["t"]);
    expect(conflicts).toEqual([]);
  });

  it("does not count the store's handle, or blank versus missing, as a change", () => {
    const stored = edit(RIKER.name, RIKER.note, [
      { ...RIKER.fields[0]!, label: "" },
      { ...RIKER.fields[1]!, handle: "tel:+15550100" },
    ]);
    expect(merge3(RIKER, RIKER, stored).conflicts).toEqual([]);
    expect(hasChanges(RIKER, stored)).toBe(false);
  });
});

describe("takeTheirs", () => {
  it("settles each kind of conflict the published way", () => {
    expect(takeTheirs(RIKER, { part: "name", theirs: "Number One" }).name).toBe("Number One");
    expect(takeTheirs(RIKER, { part: "note", theirs: null }).note).toBeNull();
    const captain = field("t", "title", "Captain");
    expect(takeTheirs(RIKER, { part: "field", fieldId: "t", theirs: captain }).fields[0]).toBe(
      captain,
    );
    const gone = takeTheirs(RIKER, { part: "field", fieldId: "t", theirs: null });
    expect(gone.fields.map((f) => f.field_id)).toEqual(["p"]);
    const url = field("u", "url", "https://example.org");
    const back = takeTheirs(gone, { part: "field", fieldId: "u", theirs: url });
    expect(back.fields.map((f) => f.field_id)).toEqual(["p", "u"]);
  });

  it("says what changed elsewhere", () => {
    expect(conflictText({ part: "name", theirs: "Number One" })).toBe(
      "The name was changed elsewhere to “Number One”.",
    );
    const captain = field("t", "title", "Captain");
    expect(conflictText({ part: "field", fieldId: "t", theirs: captain })).toBe(
      "Changed elsewhere to Title “Captain”.",
    );
    expect(conflictText({ part: "field", fieldId: "t", theirs: null })).toBe("Removed elsewhere.");
    expect(removedText(captain)).toBe(
      "You removed a field that was changed elsewhere to Title “Captain”.",
    );
  });
});

describe("hasChanges", () => {
  it("sees a change to the name, the note, a field or the fields' order", () => {
    expect(hasChanges(RIKER, RIKER)).toBe(false);
    expect(hasChanges({ ...RIKER, name: "Will" }, RIKER)).toBe(true);
    expect(hasChanges({ ...RIKER, note: null }, RIKER)).toBe(true);
    expect(hasChanges({ ...RIKER, fields: [...RIKER.fields].reverse() }, RIKER)).toBe(true);
  });

  it("does not count a field added but not yet filled in", () => {
    expect(hasChanges({ ...RIKER, fields: [...RIKER.fields, field("x", "url", "")] }, RIKER)).toBe(
      false,
    );
  });
});

describe("editLines", () => {
  it("lists each field under its kind and label, then the note", () => {
    const e = edit("Will", "plays trombone", [
      field("p", "phone", "+1 555 0100", "work"),
      field("u", "url", "https://example.org"),
    ]);
    expect(editLines(e)).toEqual([
      { label: "Phone (work)", value: "+1 555 0100" },
      { label: "Website", value: "https://example.org" },
      { label: "Note", value: "plays trombone" },
    ]);
  });
});

describe("photoDrop", () => {
  const ORIGIN = "http://127.0.0.1:5173";
  const TYPES = ["image/png", "image/jpeg"];
  function dropped(files: File[], data: Record<string, string> = {}) {
    return { files: files as unknown as FileList, getData: (k: string) => data[k] ?? "" };
  }

  it("takes an image file, and refuses a file that is no photo", () => {
    const png = new File(["x"], "troi.png", { type: "image/png" });
    expect(photoDrop(dropped([png]), ORIGIN, TYPES)).toEqual({ file: png });
    const pdf = new File(["x"], "log.pdf", { type: "application/pdf" });
    expect(photoDrop(dropped([pdf]), ORIGIN, TYPES)).toEqual({
      refused: "log.pdf is not a photo: drop a PNG, JPEG, GIF or WebP image.",
    });
  });

  it("takes an image dragged from the app itself, as a path on its origin", () => {
    const uri = `${ORIGIN}/data/slack/ingest/blob/abc?x=1`;
    expect(
      photoDrop(dropped([], { "text/uri-list": `# comment\r\n${uri}` }), ORIGIN, TYPES),
    ).toEqual({ url: "/data/slack/ingest/blob/abc?x=1" });
  });

  it("refuses an image on another site rather than fetching it", () => {
    const got = photoDrop(
      dropped([], { "text/uri-list": "https://example.org/troi.png" }),
      ORIGIN,
      TYPES,
    );
    expect(got).toHaveProperty("refused");
  });

  it("is nothing for a drop with no image", () => {
    expect(photoDrop(dropped([]), ORIGIN, TYPES)).toBeNull();
  });
});
