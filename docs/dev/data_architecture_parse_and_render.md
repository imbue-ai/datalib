# Data architecture: parse and render

Third sibling to
[`data_architecture_ingestion.md`](data_architecture_ingestion.md) and
its [practices companion](data_architecture_ingestion_practices.md).
Those two cover the **download** stage — how upstream bytes land on
disk and what shape they have at rest. This one covers what happens
next.

Two words, because they are two things and conflating them causes real
confusion:

- **parse** — deserialize a stored payload into the provider's typed
  in-memory representation. Pure, no I/O, lives in
  `render/parse.rs`.
- **render** — turn that representation into the artifacts: `<id>.md`
  for humans and the rows of the render store for the index.

Together they are one pipeline stage, the `render_markdown` step of a
source's group. When this doc needs a word for the whole transform it
says **the projection**. What it never says is "the parse step": there
isn't one, and a record that "fails to parse" is a record **render**
could not deserialize — which matters because the fix is always a
re-render and never a re-fetch.

Like its siblings this is aspirational as much as descriptive, and it
tries to say which is which at each point. §4 in particular is a set of
rules we do **not** follow today; the audit and retrofit plan is
[`plans/data_lib_as_a_library/data_handling_practices.md`](plans/data_lib_as_a_library/data_handling_practices.md).
§3's **U-rules are descriptive** — they name a pattern already built in
four places — while its **P-rules are mixed**, and P1 and P3 are both
violated today. §5 is descriptive throughout: it is how the render step
driver works.

## 1. Why there are three of these

The two ingestion docs are scoped to download, by title and by their own
opening paragraphs. Render material has nowhere to live in them, so it
lives here.

| doc | stage | contains |
| --- | --- | --- |
| [`data_architecture_ingestion.md`](data_architecture_ingestion.md) | download | principles, at-rest shape, operational properties |
| [`data_architecture_ingestion_practices.md`](data_architecture_ingestion_practices.md) | download | testing, adding a provider, schema evolution, open questions |
| **this file** | parse + render | the stage contract, the projection, the parse contract and the unification/fidelity rules, data-quality rules, incrementality and deletion, timestamps |
| [`grid_rows.md`](grid_rows.md) / [`edges.md`](edges.md) | — | the tables render writes into |
| [`entity_ids.md`](entity_ids.md) | — | the `uuid` recipe every projection must follow |

The `datalib-time` crate contract stays in the ingestion doc, since
download stamps its own `fetched_at_utc` with it; §6 here covers only
what render does with a record's own timestamps.

## 2. The stage contract

Render's input is `<data_root>/<name>/ingest/` and **nothing else** — not
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
`<data_root>/<name>/render_markdown/indexed_markdown.doltlite_db`, holding

  - `markdowns` — one row per rendered document: its `markdown_uuid`
    (the primary key for the `.md`), its `renderer_version`, its
    `md_path` and the `bucket_key` it was rendered from. Nothing on it
    is stamped per run — a re-render of an unchanged document writes
    an identical row.
  - `grid_rows` — the document's projected rows.
  - `edges` — its outgoing links.
  - `problems` — what render could not do getting there (§4).
  - `render_inputs` and `render_cursor` — what each bucket was rendered
    from, and how far into the raw store the last run got (§5).
  - `source_measurements` — the storage report's samples
    ([`grid_rows.md`](grid_rows.md)).

The human artifact stays a file: `<id>.md`, with YAML frontmatter,
beside a `blobs/` directory for its attachments.

Every table's schema is a hand-written struct in `datalib_schema` with
`#[derive(PortableTable)]` deriving the DDL, so the same struct defines
what a renderer writes and what the index reads.

Grid index reads that store — **it never re-parses markdown**. The
markdown is for humans; the store is the machine-readable projection.

This part of the pipeline aspires to the same properties as download:

  - **Monitorable**: same `obs` flags, same progress-bar contract.
  - **Incremental, by diff alone.** Render asks the raw store what
    changed since the commit its cursor names and renders only the
    buckets that read those rows; the index asks the render store the
    same question about its own commits. §5 is the whole mechanism.
  - **Resumable at every checkpoint**: render commits the store after
    every batch of documents, and each commit leaves a store a consumer
    can read. A run killed after N of M documents has N of them
    committed; the next run diffs from the cursor it last *sealed*, so
    it re-renders the range once more — to identical rows, which cost
    nothing — and continues. The `.md` files are plain files, so a
    partial one left by a SIGKILL mid-write is rewritten next run.

Less attention has been paid to render-side observability and to
making partial progress visible to the user than to the same on
download; this is an area where the implementation trails the
principle.

### Why the projection is a database and not a file tree

The render store is doltlite for the same reason the raw store is: the
question "what changed since my cursor?" is one doltlite answers, and a
file tree of sidecars would have to re-implement it with fingerprints
and full walks. Four properties follow, and each is load-bearing
somewhere downstream:

- **No tree walks.** What a run needs to know about the render store it
  reads through indexed queries; what it needs to know about the raw
  store it reads through `dolt_diff`.
- **A row cannot be unreadable.** A sidecar file could be malformed, and
  the two responses to that (skip it silently; abort the whole load)
  were both bad. There is no file to fail to parse.
- **A document lands whole or not at all.** Its rows, edges, markdown
  and problems are written inside one SQL transaction, so a commit
  landing between two documents — a checkpoint, a Ctrl-C, a rescue —
  never publishes a fraction of one.
- **Deletion is expressible.** A `dolt_diff` can name a row that
  *left*, which is how a document a source stops holding reaches the
  grid index as a deletion. Reading whole stores could never see that.
- **The step's output version is free.** A doltlite artifact versions
  as its commit hash, so the render step reports its store's HEAD and
  the runner never content-hashes the tree
  ([`dag/README.md`](../../datalib/backend/dag/README.md) says what an
  unreported version costs).

It also simplifies [§4](#4-data-quality-rules)'s problem sink. Render's
rows and the record of what was dropped or nulled getting them there
live in one database with one writer, so they commit in one transaction
and can never disagree about which run they came from. A document whose
every row was dropped is stored as no document at all — no `markdowns`
row, no `.md` file — and its problem rows are the whole record of it.

### What is still a file: the markdown

The rendered `.md` lives on disk rather than in a table, and moving it
is gated on something specific: **qmd consumes a markdown tree.** The
semantic index shells out to `@tobilu/qmd` over `render_markdown/`, so
the tree cannot simply stop existing — it would have to be materialized
for the indexer, or qmd's role taken over by something that reads from
the database (a direction
[`multimodal_retrieval.md`](plans/multimodal_retrieval.md) already
proposes for other reasons). Two smaller things point the same way:
attachment blobs are materialized into each page's `blobs/` directory,
and the markdown is deliberately human-readable and greppable on disk,
which is a property someone will miss.

The storage argument cuts both ways and should not be oversold.
`plans/multimodal_retrieval.md` §4 measured a real data root and found
the same text stored **five** times. Putting markdown in doltlite makes
that six unless the file tree actually goes away — so the win is
conditional on finishing the move, not on starting it.

Whatever the medium, the *contract* holds: render emits a human
artifact and a separate machine-readable projection, the projection is
never recovered by parsing the markdown, and the index reads the
projection. If you find yourself proposing that the grid index parse
markdown because it is conveniently in the same database, that is the
mistake ["QMDs are write-only"](/AGENTS.md) warns about, wearing a new
hat.

## 3. The projection

Three shapes, in order:

1. **The stored payload** — JSONB, wire-fidelity, whatever upstream
   sent. Owned by download, described in `schema_raw.rs`.
2. **The typed POD** — the provider's own typed in-memory
   representation, in `render/parse.rs`. Where a shape is shared
   across providers the canonical type lives in a shared crate:
   `chat-common`'s `Normalized*` types for the two chat families,
   `contact-common`'s for contacts.
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
    - Provider-specific cross-references (`notion_page_uuid`, `notion_block_uuid`, `git_sha`, …) so the UI can link sideways as well as out.

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
Signal, Claude, and ChatGPT each have their own raw tables, in their own
doltlite DBs — and because each store is its own file, the tables need
no provider prefix to stay apart. Slack's messages are in `messages`,
Beeper's in `events`, Signal's in `chat_items`. The full list is the
`schema_inventory` golden. Once we
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
     IQ Air air quality planned ([`plans/airvisual.md`](plans/airvisual.md)
     is the investigation). Per-device samples over time with a
     small fixed set of value channels. yolink projects one `Sensor
     Timeseries` row for its page plus a `Sensor Device` row per
     device (`yolink_render/src/render/render.rs::build_grid_rows`);
     the family's shared raw schema and render are still per-provider
     copies, and the plan says when to extract them.

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

This rule is violated for timestamps: six of the 22 render crates
(`chatgpt`, `facebook`, `google_takeout`, `linkedin`, `perseus`,
`sms_backup_restore`) still reach for `chrono` directly rather than
`datalib-time`, and that is where every fabricated-epoch bug in the
tree has lived. The retrofit is in
[`data_handling_practices.md`](plans/data_lib_as_a_library/data_handling_practices.md).

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
| `created_at` — source offset preserved verbatim | `created_at_utc` + `created_offset`, derived at load by `split_record_stamp` | one zone and one width, so lexical order *is* chronological order |
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
becoming ritual: `created_at_utc` carries no scheme column and should not,
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

**Status: R1's sink is built for every stage — fetch, parse, render
and grid row — and on screen, though most providers do not yet route
a per-record fetch failure into it; R2's middle category is what the
sink makes possible and is followed where the sink is wired; R3–R7 are
adopted in principle and not built.** The sink is the `problems` table
(`datalib_problems`), one row per problem per record: a fetch problem
starts in the source's raw store, everything else in its render
store, and the render store carries the raw store's rows forward so
one store holds the source's whole list. From there it is copied into
the unified index, counted on the Manage row and shown on the
document. How it is wired at each stage:

| stage | how a problem gets in | swept by |
| --- | --- | --- |
| fetch, one record | `record_object_attempt`'s failure arm (`record_object_error`), in the raw store, `Reason::FetchFailed` | the next attempt on that record, success or failure |
| fetch, a configured entry upstream does not have | `download_problems::report`, in the raw store, keyed `config:<setting>:<value>` | the next run's report, which replaces the last one's whole |
| fetch, carried into render | the render step reads the raw store's rows at the commit it rendered from and replaces its own fetch-stage rows with them, re-minted under the source's id (`render.rs`, `replace_stage_problems`) | every render |
| grid row | `GridRowBuilder::build_or_record` | the document, when re-rendered |
| parse, in a document | `NormalizedChatItem::problems` (`own_stamp_ms` for a stamp) | the document |
| parse, no document yet | `RenderCtx::report_unparsed` with a `ReadScope` | the tables the parse read whole |
| render, whole document | `RenderCtx::report_document_failed` | the document, when it next renders |

The fetch-stage rows are the download side's; the rule for writing
one is in [`data_architecture_ingestion.md` §"Error handling"](data_architecture_ingestion.md#error-handling),
and how they travel is [`etl/README.md` §"Problems flow downstream with the data"](/datalib/backend/etl/README.md#problems-flow-downstream-with-the-data).

The design and what is still open are
[`plans/problem_visibility.md`](plans/problem_visibility.md); the
audit that produced it is
[`plans/data_lib_as_a_library/data_handling_practices.md`](plans/data_lib_as_a_library/data_handling_practices.md).

They come from reading the `data-pipeline-builder` skill in
[`imbue-ai/default-workspace-template#534`](https://github.com/imbue-ai/default-workspace-template/pull/534),
which is unusually good on exactly the stage we had documented least.
Several of the formulations below are close to theirs on purpose —
they said it better than our first attempt did.

### R1 — Drop, count, log; never abort, never hide

The headline, and their phrasing. Every problem goes through one sink,
and the sink has a taxonomy first — what was lost — and a severity
second, derived from it unless the writer says otherwise:

| what happened | what we do |
| --- | --- |
| unreadable or undeserializable document | drop the record |
| no usable identity | drop the record |
| a field fails its declared coercion | null **that field**, keep the record |
| a value whose type the contract does not cover | null that field — never pass it through untyped |

`GridRowBuilder::build_or_record` is this sink for the grid-row stage:
a `created_at` that will not parse is nulled and the row kept, a row with
no identity is dropped, and each lands as a `problems` row with a
deterministic id, swept when its document is next rendered.

Every one emits `{source, stage, key_or_path, field, reason, sample}`,
where `sample` is the first 80 characters. Never a count without a
reason, never a reason without a sample. The test of the design is
their sentence for it: **run once, read the log, fix the projection for
every reason it lists, re-render.** If reading the log doesn't tell you
what to change, the sink is wrong.

### R2 — Three failure categories, not two

[`step_protocol.md`](step_protocol.md) draws one line — absent vs
malformed — and classifies a record that will not deserialize as a
`data` failure, which fails the step and poisons its entire downstream
subtree, including the `grid_index` fan-in that depends on *every*
source. Right for "the store will not open," wrong for "one of forty
thousand Slack messages has a field we did not expect."

- **absent** — nothing to render: emit nothing, exit 0.
- **malformed but isolated** — this record is bad, the rest are fine:
  drop it, count it (R1), continue, exit 0. A provider's parse
  collects the rows it could not read as `Unparsed` instead of
  `continue`ing past them, and its processor reports them; a
  conversion that fails on one document reports that document.
- **malformed systemically** — the input is not what we think it is:
  exit non-zero, `data`, poison the subtree. The threshold between
  this and the previous category is R4, which is not built.

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

Everything render appends to grows across runs: `render_markdown/`, the
render store, the problem log from R1. Give each a bound or an explicit
"unbounded, because X" in its module doc — and enforce it **inside the
run that writes, never as a separate cleanup chore someone must
remember.** Where a bound exists, the status surface states it, so
limits are never secret.

Partly blocked: doltlite never actually deletes, so a bound on anything
in a `.doltlite_db` reclaims no disk today (see
[Removing a source](data_architecture_ingestion_practices.md#removing-a-source)).
The *stating* half is not blocked.

**A reclaim mechanism is deliberately deferred**, and the reasoning has
a condition attached so it can be revisited rather than inherited.
Derived intermediates are cheap to reclaim by hand — delete the store
and rebuild it, which costs a re-render and never a re-fetch — and
while the schemas are still changing often enough that intermediates
get deleted and rebuilt anyway, a built mechanism would automate
something the churn already does. The condition to watch is the schema
settling: once a derived store starts living a long time, this needs
building. The raw store is a separate question and is *not* covered by
that argument, because it is the copy we cannot re-fetch. Documents a
source no longer produces are not part of the problem: §5's sweep
removes their rows and their `.md` files in the run that stops
producing them. What stays is the store's *history* of them, which is
doltlite's to reclaim.

## 5. Incrementality and deletion

Incremental render answers one question: *given the raw rows that
changed since the last render, which documents need rendering again?*
Deletion is the other half of the same question — a document whose
inputs are gone re-renders to nothing. Both are answered from one
record, and the record is why the render step driver never infers a
deletion from a document's *absence*: absence has three meanings
(gone, not looked at, could not look), and a mechanism that reads it
as "gone" has deleted a live source here more than once. The rule the
whole section rests on:

> **`dolt_diff` between two commits of our own raw store reports every
> row that left. Render never infers a deletion from anything else.**

The ingestion doc's
[dolt_diff supersedes per-bucket fingerprints](data_architecture_ingestion.md#dolt_diff-supersedes-per-bucket-fingerprints)
is the download-side half of the same idea.

### What is recorded

Two tables in the render store, both written by the driver
(`datalib_step/src/render.rs`):

- **`render_cursor`** — one row: the raw store's commit the last run
  consumed, and the render params (a period, a label filter — whatever
  each processor declares through `RenderProcessor::render_params`)
  the documents were rendered with. The driver writes it in the same
  transaction as the run's last work, so it can never claim a range the
  store's rows do not reflect, and rewrites it only when it moves.
- **`render_inputs`** — `(bucket_key, input_table, input_id)`, one row
  per raw row a bucket's render **asked for, found or not**. A thread
  rendered while its author's `users` row had not been fetched yet
  records `(users, U123)` anyway, so the row's arrival names the
  thread. Keyed on the bucket rather than the document because a
  periodizing renderer (slack, signal, beeper) turns one conversation
  into several documents, and the inputs belong to the conversation.
  `markdowns.bucket_key` is the link back: the bucket each document
  came from.

A **bucket** is whatever a provider loads and renders as a unit — a
conversation, a thread, a PR, a page; perseus renders its whole file
tree as one. A provider declares each bucket it rendered, once, through
`RenderCtx::declare_bucket(bucket_key, inputs)`
(`etl/render/src/processor.rs`), and every document it emits carries
that key. The declaring is done where the rows are read:
`datalib_etl_render::inputs::Inputs` collects them as a bucket is
built, and `Lookup` wraps a lookup table (users, channels, recipients)
so a key cannot be read without being declared, a miss included. A
processor that reads a table whole declares `Input::whole_table`
(`input_id = '*'`), which any row of the table matches. A composite key
is one string, its columns in `pragma_table_info` order joined by `|`.

A row a provider reads but does not declare will not re-render the
bucket when that row changes, and nothing else will notice.
`tests/fixtures/render_contract_test.py` is the check: for every raw
table in the TNG fixture it edits one row, runs the incremental path,
and asserts that the result equals a cold render and that exactly the
buckets the store says read that row were rendered.

### How a run decides what to render

The driver reads the stored cursor and decides a plan. A renderer
version bump (the tree holds `markdowns.renderer_version` values the
processors no longer declare) or a params change means **everything**;
otherwise it diffs from the stored commit; with no cursor at all the
provider reads its whole store, which is not the same as "everything"
— see the sweep below.

With a cursor, a raw doltlite store and something declared, the driver
runs the **reverse lookup** (`reverse_lookup`): it pins the raw store's
HEAD, asks `dolt_diff` for every changed key in every table
`render_inputs` mentions between the cursor and the pin, and reads back
the buckets that declared any of those rows. The provider is handed the
pin and that stale set (`RenderCtx::raw_pin`, `RenderCtx::stale_buckets`,
together `RawRange`), reads at the pin and never at HEAD, so what it
loads is what the set was computed against. A table the driver cannot
diff — the file was replaced by hand, a schema change dropped the table
— is logged and the scan is left to the provider.

The provider adds its own **forward** scan, because a row that *arrived*
was nobody's input yet: a `scan_buckets` bucket query over the added
and modified rows of its primary tables, or `changed_rows` mapped
through the rows it loads anyway where the key lives inside the payload.
`RawRange::narrow` joins the two; `narrow_by` also hands back the stale
keys whose raw row no longer exists, for the processor to declare with
nothing. `global_fanout_tables` — a table whose change re-renders the
whole source — is empty everywhere but email under a label filter,
where the mailbox tree decides which threads render at all.

**Every emitted document is written.** There is no fingerprint deciding
whether a write is needed: doltlite's tables are content-addressed, so a
row identical to the stored one is no change, carries no diff, and is
never seen by the index. A fingerprint on top of that is a second
answer to a question the store already answers, and a worse one — an
input hash that leaves out a value resolved at render time skips
changed documents, and an output hash puts a per-run column on
`markdowns`. The rule instead, pinned by
`markdowns_carries_no_per_run_stamp` in `datalib_schema`: **nothing
per-run may be written into a row whose content did not change.** The
storage report is the one document the driver decides about itself,
because its byte counts wobble run to run: it is rewritten only when a
row *count* moved (`introspect::counts_unchanged`).

### The sweep

Deletion happens at the end of a run that got through every processor,
in the same transaction as the storage report and the cursor
(`seal_run`), and it has two halves:

- **Per bucket, every run.** A bucket the run declared produces exactly
  what it emitted; any document the store still holds under that
  `bucket_key` is removed — rows and the `.md` file. A bucket declared
  with nothing is a bucket whose entity is gone; a periodized bucket
  that re-rendered to fewer documents drops the extra ones. Positive
  evidence only: a bucket the run never looked at says nothing about
  its documents, so a narrowed run cannot delete its own steady state.
- **Whole store, on a full walk only.** A *full walk* is a run that
  rendered everything (version or params changed) **and** in which
  every processor reported the raw commit it read
  (`RenderCtx::consumed`). Then a document the walk did not produce —
  a chat re-bucketed under a different period, a uuid minted by the old
  recipe — is gone. A first run with no cursor is not a full walk, and
  neither is one in which a processor read no store (none on disk,
  nothing committed): it said nothing about what should exist, and
  nothing is swept.

The `.md` file matters as much as the rows: `md_path` is what
`/applet/unified_index/chat/{uuid}` serves and what qmd indexed, so a
document deleted from the store but left on disk is a deletion the user
can still read. A document that comes back at a different path loses
the file at its old one (`put_document`). `grid_index` needs no part in
any of this — it learns of a removal from the render store's own diff.

Across a version bump or a params change the store is **kept**: its
history, the commit `grid_index` last consumed, and the cursor. "Render
every bucket again" and "forget where I was" are different requests,
and only the first is ever wanted — a re-keyed document then reaches the
index as a deletion plus an addition rather than as an old row nobody
removes.

### Edge cases

1. **First run, no cursor.** The provider reads its whole store at
   HEAD; nothing is swept except through the buckets it declared; the
   cursor is recorded at the end.

2. **`--reset-and-redownload`.** The raw tables are truncated and
   refilled, and the cursor stays: the next diff runs from the old
   commit to the pin, where the tables are populated again, and an
   unchanged row diffs as unchanged. Nothing wipes the cursor and
   nothing should. A reset read at a checkpoint taken mid-wipe is the
   one case no record can fix — every row reads as removed — and the
   rule against it is on the ingest side: a wiping run does not
   checkpoint ([`plans/one_mode.md`](plans/one_mode.md)).

3. **A store with tables but no commit.** Download ran but nothing
   was committed — a test that bypasses the runner. `pin::head`
   refuses to pin it ("unreadable, not empty"), the provider reads
   nothing and reports no consumed commit, the cursor stays and
   nothing is swept. Have the test commit after download. Two
   neighbours look similar and are not: a cursor the store cannot
   resolve (someone replaced the file by hand) is a `warn` every run
   and a render of everything, and a bucket query naming a table or
   column the store does not have is an **error**, not a cold start —
   cold-starting past it would re-render everything on every run and
   never say why.

4. **Removed rows.** The diff names them, the reverse lookup names the
   buckets that read them, and each renders to fewer documents or to
   none and is declared so. The per-bucket sweep removes the rest,
   `.md` files included — nothing sits waiting for a collector.

5. **Multi-commit ranges.** Five ingest runs between two renders are
   one diff from the cursor to the pin; a row that left and came back
   in between is `unchanged` or `modified`, which is the right answer.

6. **Concurrent renders.** Two render steps on one source are two
   writers on one doltlite file, which AGENTS.md's "One open per
   doltlite file" rules out; the runner never schedules it.

7. **No doltlite extension.** A build without `dolt_hashof` reads the
   same as a store with no commit to pin (case 3): nothing to read,
   cursor untouched.

### Rendering the delta itself

If you can render a collection of things, consider rendering the
difference between two versions of it. A **diff group** does that with
no second renderer: the source's render processors run at two raw
commits into a collecting sink — each pass the incremental render the
sync step already does, so the cost tracks the buckets that moved, not
the store — and the two sides are subtracted per document
(`datalib_etl_render::diff`): rows keyed by `uuid` become `added`,
`removed`, `modified` (naming the columns that moved) or `unchanged`;
sections keyed by their `data-section-uuid` are wrapped in
`diff-added` / `diff-removed` / `diff-modified` bands, with a
line-then-word diff inside a modified one. The result is written as an
ordinary render tree, every uuid re-minted under the diff group so it
never claims a source row's id.

What this asks of a renderer is nothing beyond the contract above: a
render must be a pure function of the raw rows at a pin (no per-run
stamp in a row), every row and section must carry a stable uuid, and
the renderer must say what its sections are (`RenderedMarkdown.sections`,
concatenated they are the `.md`) rather than leaving the driver to
parse them back — the one thing a renderer written before diff groups
may lack, and the degradation is documented: its documents diff as one
block. [`plans/diff_renderer.md`](plans/completed/diff_renderer.md) is the
design record.

### Render-side partial-progress visibility

**Desired principle**: a long-running render pass — first run after
a big initial download, or a version bump that invalidates every
document — must be as monitorable and as stoppable-resumable as
download is. The user sees "rendered 12,347 / 89,201" with an ETA;
^C-then-rerun resumes from 12,347 not 0.

**Open**: the per-batch checkpoints *do* give resumability (see §2),
but render-side progress reporting is less developed than
download-side. Worth measuring.

## 6. Timestamps

If [object identity](data_architecture_ingestion.md#object-identity-ship-of-theseus-on-uuids) is "UUIDs give global object identity," this is its temporal sibling: **timestamps give global temporal ordering** across every provider that has a time-shape to its data. That global ordering is what makes the UI's union grid time-sortable, what makes `before:` / `after:` queries mean the same thing across Slack and GitHub and Notion, and what lets a sync delta be "what happened in the last week" instead of "what happened to be at the top of each provider's result list."

The principle: **every event-shaped `GridRow` carries an ISO-8601 timestamp with explicit offset.** There are two of them, and they mean different ends of the thing:

- **`created_at`** is when the thing came into being — a Slack message's `ts`, a PR's `created_at`, a page's `created_time`. For a document row (the thread, the conversation, the PR) it is the earliest moment in the document: the first message, not the last. It is the global sort key.
- **`modified_at`** is when it last changed — the last message or reaction in a thread, a PR's `updated_at`, a page's `last_edited_time`, a vCard's `REV`. For a row inside a document it is the edit stamp where the source keeps one and **null** otherwise; null means "not known to have changed since it was created", never a copy of `created_at`.

The per-provider table is in [`grid_rows.md`](grid_rows.md#created_at-and-modified_at). Concretely, for either stamp:

- **Real upstream timestamp when one exists.** Preserved with the explicit offset upstream gave us (typically `+00:00` for APIs that hand back UTC).
- **Microsecond-bump for synthesized timestamps.** Blocks and sub-items that lack their own timestamp (chat blocks within a message, ChatGPT messages within a conversation that only has a create_time) get a synthesized one by bumping microseconds off the parent's stamp. This keeps within-parent order stable across re-runs and guarantees no collision with real stamps (real timestamps don't carry per-row µs precision from upstream).
- **Strict ISO-8601 with offset, not bare `Z` or naive.** A naive timestamp can't be globally sorted alongside a `+02:00` one without a hidden timezone assumption.

The crate that enforces all of this —
`IsoOffsetTimestamp::now_local()`, `parse_strict`,
`parse_with_assumed_utc`, `bump_micros` — is shared with download and
documented in
[the ingestion doc](data_architecture_ingestion.md#single-source-of-truth-datalib-time).

### No fabricated timestamps
A logical corollary of the broader "[don't make up data](data_architecture_ingestion.md#wire-fidelity-of-the-raw-store)" principle, called out here because timestamps are the easiest place to accidentally violate it:

- When upstream gives us no timestamp and we can't pick one up from a parent (no `bump_micros` source), `created_at` is **null**. Not "epoch," not "now," not "midnight UTC of the row's date."
- When upstream's timestamp string is naive and we haven't audited that feed, parsing returns an error — surfaced as a warning in the per-run summary, not silently rescued.
- Fallback paths that synthesize a value when upstream is silent are anti-patterns even when they "look plausible." They mask incompleteness in ways the consumer can't tell apart from real data.

### Entities without a time-shape
Some upstream object types genuinely don't have a meaningful timestamp:

- **Contacts (vCards).** A person doesn't have a creation event; they exist. The vCard's `REV` field is sometimes set, but most contacts lack one.
- **Perseus texts and other immutable corpora.** The corpus is upstream-frozen; per-section "timestamps" would be nonsense.
- **Workspace/account metadata** (Slack `team`, GitHub `org`): arguably has a creation date, but it isn't shown in any time-ordered view.

For these `created_at` is **null** and the consumer query filters them out of time-ordered views — the principle is "**event-shaped** rows get real timestamps," not "every row everywhere." A new provider should decide explicitly which of its row types are event-shaped and document the source of `created_at` for each.

## See also

- [`plans/data_lib_as_a_library/data_handling_practices.md`](plans/data_lib_as_a_library/data_handling_practices.md)
  — the audit and retrofit plan for §4.
- [`plans/one_mode.md`](plans/one_mode.md) — the rule every step's
  writes follow, of which §5's sweep is the render-side half.
- [`step_protocol.md`](step_protocol.md) — where R2's third category
  has to be written down to mean anything.
- [`grid_rows.md`](grid_rows.md), [`edges.md`](edges.md),
  [`entity_ids.md`](entity_ids.md) — the tables and the id recipe.
