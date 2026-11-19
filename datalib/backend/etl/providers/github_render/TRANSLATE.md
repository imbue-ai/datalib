# GitHub Translate

The render step for a `github` source reads the raw store its `ingest` step
writes and emits **one markdown document per pull request**, plus that
document's `grid_rows`. The document and its rows are built by
`datalib/backend/etl/forge-render-common/`, which GitLab shares; this
crate reads the raw tables into it (`src/render/parse.rs`), mints the ids
(`src/render/ids.rs`) and sets the GitHub `ForgeProfile`
(`src/render/mod.rs`).

```
<root>/<source_id>/render_markdown/indexed_markdown.doltlite_db
                            # the PR's document, kept by its id, and its
                            # rows: one for the PR + one per comment
```

## Markdown layout

1. **Front matter** — `provider`, `repo`, `pr_number`, `title`, `state`,
   `author`, `created_at`, `updated_at`, `merged_at`, `head_sha`,
   `base_sha`, `head_ref`, `base_ref`.
2. **Title** — `{title} (#{num})` as the page title, with a ↗ link to
   the PR, then a one-line
   `*{state}* — @{author} — \`{head_ref}\` → \`{base_ref}\``.
3. **Description** — the PR body as-is, or `*(no description)*`.
4. **Reviews** — one block per review, oldest first. The header carries
   the reviewer, the review state (`COMMENTED`, `APPROVED`, …) and a
   `[link]` to `#pullrequestreview-N`. A review with no body is its
   header alone.
5. **General discussion** — issue comments, oldest first, linking to
   `#issuecomment-N`.
6. **Inline comments** — review comments grouped under a
   `` ### `path:line` `` heading, chronological within each thread. A
   reply sits under its thread's first comment's anchor, so a thread
   stays together even if the diff has moved. Each links to
   `#discussion_rN`.

An empty section says so (`*(no reviews)*` and the like). The
description and each comment open with the one message header every
renderer writes (`datalib_etl_render::message::MessageHeader`): who
wrote it, a chip where GitHub names the account and `@user` otherwise,
the review's state and `reply` as tags, the time, and `↗` to it on
GitHub; then its body as its author wrote it.

## Rows

- The PR's own row comes first (`kind = "GitHub PR"`, `is_document`),
  its uuid minted by `datalib_id` from (repo, PR number) under the
  source and stamped with the PR's `created_at`.
- Then one row per comment, in the document's order (Reviews → General
  → Inline by `(path, line)`), with `message_index` counting from 0.

Every row shares the PR's `conversation_uuid` and
`markdown_uuid`. `upstream_id` is (repo, PR number) for the PR's row and
(repo, comment or review id) for the rest. A row that will not validate
is dropped and recorded as a problem rather than failing the render.

**Where a row sits** (`grid_rows.md` §"`parent_uuid`…"). The repository
is a place, keyed by the id GitHub gives it on the PR's `base.repo`,
which a rename keeps (its full name where the payload has no id). The
PR is read together inside it, and its own row is the container's: the
description is that row's text, so it has no row of its own. Every
comment sits in the PR. A review comment's `in_reply_to_id` is a
`ReplyTo` edge to the comment it answers, not a parent.

## Run it

Render runs as a source's `render_markdown` step (`datalib-step`); there
is no standalone binary. To exercise it without a sync:

```sh
bazelisk test //datalib/backend/etl/providers/github_render:github_render_unittests \
    //datalib/backend/etl/providers/github:github_tests
```

`github_tests` holds the incremental-render and playback round-trip
tests.
