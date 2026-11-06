// The order a search's rows come in, as `/search` spells it
// (`touched_at:desc,author_ref`), and what the Search card's control offers.
// No sort is the search's own order: best match for free text, else newest
// first.
import type { ColumnType } from "@/api";

export type Sorter = { columnId: string; direction: "ASC" | "DESC" };
export type SortChoice = { label: string; sort: string };

const NEWEST = "touched_at:desc";
const RANK = "score:desc";

const OTHER_CHOICES: SortChoice[] = [
  { label: "Oldest first", sort: "touched_at:asc" },
  { label: "Largest first", sort: "byte_size:desc" },
  { label: "Smallest first", sort: "byte_size:asc" },
  { label: "Author A to Z", sort: "author_ref:asc" },
  { label: "Title A to Z", sort: "subject_ref:asc" },
];

/// Which of `sortChoices` `sort` is: `""` when it asks for what no sort
/// already gives.
export function chosenSort(sort: string | null, ranked: boolean): string {
  if (!sort) return "";
  return sort === (ranked ? RANK : NEWEST) ? "" : sort;
}

/// What the control offers. An order only the table's headers can ask for,
/// by channel say, is offered too, so the control never claims another.
export function sortChoices(sort: string | null, ranked: boolean): SortChoice[] {
  const choices = [
    ...(ranked
      ? [
          { label: "Best match", sort: "" },
          { label: "Newest first", sort: NEWEST },
        ]
      : [{ label: "Newest first", sort: "" }]),
    ...OTHER_CHOICES,
  ];
  const chosen = chosenSort(sort, ranked);
  return choices.some((c) => c.sort === chosen)
    ? choices
    : [...choices, { label: "By the table's columns", sort: chosen }];
}

export function sortersOf(sort: string | null): Sorter[] {
  if (!sort) return [];
  return sort.split(",").map((one) => {
    const [columnId, direction] = one.split(":");
    return { columnId, direction: direction === "desc" ? "DESC" : "ASC" };
  });
}

export function sortOfSorters(
  sorters: readonly { columnId: string | number; direction: string }[],
): string | null {
  if (sorters.length === 0) return null;
  return sorters.map((s) => `${s.columnId}:${s.direction.toLowerCase()}`).join(",");
}

/// Whether the rows are in order of size, which a list then has to show.
export function sortsBySize(sort: string | null): boolean {
  return sortersOf(sort)[0]?.columnId === "byte_size";
}

/// Whether a column's first click asks for its largest or newest: nobody
/// sorts by a date, a size or a score to see the least of it first.
export function startsDescending(type: ColumnType): boolean {
  return ["count", "number", "bytes", "timestamp", "datetime", "quantity"].includes(type);
}
