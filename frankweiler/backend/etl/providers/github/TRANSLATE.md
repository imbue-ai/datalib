# GitHub Translate

`github-translate` reads the event-store JSONL written by
`github-download` and emits **one markdown document per pull request**,
plus a co-located `grid_rows` sidecar for the UI's flat-row view.

```
<root>/rendered_md/github/<owner>/<repo>/pr-<num>__<slug>/
    index.md                # the unified PR doc
    index.grid_rows.json     # sidecar: one row for the PR + one per comment
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

## Sidecar

The sidecar is the same `Sidecar { header, rows }` shape used by the
other providers:

- `header.document_uuid` — UUIDv5 of `github:{repo}:pr:{num}`.
- `header.source_fingerprint` — DefaultHasher hash of `RENDER_VERSION`
  + canonicalized PR JSON + canonicalized comment JSONs (sorted by
  `external_id`). Re-renders that didn't change content produce
  byte-identical sidecars.
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

```sh
cargo run -p frankweiler-etl-github --bin github-translate -- --out /tmp/github-mirror
# rendered docs land under /tmp/github-mirror/rendered_md/github/...
```
