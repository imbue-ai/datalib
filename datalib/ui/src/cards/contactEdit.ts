// Editing your contact on its card: what the card edits, as the contacts
// app's draft routes speak it (`datalib_contacts::drafts`, hand-kept),
// and the three-way rule that shows a change made elsewhere while you
// edit (docs/dev/plans/contact_editing.md § "Seeing another writer's
// change"). Pure; `ContactEditor.ce.vue` draws it and `contacts.ts`
// makes the calls.

/** `datalib_contacts::FieldKind`, hand-kept. */
export type FieldKind =
  "email" | "phone" | "organization" | "title" | "birthday" | "address" | "url" | "other";

/** Every kind, in the order the card offers them, with its label. */
export const FIELD_KINDS: readonly { kind: FieldKind; label: string }[] = [
  { kind: "phone", label: "Phone" },
  { kind: "email", label: "Email" },
  { kind: "title", label: "Title" },
  { kind: "organization", label: "Organization" },
  { kind: "address", label: "Address" },
  { kind: "birthday", label: "Birthday" },
  { kind: "url", label: "Website" },
  { kind: "other", label: "Other" },
];

export function kindLabel(kind: FieldKind): string {
  return FIELD_KINDS.find((k) => k.kind === kind)?.label ?? kind;
}

export type Field = {
  /** Minted by the card for a new field, so each autosave names it alike. */
  field_id: string;
  kind: FieldKind;
  label: string | null;
  value: string;
  /** The value as a handle, worked out by the store; ignored on the way in. */
  handle?: string | null;
  copied_from: { source_id: string; key: string } | null;
};

export type ContactEdit = { name: string; note: string | null; fields: Field[] };

/** A draft beside what it was cut from and what is published now. */
export type DraftView = {
  base: ContactEdit | null;
  mine: ContactEdit | null;
  published: ContactEdit | null;
  /** What a save names to say "I have seen this". */
  published_commit: string;
};

export type Saved =
  { outcome: "saved"; commit: string | null } | { outcome: "stale"; view: DraftView };

export function newField(kind: FieldKind, fieldId: string): Field {
  return { field_id: fieldId, kind, label: null, value: "", copied_from: null };
}

/** What an autosave sends: the edit less the fields still empty, which
 *  the store would keep as empty lines. */
export function toStore(edit: ContactEdit): ContactEdit {
  return {
    name: edit.name,
    note: edit.note?.trim() ? edit.note : null,
    fields: edit.fields
      .filter((f) => f.value.trim())
      .map((f) => ({ ...f, label: f.label?.trim() ? f.label : null })),
  };
}

/** Where the name, the note or one field changed both in the draft and,
 *  differently, in what is published. `theirs` is the published value;
 *  a field's `theirs` is null where it was removed elsewhere. */
export type Conflict =
  | { part: "name"; theirs: string }
  | { part: "note"; theirs: string | null }
  | { part: "field"; fieldId: string; theirs: Field | null };

const sameText = (a: string | null | undefined, b: string | null | undefined) =>
  (a ?? "").trim() === (b ?? "").trim();

/** Two fields say the same; the store's `handle` is not part of it. */
export function sameField(a: Field | undefined, b: Field | undefined): boolean {
  if (!a || !b) return a === b;
  return (
    a.kind === b.kind &&
    sameText(a.label, b.label) &&
    sameText(a.value, b.value) &&
    a.copied_from?.source_id === b.copied_from?.source_id &&
    a.copied_from?.key === b.copied_from?.key
  );
}

/** One part of the three-way rule: unchanged in the draft takes the
 *  published value; changed only in the draft, or to the same thing on
 *  both sides, keeps the draft's; changed differently on both is a
 *  conflict, and the draft's value stands until the person picks. */
function pick<T>(
  base: T,
  mine: T,
  published: T,
  same: (a: T, b: T) => boolean,
): { value: T; conflict: boolean } {
  if (same(mine, base)) return { value: published, conflict: false };
  if (same(published, base) || same(published, mine)) return { value: mine, conflict: false };
  return { value: mine, conflict: true };
}

/** What the card shows while editing, from what the draft was cut from,
 *  what it says now (the card's own form, which may be ahead of the
 *  last autosave) and what is published: the same three-way merge the
 *  save makes, shown before it happens. A field is a unit here; the
 *  store merges cell by cell, so this is the stricter of the two. */
export function merge3(
  base: ContactEdit,
  mine: ContactEdit,
  published: ContactEdit,
): { edit: ContactEdit; conflicts: Conflict[] } {
  const conflicts: Conflict[] = [];
  const name = pick(base.name, mine.name, published.name, sameText);
  if (name.conflict) conflicts.push({ part: "name", theirs: published.name });
  const note = pick(base.note, mine.note, published.note, sameText);
  if (note.conflict) conflicts.push({ part: "note", theirs: published.note });

  const byId = (e: ContactEdit) => new Map(e.fields.map((f) => [f.field_id, f]));
  const was = byId(base);
  const now = byId(published);
  const kept = new Set(mine.fields.map((f) => f.field_id));
  const fields: Field[] = [];
  for (const m of mine.fields) {
    const b = was.get(m.field_id);
    const p = now.get(m.field_id);
    if (!b) {
      fields.push(m);
    } else if (!p) {
      if (sameField(m, b)) continue;
      fields.push(m);
      conflicts.push({ part: "field", fieldId: m.field_id, theirs: null });
    } else {
      const f = pick<Field | undefined>(b, m, p, sameField);
      fields.push(f.value ?? m);
      if (f.conflict) conflicts.push({ part: "field", fieldId: m.field_id, theirs: p });
    }
  }
  for (const b of base.fields) {
    if (kept.has(b.field_id)) continue;
    const p = now.get(b.field_id);
    if (p && !sameField(p, b)) conflicts.push({ part: "field", fieldId: b.field_id, theirs: p });
  }
  for (const p of published.fields) {
    if (!was.has(p.field_id) && !kept.has(p.field_id)) fields.push(p);
  }
  return { edit: { name: name.value, note: note.value, fields }, conflicts };
}

/** The edit with a conflict settled the published way. */
export function takeTheirs(edit: ContactEdit, c: Conflict): ContactEdit {
  switch (c.part) {
    case "name":
      return { ...edit, name: c.theirs };
    case "note":
      return { ...edit, note: c.theirs };
    case "field": {
      const theirs = c.theirs;
      if (!theirs) return { ...edit, fields: edit.fields.filter((f) => f.field_id !== c.fieldId) };
      const at = edit.fields.findIndex((f) => f.field_id === c.fieldId);
      const fields =
        at < 0 ? [...edit.fields, theirs] : edit.fields.map((f, i) => (i === at ? theirs : f));
      return { ...edit, fields };
    }
  }
}

/** How a conflict reads on the card, as a sentence. */
export function conflictText(c: Conflict): string {
  switch (c.part) {
    case "name":
      return `The name was changed elsewhere to “${c.theirs}”.`;
    case "note":
      return c.theirs?.trim()
        ? `The note was changed elsewhere to “${c.theirs.trim()}”.`
        : "The note was removed elsewhere.";
    case "field": {
      const theirs = c.theirs;
      return theirs
        ? `Changed elsewhere to ${kindLabel(theirs.kind)} “${theirs.value}”.`
        : "Removed elsewhere.";
    }
  }
}

/** A field this draft removed that was changed elsewhere, as a sentence. */
export function removedText(theirs: Field): string {
  return `You removed a field that was changed elsewhere to ${kindLabel(theirs.kind)} “${theirs.value}”.`;
}

/** Whether the draft says anything the published contact does not. */
export function hasChanges(edit: ContactEdit, published: ContactEdit): boolean {
  const a = toStore(edit);
  const b = toStore(published);
  return (
    !sameText(a.name, b.name) ||
    !sameText(a.note, b.note) ||
    a.fields.length !== b.fields.length ||
    a.fields.some((f, i) => f.field_id !== b.fields[i]!.field_id || !sameField(f, b.fields[i]))
  );
}

/** The lines the card shows for a contact it is not editing: each
 *  field as its label (the person's, else the kind's) and value, then
 *  the note. */
export function editLines(edit: ContactEdit): { label: string; value: string }[] {
  const out = edit.fields.map((f) => ({
    label: f.label?.trim() ? `${kindLabel(f.kind)} (${f.label.trim()})` : kindLabel(f.kind),
    value: f.value,
  }));
  if (edit.note?.trim()) out.push({ label: "Note", value: edit.note.trim() });
  return out;
}

/** What a drop on the contact's photo carries: an image file, from the
 *  desktop or another app, else the address of an image on this app's
 *  own origin, such as a source's photo dragged from the card. An image
 *  on another site is refused rather than fetched: the app fetches
 *  nothing remote unasked, and its pages may not. Null when the drop
 *  holds no image at all. */
export type PhotoDrop = { file: File } | { url: string } | { refused: string };

export function photoDrop(
  dt: Pick<DataTransfer, "files" | "getData">,
  origin: string,
  types: readonly string[],
): PhotoDrop | null {
  const file = Array.from(dt.files)[0];
  if (file) {
    return types.includes(file.type)
      ? { file }
      : { refused: `${file.name} is not a photo: drop a PNG, JPEG, GIF or WebP image.` };
  }
  const uri = dt
    .getData("text/uri-list")
    .split(/\r?\n/)
    .find((l) => l.trim() && !l.startsWith("#"))
    ?.trim();
  if (!uri) return null;
  let url: URL;
  try {
    url = new URL(uri, origin);
  } catch {
    return null;
  }
  if (url.origin !== origin) {
    return {
      refused:
        "That image is on another site, which datalib does not fetch. Save it, then drop the file.",
    };
  }
  return { url: url.pathname + url.search };
}
