// What the contacts app's table (`ContactsCard.ce.vue`) does with the rows
// `GET /applet/datalib_contacts/contacts` answers: which a filter keeps,
// and the handle a row's chip names. Pure, so it is tested alone.

import { handleFromUri } from "./chipLinks";

/** One row of the table, as the applet serves it. */
export type ContactRow = {
  key: string;
  contact_ref: { id: string; label: string; icon?: string; detail?: string };
  email: string | null;
  phone: string | null;
  handles: number;
  note: string | null;
  kind: string;
  updated_at: string | null;
};

/** The handle a row's chip is drawn by; null for a contact with none. */
export function rowHandle(row: ContactRow): string | null {
  return handleFromUri(row.contact_ref.id) ?? null;
}

/** The rows whose name, address, number or note holds every word of
 *  `text`, ignoring case; every row for blank text. */
export function filterContacts(rows: ContactRow[], text: string): ContactRow[] {
  const words = text.toLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return rows;
  return rows.filter((r) => {
    const hay = [r.contact_ref.label, r.email, r.phone, r.note]
      .filter((v): v is string => !!v)
      .join("\n")
      .toLowerCase();
    return words.every((w) => hay.includes(w));
  });
}
