# Render: a document goes only when its rows did

**Status: steps 0 (#1066), 1 (#1073) and 2 are built; the `DocDraft`
reshape (§3 and step 3) is dropped; step 4 is open; step 5 is built
another way (#1087).** The render half of the 2026-10-05 audit
([`audits/2026-10-05_loose_ends.md`](../audits/2026-10-05_loose_ends.md)
§3.2, §4, and the short version's item 4), checked against the tree at
`6188afdd7`. Line numbers are at that commit.

## 1. The problem

[`data_architecture_parse_and_render.md`](../data_architecture_parse_and_render.md)
§5 states the rule: "render never infers a deletion from a document's
absence", because absence has three meanings: gone, not looked at, and
could not look. The sweep keeps half of it. A bucket the run never
looked at keeps its documents. But a bucket the run *declared with
nothing* loses them, and renderers declare a bucket with nothing
whenever they built nothing for it, whatever the reason:

- **The rows left upstream.** Correct: the diff says so.
- **The build failed.** A record that would not parse, a table that
  would not load, a field the decoder rejected. The document is deleted
  and the step reports success.

The renderers do this five different ways: a scan that pre-declares
every stale key empty (beeper, chatgpt, claude, email, signal, slack),
chat-common's `changed_chats`, where a stale uuid with no built chat is
`gone` (apple_messages, claude_code, codex, google_takeout,
sms_backup_restore, and whatsapp by hand), `RawRange::narrow` over the
loaded documents (facebook, linkedin, contacts, calendar), a
pre-declared `parsed.render` (github, gitlab, notion), and the
timeseries pages. Only pdf keeps a failed conversion's page
(`buckets_of(looked_at − failed, …)`).

So one bad record deletes its whole document for claude, whatsapp and
contacts, and, through the shared loaders' silent skip, for the four
chat-common sources above. On a full walk (a version or params bump)
every document of a table that would not load went, until #1066 fixed
that one entry point for facebook and linkedin.

## 2. The rule

**A bucket is gone only when the diff says its rows left.** Everything
else a run can say about a bucket it looked at is one of:

| what the run saw | what happens to its documents |
|---|---|
| rendered | replaced by what it emitted |
| its rows left (`dolt_diff` `removed`) | deleted |
| the renderer chose to emit nothing (email's label filter) | deleted, because the renderer says so: `excluded` |
| its build failed | **kept**, and a `problems` row says why |
| no draft and none of the above | **kept**, and a `problems` row: "this bucket produced no document" |

The full walk's whole-store sweep skips the documents of failed
buckets too.

This is the local-source rule of the sync-state plan turned to render:
there, a unit read cleanly licenses deletion inside it; here, a row the
diff reports removed does.

## 3. The shape

One way to emit, one way to finish, in `datalib_etl_render`:

```rust
pub enum Table { Absent, Rows { rows: Vec<(String, Value)>, unparsed: Vec<Unparsed> } } // no Default
pub struct DocDraft { markdown_uuid, bucket, rel_path, sections, assets, rows, edges, contacts, problems }
pub struct RunEnd { head, looked_at, excluded, failed: Vec<(String, String)>, read: ReadScope }

impl RenderCtx<'_> {
    fn with_raw<T>(&self, raw: &Path, f) -> Result<Option<T>>; // one pinned open, closed on both paths
    fn load(&self, r: &RawReader, table: &'static str) -> Result<Table>; // Absent only for a missing table
    fn emit(&self, draft: DocDraft, inputs: &[Input]) -> Result<()>;
    fn finish(self, end: RunEnd) -> Result<()>;
}
```

The framework, not the renderer, then:

- writes the `.md` and its assets, and stamps `source_id`, the bucket
  key and the render version;
- decides gone from the diff (`changed_keys` keeps `diff_type`, which
  it drops today) and keeps a failed or silent bucket's documents;
- turns `Table::Rows.unparsed` into parse problems, and replaces parse
  problems only for the entities `read` names, by stage;
- maps a fetch problem to its documents through `render_inputs`
  (attachment edges are already declared inputs), so a failed download
  reaches the document's banner for every source;
- derives render params from the render config, so a knob that changes
  the output re-renders.

A renderer keeps what is its own: how a row becomes a document.

## 4. What does not fit, and stays local

- perseus reads a file tree, never a store, and never consumes; it
  keeps its own ending.
- whatsapp and apple_messages read mirrored SQLite tables with no
  `payload` column: `ctx.load` does not apply, `with_raw` and `finish`
  do.
- The timeseries pages' `skip_if_current` finishes without emitting: a
  "nothing to do" end.
- The diff group's collecting sink and the driver's storage report stay
  outside `emit`.

## 5. Bugs this closes, and the ones it does not

| # | Finding | Closed by |
|---|---|---|
| 1 | facebook and linkedin read a load error as an absent table | step 0 (#1066), then `Table` |
| 2 | claude, whatsapp, contacts, signal, beeper drop a record with only a log line, and the stale bucket is swept | §2 and `failed` |
| 3 | the shared loaders skip an undeserializable row silently; through `changed_chats` its bucket is swept | `ctx.load` and §2 |
| 4 | codex fails its whole render on one bad line | `ctx.load` |
| 5 | attachment problems reach no document for email, facebook, sms, beeper, signal, notion | fetch problems through `render_inputs` |
| 6 | beeper's `period` and claude's `max_project_doc_bytes` are not render params | params from the config |
| 7 | whatsapp opens the raw store twice per pass | `with_raw` |
| 8 | chatgpt and slack report `Whole` scopes on narrowed runs; parse problems are swept with no stage filter | `finish` |

Not closed by the shape, fixed in the step that moves the crate: linkedin
posts keyed by row position (`posts.rs:133-151`), email_render's
`"references"` column (`parse.rs:452`), signal's `decode_chat_item`
turning a decode failure into an empty item, garmin's
`unwrap_or(Value::Null)`, chat-common's blob materialize failure. A lint
over `*_render/src` for `warn!` beside a `continue` or a skip keeps the
class out.

## 6. Order of work

Each step lands on its own, with a test watched failing first for every
bug it closes.

- **Step 0.** facebook and linkedin: a table that will not load fails
  the render (#1066).
- **Step 1. The rule, in the driver.** `changed_keys` keeps `diff_type`;
  `finish` takes `RunEnd`; a looked-at bucket with no draft is deleted
  only when its rows left, kept with a row otherwise; the full walk
  skips failed buckets. The existing `finish(buckets, head)` stays as a
  shim over it so no crate moves yet. Test: one renderer whose build
  fails for one bucket keeps that bucket's document, on a narrowed run
  and on a full walk.
  *Built as:* no `RunEnd` in the renderers yet; `RenderCtx` gained
  `exclude_bucket` and `fail_bucket` beside `declare_bucket`, and the
  rule covers a bucket declared with **no rows** that emitted nothing
  (one declared with rows and no document is still swept, as before),
  and a removed row is not the only evidence: every row it was built
  from now read by a bucket new this run is a re-key, and gone too.
  Moved: email's label filter to `exclude_bucket`, pdf's failed
  conversion to `fail_bucket`. The reference is
  `data_architecture_parse_and_render.md` §"The sweep".
- **Step 2. The chat-common sources** (fourteen crates): `changed_chats`
  stops calling an unmapped uuid gone; `ctx.load` and `emit` replace the
  shared loader and the hand-written `.md`. Closes 2, 3, 4 for them.
  *Built as:* no reshape. Step 1 already keeps a bucket `changed_chats`
  calls gone without evidence. The shared loaders fail on a row they
  cannot read back instead of skipping it; claude reports a conversation
  that will not build (`report_document_failed` + `fail_bucket`);
  contacts' skip was dead code and went. Signal's and beeper's skips are
  internal-consistency checks and whatsapp's drop orphan rows, so they
  stay. Not done: a record that fails the first time it is seen has no
  document, and only its renderer knows which rows it tried, so nothing
  general reports it; codex still fails its render on one bad line.
- **Step 3. The rest by shape:** contact-common and calendar-common;
  the forges; notion and pdf; the timeseries pages. Then the shim goes.
- **Step 4. Problems:** fetch problems through `render_inputs`; parse
  problems by stage and by entity; the lint.
- **Step 5. Params from the config.**
  *Built as:* no derivation. Beeper's `period`, claude's
  `max_project_doc_bytes` and perseus's `alignment_pairs` joined their
  processors' `render_params`, and `every_render_knob_is_a_render_param`
  (`datalib_step/src/dispatch.rs`) plans every render config with each
  of its keys changed and fails when the params do not move, so a new
  knob cannot be missed. Deriving them from the serialized config would
  have changed the stored params of email, signal, codex and claude_code,
  re-rendering each once for nothing, and read a reordered label list as
  a change.

## 7. What this costs a person

More `problems` rows, none of them new failures: a record that
silently vanished or silently deleted its document becomes a row on its
document or source. A standing row commits nothing (#1052). A document
that would have been deleted by a failed build stays, possibly stale,
with a banner saying why.
