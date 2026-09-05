# Notion Translate

`notion-translate` reads the event-store JSONL written by
`notion-download` and emits one CommonMark file per Notion page plus
that document's rows in the source's render store.

```
<out>/
  notion_official_{page,block,comment}/{created,updated}/events.jsonl   # input
  rendered_md/
    notion/
      <title-slug>__<short-id>/
        index.md                                 # the page itself
                                                 # (its row, "Notion Page",
                                                 #  goes to the render store)
        discussions/
          <discussion-short>__<snippet>.md       # one md per comment thread
          <discussion-short>__<snippet>.grid_rows.json
```

For each page parse picks the **latest** record per id across the
`created/` and `updated/` streams, so a refetched page renders from its
newest state with no need to compact the JSONL.

## Document model

Each Notion **page** becomes one document. Each **discussion** (a
comment thread anchored to a block on that page) becomes a separate
document, so an active thread doesn't churn the page-level fingerprint
every time someone replies.

| Document         | Page kind                      | Row set                                       |
|------------------|--------------------------------|-----------------------------------------------|
| Page             | one `Notion Page` row          | one row per page                              |
| Discussion thread| one `Notion Comment Thread` row + N `Notion Comment` rows | one of each, plus the thread row             |

Each document carries a `source_fingerprint` computed by hashing the
canonicalized (recursively-sorted) rows. Reruns that don't change the
content produce an identical row set; downstream consumers use the
fingerprint to skip unchanged documents.

## Block coverage

The renderer handles the full official-API block matrix:

- text & headings: `paragraph`, `heading_{1,2,3}`, `quote`, `callout`
- lists: `bulleted_list_item`, `numbered_list_item`, `to_do`, `toggle`
- code: `code`, `equation`
- media: `image`, `video`, `audio`, `pdf`, `file`, `embed`, `bookmark`, `link_preview`
- structure: `divider`, `column_list`, `column`, `table`, `table_row`, `table_of_contents`, `breadcrumb`
- references: `link_to_page`, `child_page`, `child_database`, `synced_block`
- catch-all: `unsupported`

Mentions resolve to page titles when the referenced page is also in
the capture; otherwise they degrade to a stable URL.

## Unofficial-API fallbacks

Two lookup tables are read opportunistically from a backfilled
unofficial-API capture if present in the input directory:

- `notion_user` — for comment-author display names (the official API
  only returns user IDs unless the integration also has user-read
  scopes).
- `notion_block` — for `prod-files-secure` media URLs and bookmark
  titles that the official API leaves blank.

These are optional. Without them, comments render with raw user IDs
and media falls back to a placeholder link.

## CLI

```sh
notion-translate --out ~/backups/notion
notion-translate --out ~/backups/notion --render-root /tmp/notion-render
```

`--render-root` defaults to `<out>/rendered_md/`.
