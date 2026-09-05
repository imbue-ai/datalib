# Data architecture: parse and render

Third sibling to
[`data_architecture_ingestion.md`](data_architecture_ingestion.md) and
its [practices companion](data_architecture_ingestion_practices.md).
Those two cover the **download** stage — how upstream bytes land on
disk and what shape they have at rest. This one covers what happens
next.

Two words, because they are two things and conflating them caused real
confusion while this document was being written:

- **parse** — deserialize a stored payload into the provider's typed
  in-memory representation. Pure, no I/O, lives in
  `render/parse.rs`.
- **render** — turn that representation into the artifacts:
  `<id>.md` for humans and `<id>.grid_rows.json` for the index.

Together they are one pipeline stage, invoked as `datalib-step render
<provider>`. When this doc needs a word for the whole transform it
says **the projection**. What it never says is "the parse step": there
isn't one, and a record that "fails to parse" is a record **render**
could not deserialize — which matters because the fix is always a
re-render and never a re-fetch.

Like its siblings this is aspirational as much as descriptive, and it
tries to say which is which at each point. §4 in particular is a set of
rules we do **not** follow today; the audit and retrofit plan is
[`data_lib_as_a_library/data_handling_practices.md`](data_lib_as_a_library/data_handling_practices.md),
and what the tree actually does today is measured in the
[render audit](data_lib_as_a_library/render_audit_2026_09_03.md).
§3's **U-rules are mostly descriptive** — they name a pattern already
built in four places — while its **P-rules are mixed**, and P1 and P3
are both currently violated. §2's
["Where this is heading"](#where-this-is-heading-the-artifact-becomes-a-database)
is now **half built**: Step 1 shipped — the sidecar tree is gone and
each source writes a doltlite database — and Step 2 (the markdown
itself moving into a table) is still aspiration. That section says
which is which.

## 1. Why there are three of these

`data_architecture_ingestion.md` began as one document. It was split in
`dab2c3d9` (2026-06-15) along a **principles vs practitioner** line —
the load-bearing rules stayed, testing and adding-a-provider and the
open questions moved to the companion — so the core would stay focused
and land under 1000 lines. That split is still right, and this doc does
not repeat it: parse/render's principles and its practices are one file
until it gets big enough to hurt.

The reason for a *third* file was different. Both existing docs are
scoped to ingestion, by title and by their own opening paragraphs — and
render material kept accumulating in them anyway, because there was
nowhere else to put it. The clearest tell was a bullet in the ingestion
doc that opened **"(NOT download/ingest related)"** and then ran five
lines about `GridRow` backpointers.

That homelessness had a cost beyond tidiness: it is why the
data-quality rules in §4 were first written up as a proposal instead of
landing in the architecture docs where they belong.

**Everything that was misfiled now lives here**, with a pointer left
behind at each old location: the backpointers bullet (§3), the
`GridRow.when_ts` policy (§6 — the `datalib-time` crate contract stays
in the ingestion doc, since download stamps its own `fetched_at` with
it), the render cursor and the render-side progress question (§5), the
render-store contract (§2), and the `GridRow` family taxonomy (§3).

| doc | stage | contains |
| --- | --- | --- |
| [`data_architecture_ingestion.md`](data_architecture_ingestion.md) | download | principles, at-rest shape, operational properties |
| [`data_architecture_ingestion_practices.md`](data_architecture_ingestion_practices.md) | download | testing, adding a provider, schema evolution, open questions |
| **this file** | parse + render | the stage contract (and where it is heading), the projection, the parse contract and the unification/fidelity rules, data-quality rules, incrementality, timestamps |
| [`grid_rows.md`](grid_rows.md) / [`edges.md`](edges.md) | — | the tables render writes into |
| [`entity_ids.md`](entity_ids.md) | — | the `uuid` recipe every projection must follow |

## 2. The stage contract

Render's input is `<data_root>/<name>/raw/` and **nothing else** — not
the API, not a file-backed source's `input_path`. The full argument is
[Layering of concerns](data_architecture_ingestion.md#layering-of-concerns-download-is-downstream-agnostic);
the one-line version is that the raw store is the boundary between the
outside world and our copy, so everything downstream of it is
reproducible offline.

The consequence that matters for the rest of this document: **every
parse/render defect is fixable by re-rendering.** That is what makes
§4's rules affordable — we can be strict about correctness here
precisely because being wrong here is cheap to correct.

### The render-store contract

After download, we run transformations for display and indexing —
render to markdown with YAML frontmatter, index the markdown with qmd,
derive `grid_rows` for the UI.

The cross-provider contract is the **render store**: one doltlite
database per source, at
`<data_root>/<name>/rendered_md/indexed_markdown.doltlite_db`, holding
four tables —

  - `markdowns` — one row per rendered document: its `markdown_uuid`
    (the primary key for the `.md`), its `source_fingerprint` (a hash
    of the upstream payload), its `renderer_version`, its `md_path`,
    and the `row_set_hash` over the rows below.
  - `grid_rows` — the document's projected rows.
  - `edges` — its outgoing links.
  - `render_problems` — what render could not do getting there (§4).

The human artifact stays a file: `<id>.md`, with YAML frontmatter,
beside a `blobs/` directory for its attachments.

Every table's schema is a hand-written struct in `datalib_schema` with
`#[derive(PortableTable)]` deriving the DDL, so the same struct defines
what a renderer writes and what the index reads. This *was* a JSON
sidecar file per document; see
["Where this is heading"](#where-this-is-heading-the-artifact-becomes-a-database)
for what the swap bought.

Grid index reads that store — **it never re-parses markdown**. The
markdown is for humans; the store is the machine-readable projection.

This part of the pipeline aspires to the same properties as download:

  - **Monitorable**: same `obs` flags, same progress-bar contract.
  - **Incremental, twice.** Render skips a document whose
    `source_fingerprint` already matches the one in its own store. The
    index then asks each store `dolt_diff` between the commit it last
    consumed (`source_cursors`, in the index database) and that store's
    HEAD — so a steady-state run reads no documents at all, rather than
    reading them and discarding them. The two are not redundant: the
    cursor decides *which documents to read*, the fingerprint decides
    *whether a document that was read needs writing*, and the cold path
    (no cursor, or a cursor the store's history no longer holds) needs
    the second one.
  - **Resumable in the steady state**: a render pass re-run after
    producing N of M documents skips those N via the fingerprint check
    and continues. The rows are committed once per run, so a document's
    rows and its `render_problems` can never disagree about which run
    they came from — but the `.md` files are still plain files, so a
    partial one left by a SIGKILL during a write may not match its
    recorded fingerprint. It is regenerated next run. That is good
    enough for our use case but is not a separately engineered
    property.

Less attention has been paid to render-side observability and to
making partial-progress visible to the user than to the same on
download; this is an area where the implementation trails the
principle.

### Where this is heading: the artifact becomes a database

**Status: Step 1 is built; Step 2 is not.** The two steps looked
similar when this was written and cost very different amounts, which is
why they were separated — and that turned out to be the right split.

**Step 1 — the sidecar becomes a table. Done.** Each source writes its
projected rows into
`<name>/rendered_md/indexed_markdown.doltlite_db`: `grid_rows`,
`markdowns`, `edges` and `render_problems`, already columnar and
already in the shape the unified index stacks. There is no
`<id>.grid_rows.json` anywhere in the tree any more, and
`datalib-index-lib` — the crate that existed only to define that wire
format — is deleted.

Everything the argument below predicted it would buy, it bought:

- The two tree walks are two indexed queries
  (`IndexedMarkdownStore::prior_fingerprints` / `render_versions`).
- The unreadable-sidecar failure class is gone; there is no file to
  fail to parse.
- Render commits once per run, so a document's rows and its
  `render_problems` land together or not at all.
- **Deletion is expressible, and the index now uses it.** The grid
  index keeps a per-source cursor (`source_cursors`, one commit hash
  per source, in the index database) and asks each store
  `dolt_diff` between that commit and its HEAD. A steady-state run
  reads *nothing*: the TNG fixture's second run reads 0 documents
  where the first read 58. And because a diff can name a row that
  **left**, a document a source stops holding is finally deleted from
  the index — re-reading whole stores could never see that, so before
  this a deleted conversation stayed in the grid until someone wiped
  the file.

One thing the list below got wrong, worth recording because it is the
kind of claim this repo warns about believing: the fingerprint compare
did **not** get replaced by the cursor. Both are in the tree and both
earn their place — the cursor decides *which documents to read*, the
fingerprint decides *whether a document that was read needs writing*,
and the cold path (no cursor, or a cursor the store's history no
longer contains) still needs the second one.

What Step 1 did *not* fix is deletion on the render side: nothing yet
removes a document from a source's own store when it disappears
upstream, because render is incremental and "not re-emitted this run"
overwhelmingly means "not looked at". `IndexedMarkdownStore::remove_document`
is the operation; no renderer calls it. That gap used to exist in two
places and now exists in one.

The original argument, which still reads correctly:

The argument is that **the sidecar tree is a hand-rolled version of what
doltlite already does for us one stage earlier.** Download → raw store
gets "what changed since my cursor" from `dolt_diff`. Render → sidecar
tree re-implements the same question with fingerprints and full tree
walks, and most of the render driver's complexity is the cost of that
re-implementation:

- **Two full tree walks per run.** `scan_sidecars` reads every sidecar
  header to rebuild the `markdown_uuid → fingerprint` map, and
  `grid_index` then walks the same tree again. One `dolt_diff` against a
  stored commit answers both.
- **A whole class of failure that a table does not have.** A sidecar can
  be unreadable or unparseable; a row cannot. The audit found both
  responses to that class in the tree and neither is good — the render
  driver silently skips a malformed sidecar (losing its skip), and the
  index loader aborts its entire transaction.
- **No atomicity.** §2 above admits this outright: a SIGKILL mid-write
  leaves a `.md` whose fingerprint does not match its body. A commit
  fixes that by construction.
- **Deletion is expressible.** The two known gaps — orphaned documents
  after a render-param change (§5), and the whole-tree `rm -rf` in
  `discard_tree_from_an_older_renderer` when a renderer re-keys its
  documents — are both "you cannot update a file tree in place when
  identity moves." `DELETE … WHERE` is the answer to both.
- **The step's output version comes free.** A doltlite artifact versions
  as its commit hash, so the runner never content-hashes anything.
  `rendered_tree_version` exists today only to avoid that hashing, and
  [#225](pipeline_dag_architecture.md) is what happens when the
  avoidance is missed: forty seconds of hashing, every run, to version a
  step that had already been skipped.

It also simplifies [§4](#4-data-quality-rules)'s problem sink rather
than complicating it. If render's rows already live in a per-source
doltlite database with a single writer, the problem table belongs in
**that same database** — so a document's rows and the record of what was
dropped or nulled getting them there commit in one transaction, and can
never disagree about which run they came from.

[`data_architecture_ingestion.md`](data_architecture_ingestion.md) says
the `source_fingerprint` compare stays, and this section used to claim
it would be superseded by the cursor. **The ingestion doc was right.**
Both shipped, and they answer different questions — see the correction
above.

Step 1 was close to free, and that is worth saying plainly: **nothing
outside our own code ever read a `.grid_rows.json`.** It was a machine
format with exactly one producer and one consumer, which is what made
replacing it a local change.

**Step 2 — the markdown moves too.** Longer-term, the rendered `.md`
lives in a doltlite table rather than as a file on disk.

This one is genuinely gated, and on something specific: **qmd consumes a
markdown tree.** The semantic index shells out to `@tobilu/qmd` over
`rendered_md/`, so the tree cannot simply stop existing — it would have
to be materialized for the indexer, or qmd's role would have to be taken
over by something that reads from the database (a direction
[`multimodal_retrieval.md`](multimodal_retrieval.md) already proposes for
other reasons). Two smaller things point the same way: attachment blobs
are materialized into each page's `blobs/` directory today, and the
markdown is deliberately human-readable and greppable on disk, which is
a property someone will miss.

The storage argument cuts both ways and should not be oversold.
`multimodal_retrieval.md` §4 measured a real data root and found the
same text stored **five** times. Putting markdown in doltlite makes that
six unless the file tree actually goes away — so the win is conditional
on finishing the move, not on starting it.

**What does not change in either step.** The *contract* is unaffected:
render still emits a human artifact and a separate machine-readable
projection, the projection is still never recovered by parsing the
markdown, and the index still reads the projection. Only the medium
changes. If you find yourself proposing that the grid index parse
markdown because it is now conveniently in the same database, that is
the same mistake ["QMDs are write-only"](/AGENTS.md) warns about, wearing
a new hat.

## 3. The projection

Three shapes, in order:

1. **The stored payload** — JSONB, wire-fidelity, whatever upstream
   sent. Owned by download, described in `schema_raw.rs`.
2. **The typed POD** — the provider's own typed in-memory
   representation, in `render/parse.rs`. Where a shape is shared
   across providers the canonical type lives in a shared crate:
   `chat-common`'s `Normalized*` types for the two chat families,
   `contact-common`'s for contacts. (Earlier drafts of this doc, and
   `data_architecture_ingestion.md`, call this file
   `schema_translate.rs`. No provider has ever had one — the layer
   landed under the two names above.)
3. **The rows** — `GridRow` + `EdgeRow`, handed back through
   `ctx.emit_doc` as a `RenderedMarkdown` and written into the
   source's render store.

(1) → (2) is parse; (2) → (3) plus the markdown is render. Both are
pure given the raw store, and both are the right place for §4's tests.

### Identity and backpointers are first-class in the projection

- **Backpointers and outlinks are first-class** in the projection schema. `GridRow` (one of our indexed representations, not a raw format) carries:
    - `uuid` — the Ship-of-Theseus identity, deterministic from upstream so re-ingest is idempotent.
    - `external_id` — the provider-native primary id (numeric GH/GL id, PR number, …) preserved alongside our UUID so we can round-trip back to the provider's API.
    - `source_url` — the canonical URL on the provider's web UI (e.g. `pull_request.html_url`, GitLab `note.web_url` with `#note_<id>` anchor), populated everywhere we can construct it.
    - `qmd_path` — the path to the rendered `.md`, relative to the data root.
    - Provider-specific cross-references (`notion_page_uuid`, `notion_block_uuid`, `slack_link`, `git_sha`, …) so the UI can link sideways as well as out.

The `uuid` recipe is [`entity_ids.md`](entity_ids.md) and it is not
optional: anything durably keyed on a row — feedback today,
annotations and labels later — rests on it staying stable across a
re-render. A content-hash identity would orphan every such reference
on the first edit.

### Unified where possible, per-provider where not

When several sources are shaped similarly enough (a matter of taste,
but largely driven by schema and UI overlap), they should be massaged
into a **shared canonical schema** so the rest of the pipeline (search,
display, threading, attachments, exports) shares code paths and stays
consistent.

Where unification actually happens **today**: the `GridRow` projection
(the hand-written struct at
[`datalib/backend/schema/src/grid_rows.rs`](../../datalib/backend/schema/src/grid_rows.rs),
whose DDL is derived via `#[derive(PortableTable)]` — see
[`grid_rows.md`](grid_rows.md)).
Every searchable entity from every provider collapses into rows of one
schema with `provider` + `kind` discriminators. The grid backend
reads it with a single query and renders it without knowing which
provider produced any given row.

Unification should **never** happen in the raw store: Slack, Beeper,
Signal, Anthropic, and ChatGPT each have their own raw tables, in their
own doltlite DBs (`slack_messages`, `beeper_messages`, …). Once we
*render*, though, we aspire to share as much as possible — projecting
raw data into unified schemas where appropriate, then sending that
unified data through common code paths for interpretation, rendering,
and indexing.

Examples where schema and data handling should be unified:

  1. **Chat (human)** — Slack, Beeper, Signal. "Messages in
     channels/DMs between humans with attachments and threading."
     Unified at `GridRow`; per-provider raw + render.
  2. **Chat (LLM)** — Claude, ChatGPT, Gemini (planned). Same chat
     shape but with assistant turns, thinking, and tool-use surfaced.
     Unified at `GridRow` via `kind = 'User Input' | 'LLM Response' |
     'LLM Thinking' | 'Tool Call'`.
  3. **Code review threads** — GitHub PR discussions, GitLab MR
     discussions. Threaded inline comments on diffs. Unified at
     `GridRow`; `git_sha` and `external_id` columns are specifically
     there to serve this family.
  4. **Document-comment threads** — Notion. Very similar in shape to
     (3); may eventually share more than just `GridRow` projection.
  5. **Time-series sensor data** — yolink today; Garmin fitness and
     IQ Air air quality planned. Per-device samples over time with a
     small fixed set of value channels. Not yet projected to
     `GridRow`; this family hasn't picked its shared schema yet.

A new provider that fits a family should at minimum project to the
family's `GridRow` shape rather than inventing a new `kind` taxonomy.
A provider that doesn't fit may motivate a new family; opening one
should be deliberate.

### What parse owes the rest of the pipeline

Parse is the smaller half of the projection and the easier one to get
wrong quietly, because its output is consumed only by our own render
code — there is no schema check between them, and a field that comes
out subtly wrong looks exactly like a field that came out right.

Five rules. P1–P3 are contract; P4 and P5 are about where work belongs.

**P1 — Parse is total.** A record that will not deserialize is *that
record's* problem, not the step's. Parse reports what it could not read
and keeps going; only a systemically wrong input (§4's R2 third
category) may fail the step. Concretely this means the parse of a
collection returns the records it got **and** the problems it hit, not
one or the other — see R1's sink.

**P2 — Declare the type you expect, and what happens when it isn't
that.** Every field parse reads has a declared coercion. A value the
declaration does not cover is nulled and reported, never passed through
untyped and never guessed at. "This field is a string, except the three
times upstream sent a list" is a thing to record, not a thing to
paper over.

**P3 — A value with cross-source meaning is parsed by a shared crate,
never by a provider.** This is the rule that makes unification possible
at all, and it is the one most worth stating, because breaking it is
invisible: a provider that hand-rolls its own timestamp parser produces
values that *look* fine and are quietly incomparable with every other
provider's.

If a concept means the same thing across sources, exactly one crate
owns reading and writing it, and every provider calls that crate:

| concept | the one owner |
| --- | --- |
| timestamps | [`datalib-time`](../../datalib/backend/time/src/lib.rs) — `parse_strict`, `parse_with_assumed_utc`, `parse_custom_strftime`, `bump_micros` |
| entity identity | [`datalib-id`](../../datalib/backend/id/src/lib.rs) — the five-component v5 recipe in [`entity_ids.md`](entity_ids.md) |
| the chat shape | `chat-common`'s `Normalized*` types |
| the contact shape | `contact-common`'s |

The list is short because the set of genuinely cross-source concepts is
small. It should grow deliberately: the test for admitting one is
whether two providers disagreeing about it would produce a *wrong
answer* rather than merely an inconsistent-looking one.

This rule is currently violated for timestamps — six of the twelve
render modules parse them with raw `chrono` instead, and that is where
every fabricated-epoch bug in the tree lives. See the
[render audit](data_lib_as_a_library/render_audit_2026_09_03.md) §04.

**P4 — Parse reads the raw store and nothing else.** The stage contract
from §2, restated here because parse is where the temptation appears:
a file-backed provider's `input_path` is *download's* input, not
parse's, and reaching for it makes the projection unreproducible
offline.

**P5 — Parse produces the provider's shape; render produces the shared
one.** The seam matters. `render/parse.rs` deserializes into types that
look like *that provider's* data, with its own vocabulary and its own
optionality. Projecting onto a shared schema is render's job. Keeping
the two apart is what lets a provider's oddities stay in one file
instead of leaking into a type eight providers depend on — and it is
why the unification rules below are all about render.

### Unification and fidelity, and how the tension resolves

Unification is a stated goal of this project: one `grid_rows` schema,
one query behind the grid, `before:` and `after:` meaning the same
thing whether the row came from Slack or GitHub or Notion. Without it
there is no union grid — only twenty per-provider views.

But every unification is a **claim that two things from different
sources are the same kind of thing**, and that claim is lossy at the
edges and sometimes simply wrong. A Slack `ts` and a Notion
`last_edited_time` are both "when," but one is when a human pressed
enter and the other is when anything on a page last moved. Collapsing
them into one sortable column is useful and is not free.

The tension does not resolve by choosing. It resolves by **layering**,
and the layering is already the architecture:

- **The raw store is where fidelity lives.** Wire-faithful, never
  unified (§3 above says this about the raw store, and
  [the ingestion doc](data_architecture_ingestion.md#wire-fidelity-of-the-raw-store)
  argues it in full).
- **The projection is where unification lives.** And because the raw
  store is complete and re-rendering is cheap, *the projection can
  afford to be aggressive* — a unification that turns out to be wrong
  costs a re-render, not a re-fetch. That is the same affordability
  argument §2 makes for §4's rules, applied to schema design.

So the question at any given field is never "unify or preserve." It is
**"can the consumer tell?"** — because the raw store means nothing is
truly lost, and the only real harm is a unified value sitting in a
column a consumer reads as if it were what upstream said.

Five rules follow.

**U1 — Unify the frame, keep the value.** Normalize *representation*
freely; normalize *information* almost never. Timestamps are the worked
example and the one to reason from: we unify the format completely —
ISO-8601, explicit offset, one column, one parser — and refuse to unify
the value, because [§6](#6-timestamps) keeps the source's own UTC
offset rather than normalizing to UTC. Format unification costs
nothing and buys everything; value normalization is where information
goes to die.

**U2 — When a value must be unified to be comparable, derive it
*beside* the faithful one, never over it.** This is the load-bearing
rule, and the tree already follows it in four places:

| faithful column | unified companion | what the unification buys |
| --- | --- | --- |
| `when_ts` — source offset preserved verbatim | `when_ts_utc` + `when_offset`, derived at load by `split_when_ts` | one zone and one width, so lexical order *is* chronological order |
| `upstream_id` + `upstream_entity_kind` + `upstream_scope` — the provider's own identity, byte-exact | `uuid` — our v5 over the five-component recipe | one id space across every provider; stable across re-render |
| `upstream_entity_kind` — the upstream's own word, which "may not [be reworded], because `uuid` derives from it" | `kind` — the grid's display label, which "may be reworded freely" | one Kind column the UI can filter on |
| `blake3` — the whole file | `payload_blake3` — the metadata-excluding digest | "same audio, different tags" becomes a query |

Read that table as one idea stated four times. A consumer that wants
comparison reads the derived column; a consumer that wants to know what
upstream actually said reads the faithful one; and **nobody has to make
that choice on everyone else's behalf.** It is [R6](#r6--findings-are-for-the-consumer-not-fixes-for-the-projection)
expressed as schema rather than as advice.

The failure this prevents is specific: overwriting the faithful column
is irreversible *from the index*, and while the raw store can still
answer, every consumer downstream of the index has silently lost the
distinction and cannot tell that it did.

**U3 — Stamp the recipe beside a derived value when more than one
recipe is possible.** `media` does this and explains why in one
sentence: two payload hashes "are only comparable under one recipe, so
the recipe is stored beside the digest" — hence `payload_scheme`
(`mp3.frames.v1`), and any change to what a recipe excludes bumps its
version. `upstream_scope` is the same move for identity: the exact
string a `uuid` was minted under, kept so the id can be regenerated and
checked.

The rule has a real limit, and knowing it stops the pattern from
becoming ritual: `when_ts_utc` carries no scheme column and should not,
because there is only ever one way to render an instant in UTC. Stamp
the recipe when a *choice* was made, not merely when a derivation
happened.

**U4 — Collapsing a taxonomy is allowed; losing the upstream term is
not.** Render may map a provider's twenty event types onto three
buckets when three is what the layout needs — `chat-common`'s
`ItemKind` (Text / Attachment / System) does exactly that, and it is
the right call for a renderer that has to lay something out. What makes
it safe is that the upstream's own word survives in
`upstream_entity_kind`. Collapse for the consumer; keep the original
for the record.

**U5 — Unify at render, never at parse, and never in the raw store.**
Where the seams are: the raw store keeps each provider's tables
separate and faithful; parse produces the provider's own shape (P5);
render projects onto the shared one. A provider that finds itself
unifying inside `parse.rs` is usually about to teach a shared type
something only it knows.

**When not to unify at all.** A shape that does not fit an existing
family should not be forced into it — that is what the family list
above means by "opening one should be deliberate." The tell is a
provider inventing `kind` values that mean something different from
every other member's, or needing a column no sibling would ever set.
Two families are cheaper than one family with an exception in it.

## 4. Data-quality rules

**Status: not implemented.** These are adopted-in-principle and
unimplemented in fact; see
[`data_lib_as_a_library/data_handling_practices.md`](data_lib_as_a_library/data_handling_practices.md)
for the audit that measures how far off we are and the plan to close
it.

They come from reading the `data-pipeline-builder` skill in
[`imbue-ai/default-workspace-template#534`](https://github.com/imbue-ai/default-workspace-template/pull/534),
which is unusually good on exactly the stage we had documented least.
Several of the formulations below are close to theirs on purpose —
they said it better than our first attempt did.

### R1 — Drop, count, log; never abort, never hide

The headline, and their phrasing. Every problem goes through one sink,
and the sink has a taxonomy rather than a severity:

| what happened | what we do |
| --- | --- |
| unreadable or undeserializable document | drop the record |
| no usable identity | drop the record |
| a field fails its declared coercion | null **that field**, keep the record |
| a value whose type the contract does not cover | null that field — never pass it through untyped |

Every one emits `{source, stage, key_or_path, field, reason, sample}`,
where `sample` is the first 80 characters. Never a count without a
reason, never a reason without a sample. The test of the design is
their sentence for it: **run once, read the log, fix the projection for
every reason it lists, re-render.** If reading the log doesn't tell you
what to change, the sink is wrong.

### R2 — Three failure categories, not two

[`step_protocol.md`](step_protocol.md) currently draws one line —
absent vs malformed — and classifies a record that will not deserialize
as a `data` failure, which fails the step and poisons its entire
downstream subtree, including the `grid_index` fan-in that depends on
*every* source. Right for "the store will not open," wrong for "one of
forty thousand Slack messages has a field we did not expect."

- **absent** — nothing to render: emit nothing, exit 0.
- **malformed but isolated** — this record is bad, the rest are fine:
  drop it, count it (R1), continue, exit 0.
- **malformed systemically** — the input is not what we think it is:
  exit non-zero, `data`, poison the subtree.

### R3 — Any rule that turns a non-null source value into null is a judgment call

Their sentence, kept whole. It follows that every such rule gets a row
in a per-provider table: the rule, the contract line that justifies it,
and **the number of records it affected on the last run**, generated
from R1's counts rather than hand-maintained. If we cannot generate the
count, the rule is not allowed.

This is the render-stage sibling of
[No fabricated timestamps](#no-fabricated-timestamps),
generalized: that section says don't invent a value when upstream is
silent, and this one says when you *discard* one, say so where a human
will see it.

### R4 — A run that drops too much stops

The line between R2's second and third category is a number. Start at
"more than 20% of the records read in this step were dropped": print
the log path, exit non-zero. Deliberately cruder than shape detection,
and worth having precisely because
[Detecting upstream shape drift](data_architecture_ingestion_practices.md#detecting-upstream-shape-drift)
records that we tried the sophisticated version (`endpoint_shapes`),
deleted it, and don't know what we want. Counting drops would have
caught the same class of problem.

### R5 — Verify against the source, not against yesterday's output

Every provider needs at least one assertion comparing a rendered row
back to the raw payload it came from, **sharing no code with the
projection**. Goldens stay — they are good at catching *unintended*
change — but a golden asserts that output matches what it matched last
time, so a field dropped the day a provider landed passes forever.
`AGENTS.md` says the general form in its own voice: a false
test-quality claim is self-concealing.

The failure mode this catches is worth naming in the skill's words: a
projection bug **corrupts attribution while every count still looks
right.** Row counts, provider coverage, uuid uniqueness — all the
things our fixture test already checks — are exactly the signals that
stay green when a field is silently wrong.

### R6 — Findings are for the consumer, not fixes for the projection

The best idea in the skill and the one that most needs saying here,
because we are the ones with a viewer team. Store and emit raw values;
**grouping normalization, axis clamping and null bucketing belong to
the layer that displays the data**, not to the projection. When
profiling turns up that a group-by field has forty spelling variants
that collapse to twelve under case-folding, that is a *finding to
publish*, not a normalization to apply — because applying it destroys
the distinction and the consumer can never get it back.

This is the same instinct as
[Wire-fidelity of the raw store](data_architecture_ingestion.md#wire-fidelity-of-the-raw-store)
moved one stage later: download preserves what upstream said, render
preserves what the projection found, and each layer resists the urge to
pre-chew for the next one.

### R7 — Bound what grows, and say so where it shows

Everything render appends to grows across runs: `rendered_md/`, the
render store, the problem log from R1. Give each a bound or an explicit
"unbounded, because X" in its module doc — and enforce it **inside the
run that writes, never as a separate cleanup chore someone must
remember.** Where a bound exists, the status surface states it, so
limits are never secret.

Partly blocked: doltlite never actually deletes, so a bound on anything
in a `.doltlite_db` reclaims no disk today (see
[Removing a source](data_architecture_ingestion_practices.md#removing-a-source)).
The *stating* half is not blocked.

**A reclaim mechanism is deliberately deferred** (decided 2026-09-04),
and the reasoning has an expiry date attached so it can be revisited
rather than inherited. Derived intermediates are cheap to reclaim by
hand — delete the store and rebuild it, which costs a re-render and
never a re-fetch — and while the schemas are still changing often
enough that intermediates get deleted and rebuilt anyway, a built
mechanism would automate something the churn already does. The
condition to watch is the schema settling: once a derived store starts
living a long time, this needs building. The raw store is a separate
question and is *not* covered by that argument, because it is the copy
we cannot re-fetch. Render's known instance is already
recorded: nothing prunes `rendered_md/`, so a re-render under new
params leaves documents that changed identity beside the new ones, and
they stay in the grid index.

## 5. Incrementality and progress

Render skips what it can, by four mechanisms:

- **What changed since last render** —
  [dolt_diff supersedes per-bucket fingerprints](data_architecture_ingestion.md#dolt_diff-supersedes-per-bucket-fingerprints).
  The per-source cursor stamps the doltlite HEAD into
  `_render_cursor.json`; the next run diffs from it.
- **Whether a document needs re-loading** — `source_cursors` in the
  index database names the store commit `grid_index` last consumed, and
  the `source_fingerprint` on the `markdowns` row settles anything the
  diff surfaces.
- **Forcing a rebake** — `RENDER_VERSION` in each provider's
  `render/render.rs`.
- **When a render param changes** — below.

### The same problem on the render side

Render has its own cursor (`_render_cursor.json`, see
[`render_cursor`](../../datalib/backend/etl/src/render_cursor.rs)) and the
same failure mode: a render param only reaches documents that the
upstream diff happens to surface, so widening `only_render_labels`
renders nothing new and changing `period` re-buckets only the chats that
moved.

The cursor therefore records the render params too, and
`read_for_params` drops it when they differ. Render invalidates
*wholesale* where download reacts proportionally — it's local work over
an on-disk store, so there's no rate limit to ration and the simpler
rule is easier to trust.

**Known gap:** nothing prunes `rendered_md/`. A re-render under new
params writes the new documents but leaves any that changed identity
(notably a different `period` bucketing) beside them as orphans, and
they stay in the grid index. Fixing that needs a pruning pass that knows
the full expected document set.

### Render-side partial-progress visibility

**Desired principle**: a long-running render pass — first run after
a big initial download, or a `RENDER_VERSION` bump that invalidates
every document — must be as monitorable and as stoppable-resumable as
download is. The user sees "rendered 12,347 / 89,201" with an ETA;
^C-then-rerun resumes from 12,347 not 0.

**Open**: the fingerprint-skip *does* give resumability in the steady
state (see §2), but render-side progress reporting is less developed
than download-side. Worth measuring.

## 6. Timestamps

If [object identity](data_architecture_ingestion.md#object-identity-ship-of-theseus-on-uuids) is "UUIDs give global object identity," this is its temporal sibling: **timestamps give global temporal ordering** across every provider that has a time-shape to its data. That global ordering is what makes the UI's union grid time-sortable, what makes `before:` / `after:` queries mean the same thing across Slack and GitHub and Notion, and what lets a sync delta be "what happened in the last week" instead of "what happened to be at the top of each provider's result list."

The principle: **every event-shaped `GridRow` carries an ISO-8601 timestamp with explicit offset.** Concretely, in `GridRow.when_ts`:

- **Real upstream timestamp when one exists.** A Slack message's `ts`, a GitHub PR's `created_at`, a Notion page's `last_edited_time`. Preserved with the explicit offset upstream gave us (typically `+00:00` for APIs that hand back UTC).
- **Microsecond-bump for synthesized timestamps.** Blocks and sub-items that lack their own timestamp (chat blocks within a message, ChatGPT messages within a conversation that only has a create_time) get a synthesized one by bumping microseconds off the parent's stamp. This keeps within-parent order stable across re-runs and guarantees no collision with real stamps (real timestamps don't carry per-row µs precision from upstream).
- **Strict ISO-8601 with offset, not bare `Z` or naive.** A naive timestamp can't be globally sorted alongside a `+02:00` one without a hidden timezone assumption.

The crate that enforces all of this —
`IsoOffsetTimestamp::now_local()`, `parse_strict`,
`parse_with_assumed_utc`, `bump_micros` — is shared with download and
documented in
[the ingestion doc](data_architecture_ingestion.md#single-source-of-truth-datalib-time).

### No fabricated timestamps
A logical corollary of the broader "[don't make up data](data_architecture_ingestion.md#wire-fidelity-of-the-raw-store)" principle, called out here because timestamps are the easiest place to accidentally violate it:

- When upstream gives us no timestamp and we can't pick one up from a parent (no `bump_micros` source), `when_ts` is **null**. Not "epoch," not "now," not "midnight UTC of the row's date."
- When upstream's timestamp string is naive and we haven't audited that feed, parsing returns an error — surfaced as a warning in the per-run summary, not silently rescued.
- Fallback paths that synthesize a value when upstream is silent are anti-patterns even when they "look plausible." They mask incompleteness in ways the consumer can't tell apart from real data.

### Entities without a time-shape
Some upstream object types genuinely don't have a meaningful timestamp:

- **Contacts (vCards).** A person doesn't have a creation event; they exist. The vCard's `REV` field is sometimes set, but most contacts lack one.
- **Perseus texts and other immutable corpora.** The corpus is upstream-frozen; per-section "timestamps" would be nonsense.
- **Workspace/account metadata** (Slack `team`, GitHub `org`): arguably has a creation date, but it isn't shown in any time-ordered view.

For these `when_ts` is **null** and the consumer query filters them out of time-ordered views — the principle is "**event-shaped** rows get real timestamps," not "every row everywhere." A new provider should decide explicitly which of its row types are event-shaped and document the source of `when_ts` for each.

## See also

- [`data_lib_as_a_library/data_handling_practices.md`](data_lib_as_a_library/data_handling_practices.md)
  — the audit and retrofit plan for §4.
- [`step_protocol.md`](step_protocol.md) — where R2's third category
  has to be written down to mean anything.
- [`grid_rows.md`](grid_rows.md), [`edges.md`](edges.md),
  [`entity_ids.md`](entity_ids.md) — the tables and the id recipe.
