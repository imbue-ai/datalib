// What the contacts app's table (`ContactsCard.ce.vue`) does with the rows
// `GET /applet/datalib_contacts/contacts` answers, and with Unfiled's from
// `GET /applet/unified_grid_and_search_index/people/unfiled`: which a
// filter keeps, how much each wrote and the order that sets, the handle a
// row's chip names, and who adding Unfiled rows makes. Pure, so it is
// tested alone.

import type { ColumnSpec } from "@/api";
import type { HandleFrom, NewPerson, NormalizedContact } from "./contacts";
import { recordHandles } from "./person";

/** One row of the table, as the applet serves it. */
export type ContactRow = {
  key: string;
  contact_ref: { id: string; label: string; icon?: string; detail?: string };
  email: string | null;
  phone: string | null;
  handles: number;
  note: string | null;
  /** Its labels but `real`, as chips. */
  labels: { text: string }[];
  updated_at: string | null;
  /** How much its handles wrote: what `from:contact:` finds. */
  items?: number;
  last_at?: string | null;
};

/** How much someone wrote: their rows in the grid, and the newest's stamp. */
export type Wrote = { items: number; last_at: string | null };

/** One row of Unfiled: someone who wrote and is on no contact. */
export type UnfiledRow = {
  handle: string;
  person: { id: string; label: string; icon?: string; detail?: string };
  marks: { kind: string; text: string; title: string }[];
  items: number;
  last_at: string | null;
};

/** How a list is ordered. */
export type ListSort = "recent" | "most" | "name";

export const LIST_SORTS: readonly { sort: ListSort; label: string }[] = [
  { sort: "recent", label: "Most recent" },
  { sort: "most", label: "Most items" },
  { sort: "name", label: "A–Z" },
];

/** The columns the contacts table adds to what the contacts app serves:
 *  how much each contact wrote, which the index counts. */
export const COUNT_COLUMNS: ColumnSpec[] = [
  {
    field: "items",
    header: "Items",
    type: "count",
    description: "How many rows their handles wrote: what `from:` finds.",
    default_visible: true,
    editable: false,
  },
  {
    field: "last_at",
    header: "Last",
    type: "timestamp",
    description: "When they last wrote.",
    default_visible: true,
    editable: false,
  },
];

/** The contacts app's columns with the counts after the person, where
 *  a narrow card still shows what the list is sorted by. */
export function withCountColumns(columns: ColumnSpec[]): ColumnSpec[] {
  const at = columns.findIndex((c) => c.field === "contact_ref") + 1;
  return [...columns.slice(0, at), ...COUNT_COLUMNS, ...columns.slice(at)];
}

/** The rows with what each contact wrote; one that wrote nothing, 0. */
export function withCounts(rows: ContactRow[], counts: Record<string, Wrote>): ContactRow[] {
  return rows.map((r) => ({
    ...r,
    items: counts[r.key]?.items ?? 0,
    last_at: counts[r.key]?.last_at ?? null,
  }));
}

/** `rows` in the order `sort` says: the newest writing first, the most
 *  written first, or by name; a tie by name, then by key. */
export function sortList<T>(
  rows: T[],
  sort: ListSort,
  of: (row: T) => { name: string; key: string; items: number; last: string | null },
): T[] {
  const byName = (a: T, b: T) => {
    const x = of(a);
    const y = of(b);
    return (
      x.name.localeCompare(y.name, undefined, { sensitivity: "base" }) || x.key.localeCompare(y.key)
    );
  };
  const byLast = (a: T, b: T) => (of(b).last ?? "").localeCompare(of(a).last ?? "");
  const byItems = (a: T, b: T) => of(b).items - of(a).items;
  const order: Record<ListSort, (a: T, b: T) => number> = {
    recent: (a, b) => byLast(a, b) || byItems(a, b) || byName(a, b),
    most: (a, b) => byItems(a, b) || byLast(a, b) || byName(a, b),
    name: byName,
  };
  return [...rows].sort(order[sort]);
}

export const contactSortKey = (r: ContactRow) => ({
  name: r.contact_ref.label,
  key: r.key,
  items: r.items ?? 0,
  last: r.last_at ?? null,
});

export const unfiledSortKey = (r: UnfiledRow) => ({
  name: r.person.label,
  key: r.handle,
  items: r.items,
  last: r.last_at,
});

/** The Unfiled rows whose name or handle holds every word of `text`. */
export function filterUnfiled(rows: UnfiledRow[], text: string): UnfiledRow[] {
  const words = text.toLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return rows;
  return rows.filter((r) => {
    const hay = `${r.person.label}\n${r.handle}`.toLowerCase();
    return words.every((w) => hay.includes(w));
  });
}

/** Who adding `rows` makes: each with every handle a source's record of
 *  them says is theirs, `records` giving those for a handle, and what that
 *  record said of each. Rows whose handles meet are one person, and so are
 *  rows of one name, case aside, as adding an address book's rows has it.
 *  Named as the first record names them, else as the row shows them. */
export function peopleToAdd(
  rows: UnfiledRow[],
  records: (handle: string) => NormalizedContact[],
): NewPerson[] {
  type Gathered = { name: string; handles: string[]; said: Record<string, HandleFrom> };
  const people: Gathered[] = [];
  for (const row of rows) {
    const found = records(row.handle);
    const said: Record<string, HandleFrom> = {};
    const handles = [row.handle];
    for (const c of found) {
      for (const h of recordHandles(c, {})) {
        if (!h.sure) continue;
        if (!handles.includes(h.handle)) handles.push(h.handle);
        said[h.handle] ??= {
          label: h.label,
          copied_from: { source_id: c.source_id, key: c.key },
        };
      }
    }
    const name = found.find((c) => c.names[0]?.trim())?.names[0]?.trim() ?? row.person.label;
    const sameName = (p: Gathered) => p.name.toLowerCase() === name.toLowerCase();
    const meets = people.filter((p) => sameName(p) || p.handles.some((h) => handles.includes(h)));
    const into = meets[0];
    if (!into) {
      people.push({ name, handles, said });
      continue;
    }
    for (const other of meets.slice(1)) {
      into.handles.push(...other.handles.filter((h) => !into.handles.includes(h)));
      into.said = { ...other.said, ...into.said };
      people.splice(people.indexOf(other), 1);
    }
    into.handles.push(...handles.filter((h) => !into.handles.includes(h)));
    into.said = { ...said, ...into.said };
  }
  return people.map((p) => ({ name: p.name, handles: p.handles, said: p.said }));
}

/** What merging the rows a right-click aims at comes to: every target
 *  but the row clicked goes into it. Empty when only one row is aimed at. */
export function mergeInto(anchor: ContactRow, targets: ContactRow[]): ContactRow[] {
  return targets.filter((r) => r.key !== anchor.key);
}

/** The rows whose name, address, number or note holds every word of
 *  `text`, ignoring case; every row for blank text. */
/** The rows that carry `label`; every row for none. */
export function withLabel(rows: ContactRow[], label: string | null): ContactRow[] {
  return label === null ? rows : rows.filter((r) => r.labels.some((l) => l.text === label));
}

export function filterContacts(rows: ContactRow[], text: string): ContactRow[] {
  const words = text.toLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return rows;
  return rows.filter((r) => {
    const hay = [r.contact_ref.label, r.email, r.phone, r.note, ...r.labels.map((l) => l.text)]
      .filter((v): v is string => !!v)
      .join("\n")
      .toLowerCase();
    return words.every((w) => hay.includes(w));
  });
}
