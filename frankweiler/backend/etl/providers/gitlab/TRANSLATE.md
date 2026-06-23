# GitLab Translate

`gitlab-translate` reads the event-store JSONL written by
`gitlab-download` and emits **one markdown document per merge request**,
plus a co-located `grid_rows` sidecar.

```
<root>/rendered_md/gitlab/<namespace>/<project>/mr-<iid>__<slug>/
    index.md                # the unified MR doc
    index.grid_rows.json     # sidecar: one row for the MR + one per note
```

## Markdown layout

1. **Front matter** — provider, project, mr_iid, title, state, author,
   head/base sha, source/target branches, timestamps.
2. **Title** — `# {title} (!{iid})` + a "View on GitLab" link + one-line
   `*{state}* — @{author} — \`{source}\` → \`{target}\``.
3. **Description** — `merge_request.description` as-is.
4. **General discussion** — discussions with `individual_note: true` or
   no `position`, sorted by note `created_at`. Permalinks to
   `{mr.web_url}#note_{id}`.
5. **Inline comments** — discussions with a `position` (diff-anchored),
   grouped by `(position.new_path, position.new_line)`. Replies stay
   with their parent because every note in a GitLab discussion already
   carries the same position.

`system: true` notes (label add/remove, WIP toggles, etc.) are dropped
— they're git audit log, not conversation.

## Sidecar

Same `Sidecar { header, rows }` shape as the other providers:

- `header.document_uuid` — UUIDv5 of `gitlab:{project}:mr:{iid}`.
- `header.source_fingerprint` — DefaultHasher hash of `RENDER_VERSION`
  + canonicalized MR JSON + canonicalized note JSONs (sorted by note
  id). Stable across re-renders.
- `rows[0]` — the MR row (kind = "GitLab MR").
- `rows[1..]` — one row per surviving note (General first, then
  Inline-by-`(path, line)`). `message_index` indexes within the doc;
  all rows share `qmd_path`, `conversation_uuid`, and `document_uuid`.

## Run it

The translate step is an in-process library (the `render_and_index_md`
module, called from `frankweiler-sync`); there is no standalone
`gitlab-translate` binary and no Bazel target for it. Run a sync to
exercise it, and rendered docs land under
`/tmp/gitlab-mirror/rendered_md/gitlab/...`.

To exercise the renderer in isolation, run its tests:

```sh
bazelisk test //frankweiler/backend/etl/providers/gitlab:gitlab_unittests
```
