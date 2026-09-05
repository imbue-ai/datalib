# GitHub Translate

`github-translate` reads the event-store JSONL written by
`github-download` and emits **one markdown document per pull request**,
plus that document's `grid_rows` for the UI's flat-row view.

```
<root>/<stanza>/rendered_md/<owner>/<repo>/pr-<num>/
    index.md                # the unified PR doc
<root>/<stanza>/rendered_md/indexed_markdown.doltlite_db
                            # its rows: one for the PR + one per comment
```

## Markdown layout

1. **Front matter** — provider, repo, pr_number, title, state, author,
   head/base sha+ref, created/updated/merged timestamps.
2. **Title** — `# {title} (#{num})` + a "View on GitHub" link + a one-line
   `*{state}* — @{author} — \`{head_ref}\` → \`{base_ref}\``.
3. **Description** — `pull_request.body` as-is, or `*(no description)*`.
4. **Reviews** — one block per `pr_review`, oldest first. Header carries
   the reviewer, the review state (`COMMENTED`, `APPROVED`, …), and a
   `[link]` permalink to `#pullrequestreview-N`.
5. **General discussion** — `issue_comments`, oldest first. Permalinks
   to `#issuecomment-N`.
6. **Inline comments** — `pr_review_comments` grouped by `(path, line)`,
   then chronologically within each thread. Replies inherit their
   parent's anchor (so a multi-message thread on `foo.rs:42` stays
   together even if the diff has moved). Each comment carries a `[link]`
   permalink to `#discussion_rN`.

Each comment block is blockquoted, with the header line spelling out
`**@user** *(state)* *(reply)* @ <ts> — [link](...)`.

## Rows

The same `RenderedMarkdown { markdown_uuid, source_fingerprint, rows }`
shape every provider emits:

- `markdown_uuid` — UUIDv5 of `github:{repo}:pr:{num}`.
- `source_fingerprint` — DefaultHasher hash of `RENDER_VERSION`
  + canonicalized PR JSON + canonicalized comment JSONs (sorted by
  `upstream_id`). Re-renders that didn't change content produce an
  identical row set, so the store's commit does not move.
- `rows[0]` — the PR row itself (kind = "GitHub PR").
- `rows[1..]` — one row per comment, in the same order as the rendered
  doc (Reviews → General → Inline-by-`(path, line)`). `message_index`
  is the row index *within the doc*; the UI uses
  `data-msg-index="N"` to scroll the unified doc to the right anchor.

All rows share the same `qmd_path` (the PR's `index.md`),
`conversation_uuid`, and `document_uuid` (all == the PR UUID).
`external_id` is the GitHub PR number for the head row and the comment
or review id for the rest.

## Run it

The translate step is an in-process library (the `render_and_index_md`
module, called from `datalib-sync`); there is no standalone
`github-translate` binary and no Bazel target for it. Run a sync to
exercise it, and rendered docs land under
`/tmp/github-mirror/<stanza>/rendered_md/...`.

To exercise the renderer in isolation, run its tests:

```sh
bazelisk test //datalib/backend/etl/providers/github:github_unittests
```
