// Your contact's lines beside the rest of the card: who else has an
// address or number typed into it, and copying a value from what a source
// says about the person into the contact (docs/dev/contacts.md § "Fields,
// and editing a contact through a draft"). Pure; `ContactEditor.ce.vue`
// and `PersonCard.ce.vue` draw it.

import { kindLabel, type ContactEdit, type Field, type FieldKind } from "./contactEdit";
import type { NormalizedContact, Who } from "./contacts";

/** Where a typed line's handle stands: this contact's already, nobody's,
 *  or others', who are named. Null for a line that is no handle. */
export type Standing =
  | { state: "this" }
  | { state: "none"; handle: string }
  | { state: "other"; handle: string; owners: NormalizedContact[] };

export function standing(
  handle: string | null | undefined,
  contactId: string,
  whoOf: (handle: string) => Who | undefined,
): Standing | null {
  if (!handle) return null;
  const owners = whoOf(handle)?.mine ?? [];
  if (owners.length === 0) return { state: "none", handle };
  return owners.some((c) => c.key === contactId)
    ? { state: "this" }
    : { state: "other", handle, owners };
}

/** One thing a source says that a contact can hold: its name, its note,
 *  or a value that becomes a field. */
export type SourceValue =
  | { part: "name"; value: string }
  | { part: "note"; value: string }
  | { part: "field"; kind: FieldKind; label: string | null; value: string };

/** A source detail's label as a field: `Address (work)` is an address
 *  labelled "work"; one the card has no kind for is Other, labelled as
 *  the source labels it. */
function detailField(label: string): { kind: FieldKind; label: string | null } {
  const m = /^(.*?)\s*(?:\((.*)\))?$/.exec(label.trim());
  const head = (m?.[1] ?? "").toLowerCase();
  const sub = m?.[2]?.trim() || null;
  if (head === "address") return { kind: "address", label: sub };
  if (head === "birthday") return { kind: "birthday", label: sub };
  if (head === "website" || head === "url") return { kind: "url", label: sub };
  return { kind: "other", label: label.trim() || null };
}

/** A source's address or number as a line of your contact; null for a
 *  handle with no address or number of its own (a Slack id). */
export function handleValueOf(h: NormalizedContact["handles"][number]): SourceValue | null {
  if (h.medium === "other" || !h.value.trim()) return null;
  return {
    part: "field",
    kind: h.medium === "email" ? "email" : "phone",
    label: h.label,
    value: h.value,
  };
}

/** A source's detail ("Address (work)") as a line of your contact. */
export function detailValue(d: { label: string; value: string }): SourceValue | null {
  return d.value.trim() ? { part: "field", ...detailField(d.label), value: d.value } : null;
}

export const titleValue = (title: string): SourceValue => ({
  part: "field",
  kind: "title",
  label: null,
  value: title,
});

export const orgValue = (org: string): SourceValue => ({
  part: "field",
  kind: "organization",
  label: null,
  value: org,
});

/** Everything `c` says that a contact can hold, in the card's order:
 *  name, addresses and numbers, role, the rest, the note. */
export function sourceValues(c: NormalizedContact): SourceValue[] {
  const out: SourceValue[] = [];
  const name = c.names[0]?.trim();
  if (name) out.push({ part: "name", value: name });
  for (const h of c.handles) {
    const v = handleValueOf(h);
    if (v) out.push(v);
  }
  if (c.title?.trim()) out.push(titleValue(c.title));
  if (c.org?.trim()) out.push(orgValue(c.org));
  for (const d of c.details ?? []) {
    const v = detailValue(d);
    if (v) out.push(v);
  }
  if (c.note?.trim()) out.push({ part: "note", value: c.note.trim() });
  return out;
}

/** A value as two sources might write it differently: an address in any
 *  case, a number in any punctuation. */
function comparable(kind: FieldKind, value: string): string {
  const v = value.trim().toLowerCase();
  if (kind === "phone") return v.replace(/[^\d+]/g, "");
  return v.replace(/\s+/g, " ");
}

/** Whether `edit` already says `v`, so copying it would change nothing. */
export function holds(edit: ContactEdit, v: SourceValue): boolean {
  switch (v.part) {
    case "name":
      return edit.name.trim() === v.value.trim();
    case "note":
      return (edit.note ?? "").includes(v.value.trim());
    case "field": {
      const want = comparable(v.kind, v.value);
      return edit.fields.some((f) => f.kind === v.kind && comparable(f.kind, f.value) === want);
    }
  }
}

export type CopiedFrom = NonNullable<Field["copied_from"]>;

/** `edit` with `v` copied in: the name replaced, the note added to, or a
 *  field added that remembers where it came from. A value the contact
 *  already holds is left alone. */
export function copyInto(
  edit: ContactEdit,
  v: SourceValue,
  from: CopiedFrom,
  newId: () => string,
): ContactEdit {
  if (holds(edit, v)) return edit;
  switch (v.part) {
    case "name":
      return { ...edit, name: v.value.trim() };
    case "note": {
      const note = edit.note?.trim();
      return { ...edit, note: note ? `${note}\n\n${v.value.trim()}` : v.value.trim() };
    }
    case "field":
      return {
        ...edit,
        fields: [
          ...edit.fields,
          {
            field_id: newId(),
            kind: v.kind,
            label: v.label,
            value: v.value.trim(),
            copied_from: from,
          },
        ],
      };
  }
}

/** How a value reads on its copy button's line. */
export function valueText(v: SourceValue): string {
  switch (v.part) {
    case "name":
      return `Name “${v.value}”`;
    case "note":
      return `Note “${v.value}”`;
    case "field":
      return `${kindLabel(v.kind)}${v.label ? ` (${v.label})` : ""} “${v.value}”`;
  }
}

/** Whether a copied field's source now says something else. A copy does
 *  not follow its source, so the card says so instead: `now` is what the
 *  source says now, when it says one thing of that kind; null when it no
 *  longer says this and there is no one value to take. Null overall when
 *  the source still says it, or the field was not copied from `record`. */
export function drift(field: Field, record: NormalizedContact): { now: string | null } | null {
  const from = field.copied_from;
  if (!from || from.source_id !== record.source_id || from.key !== record.key) return null;
  const same = sourceValues(record).flatMap((v) =>
    v.part === "field" && v.kind === field.kind ? [v.value] : [],
  );
  const want = comparable(field.kind, field.value);
  if (same.some((s) => comparable(field.kind, s) === want)) return null;
  return { now: same.length === 1 ? same[0]!.trim() : null };
}
