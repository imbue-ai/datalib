# Render audit, 2026-09-03: measured against the two architecture docs

> **Status: audit + proposal.** No code changed. This is passes A–D of
> [`data_handling_practices.md` §3](data_handling_practices.md#3-the-audit)
> actually run against the tree, plus the design work the findings
> imply.
>
> What it audits: every provider's parse/render code, the two shared
> render crates (`chat-common`, `contact-common`), the render step
> driver, and the grid-index loader — against
> [`data_architecture_parse_and_render.md`](../data_architecture_parse_and_render.md)
> (the stage contract, §4's rules R1–R7, §6's timestamp policy) and
> [`data_handling_practices.md`](data_handling_practices.md) (gaps
> G1–G8).
>
> Every claim below names a file and a line, or the command that
> produced the number. Where the audit disagrees with existing prose,
> the disagreement is called out rather than smoothed over — see §8.

## 1. The one-paragraph version

The render stage has **no channel through which a data problem can be
reported.** That is not a metaphor: `RunCtx::for_render` constructs its
context with `metrics: None, diagnostics: None`
([`processor.rs:253`](../../../datalib/backend/etl/src/processor.rs)),
and both accessors `panic!` when called on a render context. So a
renderer that notices a bad record has exactly two things it can do —
crash the step, or silently substitute a plausible-looking value — and
the tree shows it doing both, roughly 461 times.

This single structural fact explains every gap the practices doc
predicted, and it is why R1 (the problem sink) is genuinely the
precondition for R3 and R4 rather than merely the first item on a list.
There is nowhere to put a count today.

The severity is higher than the docs state. Verified against
[`scheduler.rs:1527`](../../../datalib/backend/dag/src/scheduler.rs):
when one provider's render fails with `FailureKind::Data`, the
`unified_index/grid` fan-in is left **`Blocked`** — it does not run.
One unparseable timestamp in one Slack message stops the grid from
updating **for every provider**, and the run reports a red step rather
than "one message was dropped."

## 2. Scope, and how the numbers were produced

Seventeen providers render (they implement
`DataProcessor::render_version`; the other three — `fsindex`,
`lightroom`, `media` — scan trees and emit no rows). Fourteen keep
render under `src/render/`; three (`google_takeout`,
`sms_backup_restore`, `linkedin`) render from a flat `src/render.rs`.

| provider | render path | uses `datalib-time` | insta golden | round-trip checked | Pass B sites |
| --- | --- | --- | --- | --- | --- |
| notion | own | **no** | 1 | no | 122 |
| slack | chat-common | **no** | 2 | yes | 63 |
| chatgpt | chat-common | **no** | 3 | yes | 57 |
| email | chat-common | **no** | 0 | no | 48 |
| anthropic | chat-common | **no** | 3 | yes | 44 |
| github | own | **no** | 1 | no | 37 |
| perseus | own | yes | 0 | no | 31 |
| gitlab | own | **no** | 1 | no | 27 |
| beeper | **own copy** | yes | 0 | no | 23 |
| google_takeout | chat-common | **no** | 0 | no | 23 |
| sms_backup_restore | chat-common | **no** | 0 | no | 16 |
| whatsapp | chat-common | **no** | 0 | no | 15 |
| yolink | own | yes | 4 | no | 14 |
| signal | **own copy** | yes | 0 | no | 10 |
| contacts | contact-common | yes | 0 | no | 6 |
| linkedin | chat-common | **no** | 0 | no | 6 |
| pdf | own | **no** | 0 | no | 3 |

Read that table as three separate findings, developed below: the
`datalib-time` column is §5, the golden/round-trip columns are §6 (R5),
and "own copy" in the render-path column is §7 (§3 unification).

### A correction to Pass B's published inventory

The practices doc reports **424** fallback sites. Re-running its
methodology (`unwrap_or(`, `unwrap_or_default`, `else {continue}`,
`.ok()`; test modules stripped; render code only) gives **461**, and the
difference reconciles exactly:

```
461  this audit, all 17 rendering providers
-37  google_takeout (18) + sms_backup_restore (14) + linkedin (5)
=424  the published figure
```

The published number is correct for the fourteen providers it walked.
It missed exactly the three that render from `src/render.rs` instead of
a `src/render/` directory — the walk was directory-shaped, so those
three are invisible to it. Worth fixing in the doc, and worth noting as
a general hazard: **`src/render/` is a convention, not an invariant,**
and any tool that assumes it under-reports by about 8%.

A second methodology note: adding `unwrap_or_else(` — which the
published count omits and which is the same pattern with a closure —
brings the total to **545**. That is the honest size of the surface.

## 3. R1 / R2 — drop-count-log, and the three failure categories

**Status: absent, and structurally blocked.**

### Every row-construction failure is fatal

`GridRow::builder().build()` is the tree's one validating chokepoint
([`grid_rows_builder.rs:153`](../../../datalib/backend/schema/src/grid_rows_builder.rs)),
and it is a good one — it rejects an empty `uuid`/`provider`/`kind`/
`source_label` and a `when_ts` that is not RFC 3339 with an explicit
offset. Its own module comment explains that validating here turns "a
silent display bug into a loud error a provider's own tests trip over."

That reasoning is right about *where* to validate and wrong about *what
to do next*. Every one of the 13 `.build()?` callsites — plus the
`.build().map_err(anyhow::Error::from)` variants in notion, signal,
email, yolink, perseus and contact-common — propagates the error to the
top of the source's render. **Not one drops the row and continues.** So
the taxonomy R1 asks for ("a field fails its declared coercion → null
that field, keep the record") cannot be expressed: `build()` offers
exactly one failure mode, and it is "kill the step."

### …and the failure is then classified as systemic

[`hints.rs:18`](../../../datalib/backend/datalib_step/src/hints.rs)
maps errors onto the DAG taxonomy by substring-matching the error
chain. Anything that is not recognizably auth-, rate-limit- or
network-shaped falls through the final `else` to `"data"`. A
`GridRowError::InvalidWhenTs` on one row of forty thousand is therefore
classified identically to "the raw store will not open."

This is R2's diagnosis exactly, now confirmed end to end:

| R2 category | what the tree does |
| --- | --- |
| absent — emit nothing, exit 0 | works |
| malformed but isolated — drop, count, continue | **does not exist** |
| malformed systemically — exit non-zero, poison | what every isolated defect gets |

### The blast radius, verified

The scheduler test `subset_sync_leaves_pending_work_in_other_chains_alone`
([`scheduler.rs:1527`](../../../datalib/backend/dag/src/scheduler.rs))
constructs precisely this scenario — an email render that fails with
`FailureKind::Data` and the message `"boom: unparseable row"` — and
asserts that `unified_index/grid` does not run that pass. The index is
a fan-in over every source's `rendered_md`, so one provider's bad
record costs every provider's index update for that run.

### The same defect at the loader

Pass C named `grid_index` and it holds up.
[`load_all_batch`](../../../datalib/backend/etl/src/grid_index.rs)
(line 785) propagates every per-sidecar error with `?`: an unreadable
file, a sidecar that will not deserialize, or a `uuid` claimed by two
sources. It is worse than "the load stops," because the whole loop runs
inside one `begin_transaction`/`commit_transaction` pair — an error
rolls the entire batch back, so **the index does not advance at all.**
One malformed sidecar out of ninety thousand reverts the run.

The id-collision case deserves separating from the rest. A `uuid`
claimed by two sources is a genuine correctness emergency (the comment
at line 477 explains why: a full overlap silently overwrites, a partial
overlap trips the primary key), and failing hard on it is defensible.
An unreadable sidecar is not in that category and should be dropped and
counted.

## 4. The proposal: a per-source problem store in doltlite

This is the design for R1's sink, incorporating the shape asked for
during the audit: a doltlite table keyed by the uuid of the item being
processed, whose rows are overwritten or removed when a later run
succeeds, with the payload a serialized JSONB object holding a list of
problems.

That shape is a better fit than a log file for a reason worth stating:
**a log answers "what happened during this run," and the question people
actually ask is "what is wrong with my data right now."** A table whose
rows disappear when the underlying problem is fixed answers the second
question directly, and — because it is doltlite — answers the first one
too, for free, via `dolt_diff`.

### Where it lives, and why not one shared file

One file per source:

```
<data_root>/<name>/render_problems.doltlite_db
```

**Not** a single `system/problems.doltlite_db`. The DAG runs with
`parallelism: 4` by default
([`scheduler.rs:104`](../../../datalib/backend/dag/src/scheduler.rs)),
so up to four render steps are live at once, and doltlite's working set
is per *file* and shared across processes — the constraint AGENTS.md
states as "one writer per file, and it is load-bearing." Four
concurrent renderers on one problems file would commit each other's
in-flight rows, which is the exact failure that moved feedback out of
the index database.

It also must not go inside the source's existing
`raw/entities.doltlite_db`: that file's single writer is the *download*
step, and render writing to it would reintroduce the same problem
across stages rather than across sources.

### Schema

```sql
CREATE TABLE render_problems (
    -- The item this is about. Normally the grid_rows.uuid the record
    -- would have produced. See "records with no identity" below for
    -- the case where we cannot know it.
    uuid            VARCHAR(96)  NOT NULL,
    -- What must be reprocessed for this row to be re-evaluated. The
    -- markdown_uuid of the document the record belongs to, or — when
    -- the failure happened before we knew that — the raw-store entity
    -- id. This is the sweep key; see "clearing on success".
    scope_key       VARCHAR(96)  NOT NULL,
    scope_kind      VARCHAR(16)  NOT NULL,  -- 'markdown' | 'entity'
    stage           VARCHAR(16)  NOT NULL,  -- 'parse' | 'render' | 'grid_row'
    -- 'dropped'  the record did not reach the index
    -- 'nulled'   it did, with at least one field discarded
    -- 'ok'       it did, intact; these problems are observations only
    -- R1's two outcomes, plus the pure-warning case (see "Rendering
    -- and reporting are not alternatives").
    outcome         VARCHAR(16)  NOT NULL,
    -- serde_json of Vec<Problem>. One row per item, all of its
    -- problems together, so an upsert replaces the item's whole state
    -- atomically.
    problems        JSONB        NOT NULL,
    first_seen_at   VARCHAR(40)  NOT NULL,
    last_seen_at    VARCHAR(40)  NOT NULL,
    render_version  INT          NOT NULL,
    PRIMARY KEY (uuid)
);
```

`first_seen_at` / `last_seen_at` follow the repo timestamp convention
(local time with explicit offset, from the run-pinned clock — see §5's
note about `RunCtx.now` being empty on render today, which this needs
fixed).

### The payload type

```rust
/// One thing that went wrong with one record. Serialized as a list
/// into `render_problems.problems`.
#[derive(Serialize, Deserialize)]
pub struct Problem {
    /// The field this is about; `None` for a record-level problem
    /// (undeserializable, no identity).
    pub field: Option<String>,
    /// Where in the stored payload, as a JSON pointer, when we know.
    pub path: Option<String>,
    pub reason: Reason,
    /// The R3 judgment-call rule that fired, when this was a
    /// deliberate lossy rule rather than a defect. This is the column
    /// the R3 table is generated from — see §9.
    pub rule: Option<&'static str>,
    /// First 80 characters of the offending value. R1: "never a count
    /// without a reason, never a reason without a sample."
    pub sample: String,
}

#[derive(Serialize, Deserialize)]
pub enum Reason {
    Undeserializable,   // → drop the record
    NoIdentity,         // → drop the record
    CoercionFailed,     // → null that field, keep the record
    UncoveredType,      // → null that field, never pass through untyped
    DeliberateLoss,     // → an R3 rule fired (truncation, chrome-strip)
    Noted,              // → nothing lost; a finding worth publishing
}
```

The first four variants are R1's taxonomy verbatim, so the table *is*
the taxonomy rather than a place where it is re-described. The last two
are the cases R1 does not cover: a deliberate lossy rule (R3), and an
observation that discarded nothing.

### Rendering and reporting are not alternatives

The most important thing this design gets right, and the thing the
current code cannot express at all: **a record can render successfully
*and* carry problems.** Those are not two outcomes to choose between.
The common case is not "the row was dropped" — it is "the row is in the
grid, and one of its fields was discarded getting it there."

The current signature makes that inexpressible. `build() -> Result<GridRow,
GridRowError>` is a sum type: either a row, or a complaint, never both.
Every lossy rule in §8 lives in that gap — `strip_repeated_chrome` and
`clamp_doc_text` both produce a perfectly good document *and* throw
source content away, and there is no return channel for the second half,
which is exactly why neither can report a count.

So the emit path should be a product, not a sum:

```rust
/// What one projection attempt produced. `value` is `None` only when
/// the record was dropped outright (R1's first two rows); everything
/// else yields a row *and* whatever was noticed along the way.
pub struct Rendered<T> {
    pub value: Option<T>,
    pub problems: Vec<Problem>,
}
```

with the row-level helper from P2 reading:

```rust
// Validates, records into the sink, and returns the row when one
// could be built. A row that built fine but lost a field comes back
// as Some(row) with the sink already holding the reason.
pub fn build_or_record(self, sink: &ProblemSink, scope: Scope) -> Option<GridRow>
```

Three consequences worth stating, because they are what make the table
useful rather than merely populated:

- **`outcome` is a property of the row, not of the run.** `'dropped'`
  and `'nulled'` in the schema above are exactly this distinction, and
  now they are both reachable. Today only `'dropped'` has any
  representation, and only as a step failure.
- **A `render_problems` row and a live `grid_rows` row coexist under
  the same uuid.** That is the point of keying on the item's uuid
  rather than on a run id: the grid can join them and mark a cell as
  degraded, and "show me every row that lost a field" becomes one
  query. A schema keyed on run-and-sequence could not answer it.
- **Warnings need a severity that does not imply loss.** `Reason`
  above covers *loss*; a renderer will also want to say "this looked
  odd but I kept everything" — an unrecognized enum variant it passed
  through verbatim, an attachment whose bytes were missing so the
  placeholder rendered instead. Those deserve a `Noted` variant with
  `outcome = 'ok'`, so the row exists as a finding without claiming
  anything was discarded. This is R6 in miniature: publish the finding,
  do not act on it.

### Clearing on success — the part that needs care

The obvious rule ("delete every row this run did not re-emit") is
wrong, and wrong in a way that would quietly empty the table. Render is
incremental: the fingerprint skip in
[`chat-common/render.rs:158`](../../../datalib/backend/etl/chat-common/src/render.rs)
means a steady-state run touches almost nothing, so "not re-emitted"
overwhelmingly means "not looked at," not "fixed."

The rule that works is to **sweep by the unit that was actually
reprocessed**:

> When document D is (re)rendered, delete every `render_problems` row
> whose `scope_key` is D, then insert whatever this pass found. A
> document that was skipped keeps its rows untouched — which is
> correct, because its last known state is still current.

That is one statement per rendered document, it composes with the
existing fingerprint skip rather than fighting it, and it handles
overwrite and removal in the same motion: an item fixed upstream simply
does not get re-inserted.

The `scope_kind = 'entity'` case exists because some failures happen
before we know which document the record belongs to — a payload that
will not deserialize at all has no `markdown_uuid`. Those are swept
when the raw entity is next successfully parsed.

### Records with no identity

R1 says a record with no usable identity is dropped — but then there is
no uuid to key it on. Give those rows a content-derived surrogate:

```
uuid = "noid:" || blake3(source_name ‖ stage ‖ raw_payload)[..16]
```

The `noid:` prefix keeps them sortable-apart and makes them obvious in
the UI, and the content hash means the same bad record does not
accumulate a new row every run. They clear by the document sweep like
everything else.

### Commit discipline

One `dolt_commit` at the end of the render step, not one per row —
matching the `RawStoreSession` pattern the download side already uses.
This buys a property worth having deliberately rather than by accident:
`dolt_diff` over the problems table answers **"what did this render
change about my data quality?"** — which problems appeared, which
disappeared — and `dolt_log` gives the per-run history. That is
regression detection on data quality for free, and it is the strongest
argument for doltlite here over a JSONL file.

### What it makes possible

- **R3's table becomes generated,** which is the doc's own condition
  for a lossy rule being allowed at all ("if we cannot generate the
  count, the rule is not allowed"):

  ```sql
  SELECT rule, COUNT(*) FROM render_problems, json_each(problems)
   WHERE rule IS NOT NULL GROUP BY rule;
  ```

- **R4 gets its numerator.** It still needs a denominator — see §9.
- **The status surface gets something real to show.** Today a render
  step reports a document count and nothing about quality.

### R7, stated rather than dodged

Rows are bounded by "items with an open problem," which is
self-limiting. The *history* is not: doltlite never reclaims, so
deleted rows persist in the commit graph. That is the honest statement
R7 asks for — and it argues for the end-of-step commit rather than
per-row, since per-row commits would make the history the dominant
cost.

## 5. §6 — fabricated timestamps

**This is the section with concrete, present-tense bugs.** The
architecture doc is unambiguous: when upstream gives no timestamp and
none can be inherited from a parent, `when_ts` is **null** — "not
'epoch,' not 'now,' not 'midnight UTC of the row's date.'"

The `datalib-time` crate enforces this properly. Its module doc calls
itself the funnel for "every `now()` and every inbound-timestamp
parse," `parse_strict` demands an explicit offset, and
`parse_with_assumed_utc` is documented as "the **single function in the
whole repo** where 'assume UTC' is legal."

**The render stage bypasses it.** Six render modules import
`datalib_time`; six parse timestamps with raw `chrono` instead — and
every fabrication below is in the bypass group.

| # | site | what happens |
| --- | --- | --- |
| T1 | [`slack/render/mod.rs:39,46,50`](../../../datalib/backend/etl/providers/slack/src/render/mod.rs) | `ts_to_iso` fabricates epoch **three ways**: non-numeric seconds → `unwrap_or(0)`, non-numeric fraction → `unwrap_or(0)`, out-of-range → `Utc.timestamp_opt(0,0)`. Output is a real-looking `1970-01-01T00:00:00.000000+00:00`. |
| T2 | [`slack/render/render.rs:272,278`](../../../datalib/backend/etl/providers/slack/src/render/render.rs) | A second, independent copy of the same parser with the same two `unwrap_or(0)`s. |
| T3 | [`email/render/render.rs:436`](../../../datalib/backend/etl/providers/email/src/render/render.rs) | `date_ms: em.received_at.and_then(iso_to_ms).unwrap_or(0)`. An email with a missing or non-RFC-3339 `Date` header lands at the epoch. Malformed `Date` headers are common in real mail. |
| T4 | [`whatsapp/render/parse.rs:222,261,281,431`](../../../datalib/backend/etl/providers/whatsapp/src/render/parse.rs) | `timestamp.unwrap_or(0)` — a NULL timestamp column becomes 1970. |
| T5 | [`sms_backup_restore/render.rs:226`](../../../datalib/backend/etl/providers/sms_backup_restore/src/render.rs) | `v.get("date")…unwrap_or(0)`. |
| T6 | [`google_takeout/render.rs:305`](../../../datalib/backend/etl/providers/google_takeout/src/render.rs) | `parse_date_ms` — doc comment says *"Returns 0 on any unexpected shape."* Also parses naive and calls `.and_utc()`, i.e. assume-UTC outside the one blessed function. |
| T7 | [`linkedin/render.rs:198`](../../../datalib/backend/etl/providers/linkedin/src/render.rs) | Same, and its doc comment is explicit about the consequence: *"Returns 0 on any unexpected shape (sorts such rows to the top)."* |
| T8 | [`chat-common/render.rs:738`](../../../datalib/backend/etl/chat-common/src/render.rs) | `iso_from_ms` falls back to `"1970-01-01T00:00:00+00:00"` on out-of-range. It does `warn!` first — better than the rest — but the warning is emitted on a `spawn_blocking` thread with no diagnostics buffer installed, so nothing captures it. |
| T9 | [`chat-common/render.rs:521`](../../../datalib/backend/etl/chat-common/src/render.rs) | `first_ts = doc.items.first()…unwrap_or_else(\|\| iso_from_ms(0))`. **An empty bucket gets a chat-level row stamped 1970.** Reachable: `render_markdown` explicitly handles `doc.items.is_empty()` with "_(no messages)_". |
| T10 | [`signal/render/render.rs:510`](../../../datalib/backend/etl/providers/signal/src/render/render.rs) | Its own copy of T8. |

T6 and T7 are the ones to sit with. Both fabrications are **known,
documented in the code, and reasoned about** — T7 even notes that the
fabricated rows sort to the top of the grid. Nobody was careless; there
was simply no way to express "this row has no timestamp" that did not
also mean "fail the step." That is the strongest available argument
that R1's sink is the unblocking change and not bookkeeping.

### Why null is genuinely better here

The grid sorts on `when_ts_utc`, derived from `when_ts` via
`split_when_ts`, which returns `None` on failure "so the caller can
leave both index columns NULL rather than fabricate a value." The
machinery for a null timestamp is already built and already correct.
A 1970 stamp is strictly worse than null in three ways: it sorts into a
real position, it is indistinguishable from a genuine 1970 record, and
it satisfies `before:`/`after:` queries it should not match.

### The type that forces it

`NormalizedChatItem.date_ms` is `i64`, not `Option<i64>`
([`chat-common/types.rs:124`](../../../datalib/backend/etl/chat-common/src/types.rs)).
**A chat item cannot express "no timestamp,"** so every one of the
eight providers on chat-common must invent a value at the boundary —
which is exactly what T3, T4, T5, T6 and T7 are doing. Fixing the
callsites without fixing the type leaves the trap armed for the next
provider.

The fix is `Option<i64>`, with the existing microsecond-bump
inheritance (already implemented in anthropic and chatgpt, and correct)
applied first and `None` surviving to a null `when_ts`.

## 6. R5 — verification against the source

**Status: the weakest area, and the one that hides the others.**

The practices doc calls G5 "the only gap whose absence hides the
others." Measured:

- **The round-trip check covers 3 of 17 rendering providers.**
  `SCOPE_TAG_BY_PROVIDER` in
  [`ingested_tng_test.py:111`](../../../tests/fixtures/ingested_tng_test.py)
  holds `anthropic`, `openai`, `slack`. Fourteen providers are checked
  by nothing with source independence.
- **It checks identity, never content.** It recomputes `uuid` from
  `(upstream_entity_kind, upstream_id, upstream_scope)` and compares.
  Nothing compares `author`, `when_ts`, `source_url` or `text` back to
  the raw payload — which is Pass A's whole proposal, and it is not
  built.
- **Ten of seventeen providers have no insta golden either**: email,
  perseus, beeper, google_takeout, sms_backup_restore, whatsapp,
  signal, contacts, linkedin, pdf. Their entire render coverage is the
  fixture test's row-count assertions.

The fixture test is well-built and says so about itself — the comment
at line 61 records that `gitlab` "silently produced nothing for three
months" because its records spelled a field `project_path` while every
consumer had moved to `project_full_path`. That is precisely the
failure mode R5 describes, it already happened here once, and the
defense added afterwards was a *provider-presence* assertion —
which catches "produced nothing" but not "produced everything, with one
field wrong."

**The good news is that Pass A is cheap.** The harness exists: the test
is Python (so it shares no code with the Rust projection by
construction), it already opens both the index and the per-source
entity stores through the Bazel-built doltlite shell, and
`_roundtrip_failures` is the right shape already. Extending it from
identity to content is adding a query, not building infrastructure.

## 7. §3 — unification, and two copies of the chat renderer

`chat-common` is a genuine success and should be said so plainly: it is
the "typed POD" layer §3 asks for, it serves eight providers, and its
`RenderProfile` parameterization is the right way to keep one renderer
serving many sources.

Two things are off.

**beeper and signal do not use it.** They carry their own full copies
of the chat render path — 1,256 and 1,071 lines — including their own
`build_grid_rows`, their own fingerprint computation, and (T10) their
own copy of the epoch fallback. Neither `Cargo.toml` depends on
`chat-common`.

This is not merely duplication; it is duplication that the shared
crate's own documentation denies. `chat-common/types.rs` opens with a
mapping table whose first rows are Beeper and Signal:

| provider | source value | NormalizedItem.kind |
|---|---|---|
| Beeper | TEXT, NOTICE | Text |
| Signal | StandardMessage | Text or Attachment |

A reader of that table would reasonably conclude both providers route
through `render_all`. They do not. Either migrate them or correct the
table — the current state is the "well-argued paragraph that reads as
evidence" AGENTS.md warns about.

**Slack has two timestamp parsers.** T1 and T2 are the same function
implemented twice in the same crate, with the same bugs, at
`render/mod.rs:37` (`ts_to_iso`) and `render/render.rs:270` (`ts_to_ms`). One differs in
precision (micros vs millis), which is presumably why the second was
written, and neither is reachable from the other.

## 8. R3, R6, R7 — the lossy rules, and one model citizen

R6 ("findings are for the consumer, not fixes for the projection") is
**broadly respected**, and the reason is architectural rather than
disciplinary: the raw store keeps upstream verbatim and every render
defect is fixable by re-rendering, so the projection has never been
under pressure to be the place where data is preserved. That is the
stage contract doing its job.

Two rules genuinely discard source content:

**`strip_repeated_chrome`**
([`pdf/render/convert.rs:132`](../../../datalib/backend/etl/providers/pdf/src/render/convert.rs))
removes running headers and footers. It is *well* built — position-
constrained to first/last non-empty line, frequency-constrained to half
the pages and at least two, and asymmetric between headers (exact
match) and footers (digit-normalized) with a test named
`keeps_headings_that_differ_per_page` guarding the case where fusing
them would delete every chapter title in a book. Nothing about the rule
needs changing. What is missing is only R3's accounting: **how many
lines did it remove on the last run?** Nobody can answer that.

**`clamp_doc_text`**
([`anthropic/render/render.rs:528`](../../../datalib/backend/etl/providers/anthropic/src/render/render.rs))
truncates long project documents — and it is the model citizen of the
whole audit. It cuts on a char boundary, emits a visible marker, says
how many bytes of how many are shown, names the config knob that raises
the ceiling, and tells the reader the raw store has the rest. It is the
only stated bound in the entire render surface, and its shape is the
one to generalize: **bounded, visible in the output, self-describing,
and honest about where the full value lives.**

`perseus`'s `normalize_whitespace` is a third, milder case — it
collapses whitespace before storing text. Defensible for a classical
corpus, still a judgment call, still uncounted.

### R7 — growth

Grepping the whole render surface for a stated bound returns exactly
one hit: `clamp_doc_text`. R7's requirement is that each thing render
appends to carries a bound *or* an explicit "unbounded, because X" in
its module doc. Nothing else has either.

The `rendered_md/` pruning gap is **half fixed, and the docs do not say
so.** `discard_tree_from_an_older_renderer`
([`datalib_step/render.rs:246`](../../../datalib/backend/datalib_step/src/render.rs))
now deletes the whole tree when `render_version` moves, which closes
the re-keying case the parse-and-render doc §5 describes. It does *not*
close the render-param case: `read_for_params` invalidates the cursor
and re-renders under new params, but nothing deletes the old files, so
changing `period` from `month` to `year` leaves twelve stale documents
per chat beside each new one — and they stay in the grid index, because
`apply_markdown` deletes by `markdown_uuid` and orphans are never
revisited.

## 9. Proposals

### General patterns

**P1 — Give render an observability channel.** Add
`problems: Arc<ProblemSink>` to `RunCtx`, populated on both
`for_download` and `for_render`. Pass it **explicitly** rather than via
`tokio::task_local`: the existing `Diagnostics` mechanism documents its
own caveat that "an event from a detached `spawn`/`spawn_blocking`
won't see the task-local," and the render wave runs inside
`spawn_blocking`
([`datalib_step/render.rs:66`](../../../datalib/backend/datalib_step/src/render.rs)).
Copying the ambient pattern here would produce a sink that silently
collects nothing.

**P2 — Give `GridRow::build` a non-fatal sibling.** This is the highest
leverage single change in the audit, because it converts R2-category-3
behavior into R2-category-2 behavior at *every* callsite at once:

```rust
// Validate; on failure record the problem and return None so the
// caller drops this row and keeps the rest of the document.
pub fn build_or_record(self, sink: &ProblemSink, scope: Scope) -> Option<GridRow>
```

Sixteen callsites change from `?` to `else { continue }`. The existing
`build()` stays for tests and for callers that genuinely want the hard
failure.

The important half of this is not the non-fatal return — it is that the
sink is a *second* output channel rather than a replacement for the
first. `Result` forces a choice between a row and a complaint; the
common case is a row **and** a complaint (see "Rendering and reporting
are not alternatives" in §4). A design that only reports on failure
still cannot count `strip_repeated_chrome`, which never fails.

**P3 — Make `datalib-time` mandatory in render.** Delete the six
hand-rolled `iso_to_ms` / `parse_date_ms` / `ts_to_iso` functions and
route them through `parse_strict` / `parse_with_assumed_utc` /
`parse_custom_strftime`, each returning `Option`. The crate already
claims to be the single funnel; making that true removes the entire T1–T10
class rather than the ten instances. A clippy `disallowed_methods` entry
for `chrono::DateTime::parse_from_rfc3339` inside provider crates would
keep it true.

**P4 — `Option<i64>` for `NormalizedChatItem.date_ms`,** so "no
timestamp" is expressible and nulls survive to `when_ts` (§5).

**P5 — Split the loader's failure modes.** In `load_all_batch`, keep
the hard failure for a `uuid` collision (a real correctness emergency,
well-argued at line 477) and convert unreadable/undeserializable
sidecars to drop-count-log. Today one bad file rolls back the whole
index transaction.

**P6 — A typed step failure.** `hints::classify`'s substring matching
cannot distinguish "one bad record" from "the store will not open," and
never will. R2's third category needs a real signal — a
`StepError`-carrying error type the provider constructs deliberately —
before "malformed systemically" can mean anything.

**P7 — R4's threshold, once P1 exists.** The render driver already
counts documents
([`datalib_step/render.rs:59`](../../../datalib/backend/datalib_step/src/render.rs));
add `records_read` alongside `records_dropped` and fail the step past
20%. Note that the denominator does not exist today — no provider
counts records read — so R4 is blocked on P1 in a way R3 is not.

**P8 — Escape the YAML frontmatter.** `render_markdown` builds
frontmatter by string concatenation and escapes only `"` in `title` and
`display`; `account`, `project`, `external_id` and `period` are
interpolated raw, and a newline in any value breaks out of the scalar
entirely. These are upstream-controlled strings (chat display names,
channel topics). The backend never parses QMD back — AGENTS.md is
emphatic that QMDs are write-only — but Quarto and the qmd indexer do,
so a malformed document is a real (if low-severity) consequence. Use a
serializer.

### Specific code paths, in the order I would do them

| # | change | why first |
| --- | --- | --- |
| 1 | Pass A content spot check in `ingested_tng_test.py` | Independent evidence before any refactor; makes everything after it verifiable rather than hopeful. The harness already exists. |
| 2 | P1 + P2 + the problem store (§4) | The precondition for R3 and R4. Nothing else can be counted until this lands. |
| 3 | P3 + P4, then T1–T10 | The only findings that are wrong *today*, in shipped data. |
| 4 | P5 | One malformed sidecar currently reverts the entire index. |
| 5 | R3 rows for `strip_repeated_chrome`, `clamp_doc_text`, `normalize_whitespace` | Falls out of (2) almost for free. |
| 6 | beeper + signal onto `chat-common`, or fix the table | Removes two copies of the epoch bug and the §7 doc/tree disagreement. |
| 7 | `rendered_md/` orphan pruning for the param-change case | The remaining half of the §5 known gap. |

On the practices doc's advice to do one provider end to end first: it
nominates `anthropic`, and I would agree — it is on chat-common, it has
three goldens, it is round-trip checked, and it already contains the
best-behaved lossy rule in the tree to model the R3 row on.

## 10. Doc corrections

Found while auditing; each is prose disagreeing with the tree, which
AGENTS.md asks be fixed in the same change as the finding.

**Items 1–3 were fixed on 2026-09-04**, in the same change that added
§3's parse contract and unification rules to `parse_and_render.md`.
They are kept here rather than deleted, because the finding is the
record of *how* the prose drifted — and item 3 in particular is worth
remembering, since a name three docs used confidently had never existed
in the tree at all.

1. ~~**`parse_and_render.md` §2**~~ *(fixed)* — the sidecar header field was named
   `document_uuid` in the doc and `markdown_uuid` in
   [`index_lib/src/lib.rs`](../../../datalib/backend/index_lib/src/lib.rs)
   and in every sidecar on disk.
2. ~~**`parse_and_render.md` §2**~~ *(fixed)* — "Grid index reads
   `(qmd_path, source_fingerprint)` from `markdowns_loaded`." No such
   table. `load_fingerprints` reads `(markdown_uuid,
   source_fingerprint)` from `markdowns`. `markdowns_loaded` is a
   counter field on `GridIndexSummary`; the table by that name was
   merged into `documents` (noted in
   `unified_index/tests/fixture_db_snapshot.rs:230`), and a stale
   comment in `etl/src/bin/grid_rows_load.rs:6` still describes it.
3. ~~**`parse_and_render.md` §3**~~ *(fixed there; `ingestion.md` still
   carries it)* — `schema_translate.rs` was referenced as the typed-POD
   layer "landing per provider." It exists in **zero** providers. The
   concept did land, under two other names: `render/parse.rs` per
   provider, and `chat-common` / `contact-common` for the shared
   shapes.
4. **`data_handling_practices.md` §3 Pass B** — 424 should be 461; the
   walk misses the three providers that render from `src/render.rs`
   (§2 above).
5. **`parse_and_render.md` §5** — the "nothing prunes `rendered_md/`"
   gap is now half closed by
   `discard_tree_from_an_older_renderer`; only the param-change case
   remains (§8).
6. **`chat-common/src/types.rs`** — the `ItemKind` mapping table lists
   Beeper and Signal as consumers of a renderer neither uses (§7).

## 11. What this audit did not do

- **Pass D (growth) is only partly done.** §8 records that the render
  surface states one bound; a per-store inventory of the raw tables,
  the CAS, the event tape and the qmd index is still owed.
- **The 545 fallback sites are not individually triaged.** This audit
  measured the surface, corrected the count, and triaged the timestamp
  class exhaustively because it was the one with a crisp rule to test
  against. Notion alone has 122 sites and no round-trip coverage; it is
  the largest unexamined area in the tree and deserves its own pass.
- **Nothing was run.** Findings come from reading the tree, the tests
  and the scheduler's own assertions, not from executing a pipeline.
  The claims about *what the code does* are grounded in file and line;
  the claims about *how often a fallback fires in practice* are
  explicitly not made — that is what the problem store exists to
  answer, and is the strongest reason to build it before triaging
  further.
