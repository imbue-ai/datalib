# Data architecture: render and projection

Third sibling to
[`data_architecture_ingestion.md`](data_architecture_ingestion.md) and
its [practices companion](data_architecture_ingestion_practices.md).
Those two cover the **download** stage — how upstream bytes land on
disk and what shape they have at rest. This one covers what happens
next: **render**, which reads the raw store and projects it into
markdown plus `GridRow` sidecars.

Like its siblings it is aspirational as much as descriptive. §4 in
particular is a set of rules we do **not** follow today; the audit and
retrofit plan for getting there is
[`data_lib_as_a_library/data_handling_practices.md`](data_lib_as_a_library/data_handling_practices.md).

## 1. Why there are three of these now

`data_architecture_ingestion.md` began as one document. It was split in
`dab2c3d9` (2026-06-15) along a **principles vs practitioner** line —
the load-bearing rules stayed, and testing, adding a provider, schema
evolution and the open questions moved to the companion — so the core
would stay focused and land under 1000 lines. That split is still the
right one, and this doc does not repeat it: render's principles and its
practices are one file until it gets big enough to hurt.

The reason for a *third* file is different. Both existing docs are
scoped to ingestion, by title and by their own opening paragraphs — and
render material kept accumulating in them anyway, because there was
nowhere else to put it. The clearest tell is a bullet in the ingestion
doc that opens **"(NOT download/ingest related)"** and then goes on for
five lines about `GridRow` backpointers. Others: the whole "Time and
ordering discipline" section is a policy about `GridRow.when_ts`;
"Render and downstream stages" and "Shared schemas across similar
sources" sit in the *ingestion practices* file; the render cursor is
documented as a footnote to the download cursor.

That homelessness has a cost beyond tidiness. It is why the
data-quality rules in §4 — which are entirely render-stage concerns —
had to be written up in a separate proposal instead of landing in the
architecture docs where they belong.

**The map, after this file:**

| doc | stage | contains |
| --- | --- | --- |
| [`data_architecture_ingestion.md`](data_architecture_ingestion.md) | download | principles, at-rest shape, operational properties |
| [`data_architecture_ingestion_practices.md`](data_architecture_ingestion_practices.md) | download | testing, adding a provider, schema evolution, open questions |
| **this file** | render | the stage contract, the projection, data-quality rules, incrementality |
| [`grid_rows.md`](grid_rows.md) / [`edges.md`](edges.md) | — | the tables render writes into |
| [`entity_ids.md`](entity_ids.md) | — | the `uuid` recipe every projection must follow |

## 2. The stage contract

Render's input is `<data_root>/<name>/raw/` and **nothing else** — not
the API, not a file-backed source's `input_path`. The full argument for
that is [Layering of concerns](data_architecture_ingestion.md#layering-of-concerns-download-is-downstream-agnostic)
in the ingestion doc and is not repeated here; the one-line version is
that the raw store is the boundary between the outside world and our
copy, so everything downstream of it is reproducible offline.

The consequence that matters for the rest of this document: **every
render-stage defect is fixable by re-rendering.** A projection bug is
never a re-fetch. That is what makes the rules in §4 affordable — we
can be strict about correctness at this stage precisely because being
wrong here is cheap to correct.

Its output is the sidecar pair per document (`<id>.md` +
`<id>.grid_rows.json`), described in
[Render and downstream stages](data_architecture_ingestion_practices.md#render-and-downstream-stages).
Grid index reads the JSON and **never re-parses the markdown**.

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

Deserializing (1) into (2) and projecting (2) into (3) is the work this
document calls **the projection**. It is a pure function per provider,
lives in `render/parse.rs` / `render/schema_translate.rs`, and is the
right place to put the tests in §4.

### Identity and backpointers are first-class in the projection

Moved here in spirit from the ingestion doc's bullet that announces
itself as not belonging there. `GridRow` carries:

- **`uuid`** — the Ship-of-Theseus identity, deterministic from
  upstream so re-ingest is idempotent. The recipe is
  [`entity_ids.md`](entity_ids.md) and it is not optional: durable
  anything-keyed-on-a-row (annotations, feedback, future labels) rests
  on this being stable across a re-render.
- **`upstream_id`** / **`upstream_entity_kind`** / **`upstream_scope`**
  — the provider-native ids, preserved so we can round-trip back to
  the provider's API.
- **`source_url`** — the canonical URL on the provider's web UI,
  populated everywhere we can construct it.
- **`qmd_path`** — the rendered markdown this row points into.
- Provider-specific cross-references (`notion_page_uuid`, `slack_link`,
  `git_sha`, …) so the UI can link sideways as well as out, plus the
  general [`edges`](edges.md) table for cross-document links.

### Unified where possible, per-provider where not

`grid_rows` is one denormalized table across every source, with
`provider` + `kind` discriminators, and the backend renders it with a
single query and no per-provider branches. The families that have
already converged, and the rule for a new provider joining one, are in
[Shared schemas across similar sources](data_architecture_ingestion_practices.md#shared-schemas-across-similar-sources).

The design intent is **a unified column where a unified answer exists,
and a new column where it doesn't** — not a lowest common denominator,
and not a per-provider free-for-all. A new provider that fits a family
projects to that family's shape rather than inventing a `kind`
taxonomy; a provider that genuinely doesn't fit may motivate a new
column, and opening one should be deliberate.

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
[No fabricated timestamps](data_architecture_ingestion.md#no-fabricated-timestamps),
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

## 5. Incrementality

Render's skip logic and its cursor are documented where the mechanism
lives rather than duplicated here:

- **What changed since last render** —
  [dolt_diff supersedes per-bucket fingerprints](data_architecture_ingestion.md#dolt_diff-supersedes-per-bucket-fingerprints).
  The per-source cursor stamps the doltlite HEAD into
  `_render_cursor.json`; the next run diffs from it.
- **When a render param changes** —
  [The same problem on the render side](data_architecture_ingestion.md#the-same-problem-on-the-render-side).
  Render invalidates wholesale where download reacts proportionally,
  because local work has no rate limit to ration.
- **Whether a sidecar needs re-loading** — the `source_fingerprint` in
  the sidecar header, honored by `grid_index`.
- **Forcing a rebake** — `RENDER_VERSION` in each provider's
  `render/render.rs`.

Render-side progress reporting is weaker than download's; that is
recorded as an open question in
[Render-side partial-progress visibility](data_architecture_ingestion_practices.md#render-side-partial-progress-visibility).

## 6. What should move here

Not done, deliberately — moving text out of two docs people know is
churn worth doing on purpose rather than in passing. Proposed:

- **From `data_architecture_ingestion.md`:** the backpointers bullet
  that already flags itself as out of place; the `GridRow.when_ts`
  half of "Time and ordering discipline" (the `datalib-time` crate
  rules stay there — download stamps its own `fetched_at` too, so
  that half is genuinely cross-cutting); "The same problem on the
  render side."
- **From `data_architecture_ingestion_practices.md`:** "Render and
  downstream stages," "Shared schemas across similar sources," and the
  "Render-side partial-progress visibility" open question.

Each move leaves a one-line pointer behind. Until then this file links
to them in place, which is why §2, §3 and §5 above are mostly
cross-references rather than prose.

## See also

- [`data_lib_as_a_library/data_handling_practices.md`](data_lib_as_a_library/data_handling_practices.md)
  — the audit and retrofit plan for §4.
- [`step_protocol.md`](step_protocol.md) — where R2's third category
  has to be written down to mean anything.
- [`grid_rows.md`](grid_rows.md), [`edges.md`](edges.md),
  [`entity_ids.md`](entity_ids.md) — the tables and the id recipe.
