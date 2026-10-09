// Which columns a Browse card opens with, per source type.
//
// A preset is a *ceiling*, not a fixed set. The grid still applies its
// adaptive rule inside it (GridCard's `applyAdaptiveVisibility`), hiding
// any of these whose values are all the same across the rows actually
// loaded. So naming a column a provider only sometimes fills costs
// nothing — an empty one disappears on its own. What a preset does is
// keep columns that are *meaningless* for a source from ever appearing:
// no Channel on a GitHub browse, no Project on WhatsApp.
//
// The sets come from what each provider's render crate puts in each
// column (the rules are `docs/dev/grid_rows.md` § "Column conventions"),
// checked against a real index.

import type { SearchRow } from "@/api";

/// A column a preset may name. Typed against the row the grid actually
/// paints, so a preset naming a field that does not exist is a compile
/// error rather than a column that silently never appears — the grid
/// ignores an unknown column id without complaint.
export type BrowseColumn = keyof SearchRow;

/// Columns every source's browse opens with, in this order, before the
/// type's `EXTRA`. `kind` leads because it is the within-source
/// discriminator even among documents (Claude has chats and projects,
/// Notion has pages and comment threads); then what the document is
/// called and what it says, so the text is on screen at any width. One
/// stamp: a Browse is one row per document, and when it was last touched
/// is what tells a live thread from a dead one. Unlike Modified, Touched
/// is never empty on a row that has a stamp at all. A type can swap the
/// stamp (`STAMP`) or leave a column out (`OMIT`).
const ALWAYS: BrowseColumn[] = ["kind", "conversation_name", "snippet", "touched_at"];

/// The stamp a type shows in place of `touched_at`.
const STAMP: Record<string, BrowseColumn> = {
  // An event's created_at is when it happens, often years ahead; its
  // touched_at is only when it was last edited.
  calendar: "created_at",
};

/// Extra columns per source type, after `ALWAYS`.
const EXTRA: Record<string, BrowseColumn[]> = {
  // Channelled group chat: who said it, and where.
  slack: ["channel", "author_ref"],
  whatsapp: ["channel", "author_ref"],
  signal: ["channel", "author_ref"],
  beeper: ["channel", "author_ref", "account", "project"],
  google_takeout: ["channel", "author_ref", "project"],
  sms_backup_restore: ["channel", "author_ref", "project"],
  apple_messages: ["channel", "author_ref"],
  linkedin: ["channel", "author_ref", "account"],
  // Posts, albums, comments, reactions and friends, all the owner's own:
  // `author` is who wrote it, `account` whose export it is.
  facebook: ["author_ref", "account"],

  // Mail and address books: a correspondent and a mailbox.
  email: ["channel", "author_ref", "account"],
  contacts: ["channel", "author_ref", "account"],
  // A calendar and the organizer.
  calendar: ["channel", "author_ref", "account"],

  // Assistant chats. `project` is Claude's project name and `org_name`
  // is the owning Anthropic organization — the only provider with one.
  claude: ["project", "org_name", "account", "author_ref"],
  chatgpt: ["channel", "account", "author_ref"],

  // Code review: `project` is the repo (GitHub) or the project path
  // (GitLab). Neither has a channel.
  github: ["project", "author_ref"],
  gitlab: ["project", "author_ref"],

  notion: ["account", "author_ref"],
  perseus: ["author_ref"],
  yolink: ["channel"],
  // The device rows: `channel` is the device name.
  garmin: ["channel", "author_ref"],

  // The one source with a real per-row size and count: a document's
  // bytes, and its page count. Everywhere else those two are non-null
  // only on the storage rows, where they would be noise.
  pdf: ["author_ref", "byte_size", "item_count"],
};

/// Columns of `ALWAYS` a source type leaves out.
const OMIT: Record<string, BrowseColumn[]> = {
  // A thread's conversation name is its channel's name again
  // (`#general`, `@Picard`), so Channel alone says where it is.
  slack: ["conversation_name"],
};

/// Every source type this file names a preset for. Exists so a test can
/// check them against the catalog: a key misspelled here is not an
/// error, it silently falls through to the generic preset below.
export function browsePresetTypes(): string[] {
  return [...new Set([...Object.keys(EXTRA), ...Object.keys(OMIT), ...Object.keys(STAMP)])];
}

/// The columns a Browse of a source of this type opens with, or `null`
/// for the unified projection — a browse across every source keeps the
/// grid's own defaults, because there the source columns are the point.
export function browseColumns(type: string | null): BrowseColumn[] | null {
  if (!type) return null;
  if (type === DIFF_TYPE) return DIFF_COLUMNS;
  const extra = EXTRA[type] ?? ["channel", "author_ref", "account", "project"];
  const omit = OMIT[type] ?? [];
  const stamp = STAMP[type] ?? "touched_at";
  const always = ALWAYS.filter((c) => !omit.includes(c)).map((c) =>
    c === "touched_at" ? stamp : c,
  );
  return [...always, ...extra];
}

/// A diff group (`docs/dev/plans/completed/diff_renderer.md`) is not a source
/// type the catalog offers, so it is not in `EXTRA`: its rows are the
/// underlying source's, and what a browse of one is for is *what
/// changed* — every row that did, not one per document, with the two
/// diff columns first.
const DIFF_TYPE = "diff";
const DIFF_COLUMNS: BrowseColumn[] = [
  "diff_status",
  "diff_changed_columns",
  "kind",
  "conversation_name",
  "snippet",
  "channel",
  "author_ref",
  "touched_at",
];

/// The name a Browse card opens with: "Slack documents", or for a diff,
/// whose browse is what moved, "<name> changes".
export function browseName(sourceName: string, type: string | null = null): string {
  return type === DIFF_TYPE ? `${sourceName} changes` : `${sourceName} documents`;
}

/// The search a Browse of this group opens: the documents filed under
/// it — one row per thread, conversation, PR or page, not the messages
/// inside them, which repeat the document's name down the grid and are
/// one chip-delete away (`is:document`). A group id is its directory
/// under the data root, which is what `source_id:` matches on. Its own
/// data, not datalib's report on it: the storage rows sit in the same
/// directory but are filed under `datalib`, and the filter leaves them
/// out.
export function browseQuery(groupId: string, type: string | null = null): string {
  // A diff's browse is the rows that moved: `-change:unchanged` drops
  // the rows a changed document carries for context, and every diff
  // row is worth a line of its own, so no `is:document`.
  if (type === DIFF_TYPE) return `source_id:${groupId} -change:unchanged`;
  return `source_id:${groupId} is:document`;
}
