# GitLab Translate

The render step for a `gitlab` source reads the raw store `gitlab-ingest`
writes and emits **one markdown document per merge request**, plus that
document's `grid_rows`. The document and its rows are the shared forge
shape GitHub uses, described in
[`../github_render/TRANSLATE.md`](../github_render/TRANSLATE.md); this
crate reads the raw tables into it (`src/render/parse.rs`) and sets the
GitLab `ForgeProfile` (`src/render/mod.rs`). What differs:

```
<root>/<source_id>/render_markdown/<namespace>/<project>/mr-<iid>/index.md
```

- **Front matter** names the MR `project` and `mr_iid`, and its refs
  `source_branch` and `target_branch`.
- **Title** is `{title} (!{iid})`.
- **No Reviews section.** Each discussion is unrolled into its notes.
  A note with a diff position (`new_path`, else `old_path`) in a
  discussion that is not an `individual_note` goes under **Inline
  comments**, grouped by `(path, line)`; every other note is **General
  discussion**. Every note after a discussion's first is shown as a
  reply to it. Permalinks are `{mr.web_url}#note_{id}`.
- **`system: true` notes are dropped** — label changes, draft toggles
  and the like are GitLab's audit log, not conversation.
- **Rows**: the MR's own row is `kind = "GitLab MR"`; a note's is
  `"GitLab Discussion Note"` or `"GitLab Inline Note"`.

## Run it

Render runs as a source's `render_markdown` step (`datalib-step`); there
is no standalone binary. To exercise it without a sync:

```sh
bazelisk test //datalib/backend/etl/providers/gitlab_render:gitlab_render_unittests \
    //datalib/backend/etl/providers/gitlab:gitlab_tests
```
