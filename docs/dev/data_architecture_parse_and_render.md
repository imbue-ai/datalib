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
  `render/parse.rs` / `render/schema_translate.rs`.
- **render** — turn that representation into the artifacts:
  `<id>.md` for humans and `<id>.grid_rows.json` for the index.

Together they are one pipeline stage, invoked as `datalib-step render
<provider>`. When this doc needs a word for the whole transform it
says **the projection**. What it never says is "the parse step": there
isn't one, and a record that "fails to parse" is a record **render**
could not deserialize — which matters because the fix is always a
re-render and never a re-fetch.

Like its siblings this is aspirational as much as descriptive. §4 in
particular is a set of rules we do **not** follow today; the audit and
retrofit plan is
[`data_lib_as_a_library/data_handling_practices.md`](data_lib_as_a_library/data_handling_practices.md).

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
sidecar contract (§2), and the `GridRow` family taxonomy (§3).

| doc | stage | contains |
| --- | --- | --- |
| [`data_architecture_ingestion.md`](data_architecture_ingestion.md) | download | principles, at-rest shape, operational properties |
| [`data_architecture_ingestion_practices.md`](data_architecture_ingestion_practices.md) | download | testing, adding a provider, schema evolution, open questions |
| **this file** | parse + render | the stage contract, the projection, data-quality rules, incrementality, timestamps |
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

### The sidecar contract

After download, we run transformations for display and indexing —
render to markdown with YAML frontmatter, index the markdown with qmd,
derive `grid_rows` for the UI.

The cross-provider contract is the **sidecar**: for every rendered
document, Render emits two co-located files —

  - `<id>.md` — human-readable, with YAML frontmatter.
  - `<id>.grid_rows.json` — the
    [`Sidecar`](../../datalib/backend/index_lib/src/lib.rs):

    ```jsonc
    {
      "header": {
        "document_uuid": "…",       // primary key for the document
        "source_fingerprint": "…",  // hash of upstream payload
        "render_version": 1         // renderer-side schema stamp
      },
      "rows": [GridRow, …]
    }
    ```

Grid index reads the sidecar tree — **it never re-parses markdown**.
The markdown is for humans; the JSON sidecar is the machine-readable
projection.

This part of the pipeline aspires to the same properties as download:

  - **Monitorable**: same `obs` flags, same progress-bar contract.
  - **Incremental**: the sidecar `source_fingerprint` short-circuits
    re-render. Grid index reads `(qmd_path, source_fingerprint)` from
    `markdowns_loaded` and skips unchanged sidecars.
  - **Resumable in the steady state**: a render pass that gets
    re-run after producing N of M sidecars will skip those N via the
    fingerprint check and continue from where it stopped. We do not,
    however, guarantee crash-mid-write atomicity per file; a partial
    `.md` left by a SIGKILL during a write may have a fingerprint that
    no longer matches the file body and will be regenerated next run.
    That's good enough for our use case but is not a separately
    engineered property.

Less attention has been paid to render-side observability and to
making partial-progress visible to the user than to the same on
download; this is an area where the implementation trails the
principle.

## 3. The projection

Three shapes, in order:

1. **The stored payload** — JSONB, wire-fidelity, whatever upstream
   sent. Owned by download, described in `schema_raw.rs`.
2. **The typed POD** — the provider's normalized in-memory
   representation, in `render/schema_translate.rs` (aspirational,
   landing per provider). Where a shape is shared across providers the
   canonical type lives in a shared crate.
3. **The rows** — `GridRow` + `EdgeRow`, emitted through
   `datalib_index_lib::emit_sidecar`.

(1) → (2) is parse; (2) → (3) plus the markdown is render. Both are
pure given the raw store, and both are the right place for §4's tests.

### Identity and backpointers are first-class in the projection

- **Backpointers and outlinks are first-class** in the projection schema. `GridRow` (one of our indexed representations, not a raw format) carries:
    - `uuid` — the Ship-of-Theseus identity, deterministic from upstream so re-ingest is idempotent.
    - `external_id` — the provider-native primary id (numeric GH/GL id, PR number, …) preserved alongside our UUID so we can round-trip back to the provider's API.
    - `source_url` — the canonical URL on the provider's web UI (e.g. `pull_request.html_url`, GitLab `note.web_url` with `#note_<id>` anchor), populated everywhere we can construct it.
    - `qmd_path` — the path to the rendered markdown sidecar.
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
sidecars, the problem log from R1. Give each a bound or an explicit
"unbounded, because X" in its module doc — and enforce it **inside the
run that writes, never as a separate cleanup chore someone must
remember.** Where a bound exists, the status surface states it, so
limits are never secret.

Partly blocked: doltlite never actually deletes, so a bound on anything
in a `.doltlite_db` reclaims no disk today (see
[Removing a source](data_architecture_ingestion_practices.md#removing-a-source)).
The *stating* half is not blocked. Render's known instance is already
recorded: nothing prunes `rendered_md/`, so a re-render under new
params leaves documents that changed identity beside the new ones, and
they stay in the grid index.

## 5. Incrementality and progress

Render skips what it can, by four mechanisms:

- **What changed since last render** —
  [dolt_diff supersedes per-bucket fingerprints](data_architecture_ingestion.md#dolt_diff-supersedes-per-bucket-fingerprints).
  The per-source cursor stamps the doltlite HEAD into
  `_render_cursor.json`; the next run diffs from it.
- **Whether a sidecar needs re-loading** — the `source_fingerprint` in
  the sidecar header, honored by `grid_index`.
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
every sidecar — must be as monitorable and as stoppable-resumable as
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
