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
// The sets come from the per-provider mapping in `docs/dev/grid_rows.md`,
// corrected against a real index where the two disagreed — that doc's
// `account` / `project` / `channel` table lists eight providers and at
// least eight more populate `channel`.

import type { SearchRow } from "@/api";

/// A column a preset may name. Typed against the row the grid actually
/// paints, so a preset naming a field that does not exist is a compile
/// error rather than a column that silently never appears — AG Grid
/// ignores an unknown `colId` without complaint.
export type BrowseColumn = keyof SearchRow;

/// Columns every source's browse opens with, in this order. `kind` leads
/// because it is the within-source discriminator even among documents
/// (Claude has chats and projects, Notion has pages and comment
/// threads). Both stamps, because a Browse is one row per document and
/// "last touched" is what tells a live thread from a dead one.
const ALWAYS: BrowseColumn[] = [
  "kind",
  "created_at",
  "modified_at",
  "conversation_name",
  "snippet",
];

/// Extra columns per source type, inserted before `snippet`.
const EXTRA: Record<string, BrowseColumn[]> = {
  // Channelled group chat: who said it, and where.
  slack: ["channel", "author"],
  whatsapp: ["channel", "author"],
  signal: ["channel", "author"],
  beeper: ["channel", "author", "account", "project"],
  google_takeout: ["channel", "author", "project"],
  sms_backup_restore: ["channel", "author", "project"],
  apple_messages: ["channel", "author"],
  linkedin: ["channel", "author", "account"],
  // Posts, albums, comments, reactions and friends, all the owner's own:
  // `author` is who wrote it, `account` whose export it is.
  facebook: ["author", "account"],

  // Mail and address books: a correspondent and a mailbox.
  email: ["channel", "author", "account"],
  contacts: ["channel", "author", "account"],

  // Assistant chats. `project` is Claude's project name and `org_name`
  // is the owning Anthropic organization — the only provider with one.
  claude: ["project", "org_name", "account", "author"],
  chatgpt: ["channel", "account", "author"],

  // Code review: `project` is the repo (GitHub) or the project path
  // (GitLab). Neither has a channel.
  github: ["project", "author"],
  gitlab: ["project", "author"],

  notion: ["account", "author"],
  perseus: ["author"],
  yolink: ["channel"],
  // The device rows: `channel` is the device name.
  garmin: ["channel", "author"],

  // The one source with a real per-row size and count: a document's
  // bytes, and its page count. Everywhere else those two are non-null
  // only on the storage rows, where they would be noise.
  pdf: ["author", "byte_size", "item_count"],
};

/// Every source type this file names a preset for. Exists so a test can
/// check them against the catalog: a key misspelled here is not an
/// error, it silently falls through to the generic preset below.
export function browsePresetTypes(): string[] {
  return Object.keys(EXTRA);
}

/// The columns a Browse of a source of this type opens with, or `null`
/// for the unified projection — a browse across every source keeps the
/// grid's own defaults, because there the source columns are the point.
export function browseColumns(type: string | null): BrowseColumn[] | null {
  if (!type) return null;
  const extra = EXTRA[type] ?? ["channel", "author", "account", "project"];
  return [...ALWAYS.slice(0, -1), ...extra, "snippet"];
}

/// The search a Browse of this group opens: the documents filed under
/// it — one row per thread, conversation, PR or page, not the messages
/// inside them, which repeat the document's name down the grid and are
/// one chip-delete away (`is:document`). A group id is its directory
/// under the data root, which is what `source_id:` matches on. Its own
/// data, not datalib's report on it: the storage rows sit in the same
/// directory but are filed under `datalib`, and the filter leaves them
/// out.
export function browseQuery(groupId: string): string {
  return `source_id:${groupId} is:document`;
}
