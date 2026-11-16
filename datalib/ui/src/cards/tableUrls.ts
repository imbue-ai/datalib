// The tables a card's source may point `gridView({url})` or
// `tableView({url})` at. Card source comes from links anyone can write
// (docs/dev/cards.md § "Card source"), and a grid draws whatever its url
// answers: the columns, the rows, the errors it toasts and the link each
// row opens. So a url is taken only when it is one of the app's own
// tables, named here in full, with no query and no other spelling.
import { REMOTE_ALLOW_TABLE, REMOTE_FETCHED_TABLE, SEARCH, UNIFIED_DOCUMENT_INDEX } from "@/api";

const PROBLEMS = `${UNIFIED_DOCUMENT_INDEX}/problems`;

/// Tables that page, sort and group the way the search does.
export const GRID_TABLES: readonly string[] = [SEARCH, PROBLEMS];

/// Tables that answer `{columns, rows}` (docs/dev/cards.md § "Typed tables").
export const TYPED_TABLES: readonly string[] = [
  "/api/manage/rows",
  REMOTE_ALLOW_TABLE,
  REMOTE_FETCHED_TABLE,
  ...GRID_TABLES,
];

export function listedTable(view: string, url: unknown, listed: readonly string[]): string {
  if (typeof url === "string" && listed.includes(url)) return url;
  throw new Error(
    `${view} shows only the app's own tables (${listed.join(", ")}), not ${JSON.stringify(url)}.`,
  );
}
