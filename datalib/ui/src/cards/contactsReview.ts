// What the contacts app's review (`ContactsReview.ce.vue`) does with the
// rows `GET /applet/datalib_contacts/review` answers: which a filter keeps,
// the labels to offer, and the handle a row's chip names. Pure, so it is
// tested alone.

import { handleFromUri } from "./chipLinks";

/** `datalib_contacts::REAL`, hand-kept: the label agents curate, in when
 *  someone or something is not spam. */
export const REAL = "real";

/** One row of the review, as the applet serves it. `key` is the subject:
 *  a handle, or `contact:<id>`. */
export type ReviewRow = {
  key: string;
  contact_id: string | null;
  who: { id: string; label: string; icon?: string; detail?: string };
  /** What the `real` label says: in, out, or `null` when nobody has. */
  real: boolean | null;
  label_values: string[];
  /// How sure the agent was of the least sure thing waiting here.
  confidence: number | null;
  status: string;
  awaiting: boolean;
  why: string;
  agent: string;
};

export type ReviewFilter = {
  /** Only what waits for a person. */
  waiting: boolean;
  /** What `real` must say, or `""` for any. */
  real: "" | "in" | "out" | "unknown";
  /** A label the subject holds in, or `""` for any. */
  label: string;
  text: string;
};

export function filterReview(rows: ReviewRow[], f: ReviewFilter): ReviewRow[] {
  const words = f.text.toLowerCase().split(/\s+/).filter(Boolean);
  return rows.filter((r) => {
    if (f.waiting && !r.awaiting) return false;
    if (f.real && f.real !== (r.real == null ? "unknown" : r.real ? "in" : "out")) return false;
    if (f.label && !r.label_values.includes(f.label)) return false;
    if (words.length === 0) return true;
    const hay = [r.who.label, r.who.detail, r.key, r.why, ...r.label_values]
      .filter((v): v is string => !!v)
      .join("\n")
      .toLowerCase();
    return words.every((w) => hay.includes(w));
  });
}

/** Close calls first: what waits for you, least sure first and those
 *  that say nothing of how sure last, then everything else as it came. */
export function orderReview(rows: ReviewRow[]): ReviewRow[] {
  const rank = (r: ReviewRow) => (!r.awaiting ? 3 : r.confidence == null ? 2 : 1);
  return rows
    .map((r, i) => ({ r, i }))
    .sort(
      (a, b) =>
        rank(a.r) - rank(b.r) ||
        (a.r.awaiting && a.r.confidence != null && b.r.confidence != null
          ? a.r.confidence - b.r.confidence
          : 0) ||
        a.i - b.i,
    )
    .map(({ r }) => r);
}

/** The label a membership column's field names (`label:<label>`). */
export function membershipLabel(field: string): string | null {
  return field.startsWith("label:") ? field.slice("label:".length) : null;
}

/** Every label some row holds in but `real`, most used
 *  first: the clusters a person can filter by. */
export function clusterLabels(rows: ReviewRow[]): { label: string; count: number }[] {
  const counts = new Map<string, number>();
  for (const r of rows) {
    for (const l of r.label_values) {
      if (l === REAL) continue;
      counts.set(l, (counts.get(l) ?? 0) + 1);
    }
  }
  return [...counts]
    .map(([label, count]) => ({ label, count }))
    .sort((a, b) => b.count - a.count || a.label.localeCompare(b.label));
}

/** The handle a row's chip is drawn by; null for a contact with none. */
export function reviewHandle(row: ReviewRow): string | null {
  return handleFromUri(row.who.id) ?? null;
}
