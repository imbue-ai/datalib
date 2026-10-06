// What the search grid opens with when its card source asks for nothing
// more: one row per document, and a hint whose examples are this
// library's own sources rather than ones it may not have.
import type { RowGroup, SearchRow } from "@/api";

/// A thread, an email, a PR, a chat — not every message inside it. One
/// term, so deleting it from the search bar shows every row.
export const DEFAULT_QUERY = "is:document";

/// The hint when there is no source to name.
export const PLAIN_HINT = "search…";

/// Datalib's report on each source's storage, filed under this id.
const DATALIB_SOURCE_ID = "datalib";

/// The hint for an empty search bar, from the sources the index holds
/// (grouped by `source_ref`): the biggest one's id to filter on, and the
/// year of its newest row to filter from. `PLAIN_HINT` when there is
/// nothing to name.
export function searchPlaceholder(bySource: RowGroup<Partial<SearchRow>>[]): string {
  const biggest = bySource
    .filter((g) => g.values[0] && g.values[0] !== DATALIB_SOURCE_ID)
    .reduce<RowGroup<Partial<SearchRow>> | null>(
      (best, g) => (best && best.count >= g.count ? best : g),
      null,
    );
  if (!biggest) return PLAIN_HINT;
  const examples = [`source_id:${biggest.values[0]}`];
  const year = biggest.sample.touched_at?.slice(0, 4);
  if (year && /^\d{4}$/.test(year)) examples.push(`after:${year}-01-01`);
  return `${PLAIN_HINT}  (try: ${examples.join(", ")})`;
}
