# Data handling practices: what we adopt, how we retrofit, how we build next

> **Status: proposal.** Nothing here is implemented. Written 2026-09-03.
> Prompted by the `data-pipeline-builder` skill in
> [`imbue-ai/default-workspace-template#534`](https://github.com/imbue-ai/default-workspace-template/pull/534),
> which turned out to be ahead of us on a set of things worth stealing.
>
> **This does not replace
> [`data_architecture_ingestion.md`](../data_architecture_ingestion.md)
> or its [practices companion](../data_architecture_ingestion_practices.md).**
> Those own storage, identity, incrementality and the download
> architecture, and nothing here contradicts them. This doc owns one
> layer they barely cover: **what happens to a record we cannot store
> correctly.** Where a rule here is new, it should eventually be folded
> back into those two rather than living apart.
>
> Companion doc: [`toolchain_for_agents.md`](toolchain_for_agents.md),
> which is about shipping this to other people. **Read this one first
> — that one depends on it.**

## 1. Where this came from, and the honest scorecard

`imbue-ai/default-workspace-template#534` adds a skill that walks an
agent through building an ingestion tool in about ten minutes and ~600
lines of stdlib Python. Read its
SKILL.md next to our ingestion doc and the **storage core** converges,
independently and closely: upstream identity as the primary key with no
surrogates, `ON CONFLICT (id) DO UPDATE` with every column from
`excluded` as the only write shape, chunked multi-row writes in one
transaction, a raw layer preserved verbatim with everything downstream
derived from it.

**A note on vocabulary, because the mapping is not one-to-one.** The
skill has a single pure `parse(record) -> row` step. We have no stage
by that name. Our equivalent work happens in **render**: download
stores the upstream payload verbatim (decoding a binary wire format
where one exists, but never normalizing), and render deserializes that
stored payload, projects it into `GridRow`s, and writes the markdown
and the sidecar. So when this doc says a record "fails to parse," the
thing that actually happened is that **render could not deserialize or
project it** — and the fix is a re-render, never a re-fetch. Where a
sentence below describes the skill, it keeps the skill's word; where it
describes us, it says render.

The **data-quality surface does not converge**, and there we are behind.
The pattern is consistent and has an obvious cause: datalib grew from
the download side, where the interesting failures are network-shaped
(timeouts, 429s, expired cookies), and we built real machinery for
those. The skill grew from the parse side, where failures are
data-shaped (a field that is a string this week and a list next week),
and we have almost nothing.

**What we already get right** — keep these, they are not in question:
upstream-id primary keys; complete single-writer upserts; per-attempt
bookkeeping on a sidecar table so the entity diff stays clean; wire
fidelity with normalization deferred to render; the timestamp
convention (preserve the source offset, never fabricate); sorting a bag
before storing it; absent-vs-malformed for a whole source; a
consecutive-failure budget and a give-up policy on fetch;
`--reset-and-redownload` as a completeness check.

**Where the skill is ahead** — checked against the tree, not against our
own prose:

| | the gap | what we have instead |
| --- | --- | --- |
| G1 | One problem sink with a reason taxonomy: unreadable file / non-object / no identity → **drop**; failed coercion → **null**; each one line with a reason and a sample | A single `errors=N` for fetch failures, and nothing at all for projection |
| G2 | A stated field-nulling policy — "any rule that turns a non-null source value into null is a judgment call" | No such concept, so no way to count them |
| G3 | The **judgment-call table**: every lossy rule listed with the number of records it affected | No analogue anywhere |
| G4 | A **systematic-breakage exit**: stop when a run drops more than some fraction of what it read | Nearest is a consecutive-failure budget on *fetch*. See [§"Detecting upstream shape drift"](../data_architecture_ingestion_practices.md#detecting-upstream-shape-drift), recorded as an open question after `endpoint_shapes` was deleted |
| G5 | A **spot check sharing no code with the code under test** (their `parse`; our render projection): N random exported rows compared against their raw records | Insta goldens, which assert output matches *what it matched last time* |
| G6 | **Order-independence** asserted as a property, falling out of a `max((version, batch))` merge | Last-complete-write-wins, plus `--reset-and-redownload` for completeness only |
| G7 | **Retention**: bound the store, the ledger and the log inside the load itself | No pruning anywhere in the ingestion path; no CAS GC either |
| G8 | Profiling the data before writing the projection | Nothing |

One of these is load-bearing beyond its own row. **G5 is the only gap
whose absence hides the others.** A golden cannot tell you a field has
been dropped since the day a provider landed, because the golden was
recorded after the drop. `AGENTS.md` says this in its own voice —
"test-quality claims are the highest-risk category, because a false one
is self-concealing" — and our provider goldens are exactly that shape.

## 2. The practices

**The rules themselves now live in
[`docs/dev/data_architecture_parse_and_render.md` §4](../data_architecture_parse_and_render.md#4-data-quality-rules)**,
because that is where they belong: they are durable architecture for
the render stage, not a project plan. They were written up here first
only because render had no architecture document to put them in — which
is itself the finding that produced one.

In short, and in the order that doc states them:

| | rule |
| --- | --- |
| R1 | **Drop, count, log; never abort, never hide** — one sink, one taxonomy, `{source, stage, key, field, reason, sample}` |
| R2 | **Three failure categories, not two** — absent / malformed-but-isolated / malformed-systemic |
| R3 | **Any rule that turns a non-null source value into null is a judgment call** — and gets a generated table row with a count |
| R4 | **A run that drops too much stops** |
| R5 | **Verify against the source, not against yesterday's output** |
| R6 | **Findings are for the consumer, not fixes for the projection** — normalization belongs to the display layer |
| R7 | **Bound what grows, and say so where it shows** |

Two scope notes that belong to this plan rather than to the
architecture doc:

- **R6's order-independence sibling is out of scope for the retrofit.**
  `max((version, batch))` beats last-complete-write-wins only when
  inputs arrive in batches in arbitrary order. Our twenty providers
  each have one writer walking a live upstream whose newest state is by
  definition the truth, so retrofitting them buys nothing. It is a
  requirement for **new multi-batch-input code** — see the `docs`
  provider in [`toolchain_for_agents.md`](toolchain_for_agents.md).
- **R7 is half-blocked** and should not gate R1–R5. Bounding anything
  in a `.doltlite_db` reclaims no disk today; stating the bounds is not
  blocked and comes first.

**What we already get right** stays unchanged and is not in question:
upstream-id primary keys; complete single-writer upserts; per-attempt
bookkeeping on a sidecar table; wire fidelity with normalization
deferred to render; the timestamp convention; sorting a bag before
storing it; absent-vs-malformed for a whole source; a
consecutive-failure budget on fetch; `--reset-and-redownload` as a
completeness check.

## 3. The audit

Four passes over what we already shipped. Each names its command, what
it produces, and what counts as a finding. **Run them in this order** —
A finds real defects today, the rest map the surface.

### Pass A — content spot check (finds bugs now)

The strongest available independence is already sitting in the tree:
[`tests/fixtures/ingested_tng_test.py`](/tests/fixtures/ingested_tng_test.py)
is **Python**, so it shares no code with the Rust render path by
construction, and it already opens both the index db and the per-source
entity stores through the Bazel-built doltlite shell.

It also already contains the right idea applied to *identity*:
`_roundtrip_failures` recomputes each row's uuid from its stored
backpointer and fails on disagreement, reasoning that a mismatch "means
the backpointer is decorative … broken in a way nothing else would
notice, because both columns still look perfectly plausible." Extend
exactly that from identity to **content**:

> For N random `grid_rows` per provider, fetch the raw payload by
> `upstream_id` and assert the pass-through scalars agree —
> `author`, `when_ts`, `source_url`, and a prefix of `text`.

Every failure is one of three things: a real loss, a deliberate rule
that belongs in R3's table, or a mapping we cannot express in SQL (fine
— exclude it by name, with a comment saying why).

### Pass B — the silent-fallback inventory

Measured on this tree, test modules stripped, render code only:

```
TOTAL 424   unwrap_or(=283  unwrap_or_default=79  else{continue}=46  .ok()=16

  notion 110    chatgpt 52    slack 49    email 45    claude 37
  perseus 30    github 29     gitlab 23   beeper 16   whatsapp 12
  yolink 9      signal 5      contacts 4  pdf 3
```

**This is the size of the audit surface, not a bug count**, and saying
otherwise would be the exact error this repo keeps warning about. Most
of these are fine: `unwrap_or("")` on an optional display string is not
data loss. The triage question per site is one line:

> Can this fallback fire on input that upstream actually sends, and if
> it does, does the row that lands differ from the truth?

Three outcomes: **harmless** (leave, no comment needed); **lossy but
intended** (route through R1's sink, add the R3 row); **lossy and
unintended** (a bug — fix it). Notion, chatgpt, slack and email are 60%
of the surface between them and are where to start.

### Pass C — the abort-on-one-bad-record inventory

Which paths fail an entire step because one record is bad. Two known
already: `grid_index`'s per-sidecar loop propagates every error with
`?` (an unreadable sidecar, or a uuid claimed by two sources, ends the
whole load), and each provider's render entry does the same for a
payload that will not deserialize. Walk each provider's render entry
and classify every `?` as R2's second or third category. The output is
a list of places to convert, not a count.

### Pass D — growth

For each append-only store and tree — raw entity tables, the CAS, the
event tape, `rendered_md/`, the qmd index, `system/usage.doltlite_db` —
record what bounds it today. Expect the answer to be "nothing" almost
everywhere. `multimodal_retrieval.md` §4 already measured a real data
root and found the same text stored five times and attachment bytes
twice, uncompressed; this pass is that, extended and written down per
store.

### Recording the results

One table per pass, appended to this doc, with a row per provider and a
date. Not issues-only: the value is being able to see the whole surface
at once, and an issue tracker cannot show you that. File issues for the
*fixes*, link them from the table.

**First run: [`render_audit_2026_09_03.md`](render_audit_2026_09_03.md)**
— passes A–D against the tree, with the per-provider table, the R1 sink
designed as a per-source doltlite `render_problems` store, and ten
named timestamp fabrications. It grew past a table, so it lives in its
own file rather than inline here. Two corrections to this doc came out
of it: Pass B's 424 sites should be **461** (the walk missed the three
providers that render from `src/render.rs` rather than a `src/render/`
directory), and R7's `rendered_md/` pruning gap is now half closed by
`discard_tree_from_an_older_renderer`.

## 4. Retrofit

**Order.** Pass A first, and land the harness before triaging anything,
because it converts the rest of the work from speculative to
evidence-driven. Then R1's sink (it is the precondition for R3 and R4).
Then providers in Pass B order — notion, chatgpt, slack, email — since
that is where the surface is.

**Definition of done, per provider.** A provider is retrofitted when:

1. Its render path routes every drop and every null through `problem()`.
2. Its Pass B sites are triaged: each is harmless, or sinks, or fixed.
3. It has an R3 table, generated, with real counts from a real run.
4. It fails the step only for R2's third category.
5. Pass A's spot check covers its pass-through columns, or names the
   ones it deliberately skips.

**Do not do all twenty at once.** One provider end-to-end first — I
would pick `claude`, which the practices doc already names as the
template for API-backed sources and which is small enough to finish —
then use what it taught us to fix the shape before touching notion.

**Expect the first provider to change the plan.** If it does not, we
were not paying attention.

## 5. New ingestion code going forward

These are additions to the existing recipe in
[`data_architecture_ingestion_practices.md`
§"Adding new sources is meant to be easy"](../data_architecture_ingestion_practices.md#adding-new-sources-is-meant-to-be-easy),
not a replacement for it. That list stays; this is what it grows.

**Before writing the projection**, when nobody on the team has read the
corpus (a bulk export, a new provider's API, a file format): write one
throwaway script, at most ~60 lines, run once over the *smallest*
sample, and print — per identity candidate, distinct values vs records
and how many repeat within one batch; per group-by field, distinct
values, null rate, and how many collapse under case-folding; per
timestamp field, null rate, unparseable count, min, max, and how many
parse but are implausible; per field, the value *types* actually seen.
Paste the output verbatim into the provider's `DOWNLOAD.md`. This is
G8, scoped to where it pays: not a tool, a habit, and only when the
corpus is unread.

**When writing the projection** (the pure function in the provider's
`render/parse.rs` / `render/schema_translate.rs`)**:** a table-driven
test with one row per
oddity class the profile turned up — a list where a string is
declared, a bare string where a list is, an int where a string is, an
unparseable date, a timestamp that parses but is absurd, a missing
optional, a type the contract does not cover — written *before* the
projection, so each row is one line to add.

**When writing the render path:** every drop and null through
`problem()` (R1); no fallback without a one-line comment saying what it
means when it fires; the R3 table generated from the first real run.

**When writing tests:** the spot check against source (R5) alongside the
golden, not instead of it.

**When the input arrives in batches:** an order-independent merge —
`max((version, batch))`, per §2's scope note — and one test asserting
the export is identical for every batch ordering.

**Before calling it done:** what does this grow without bound, and is
that written down (R7)?

## 6. What we are not adopting

- **A hard line count.** The skill budgets ~600 non-blank lines and
  bans a base class with one subclass. Right for a ten-minute
  single-purpose tool, wrong for a twenty-provider framework whose
  whole value is shared machinery.
- **"Store the projection, not raw records."** We deliberately do the
  opposite at the raw layer so a projection bug is a re-render rather
  than a re-fetch. The skill's rule is correct for its input model — raw
  batches sitting on local disk, where re-parsing is free — and wrong
  for an API we cannot re-pull. See
  [§7 of the toolchain doc](toolchain_for_agents.md), which proposes
  saying so to its authors.
- **The ban on benchmarks and timing runs.** Aimed at stopping an agent
  from burning its budget measuring; we have the opposite problem —
  `practices.md` §"Quantitative bound on 'fast incremental'" records
  that we *don't* measure and should.
- **Retrofitting an order-independent merge onto existing providers.**
  See §2's scope note.

## See also

- [`toolchain_for_agents.md`](toolchain_for_agents.md) — the companion,
  and downstream of this one.
- [`data_architecture_ingestion.md`](../data_architecture_ingestion.md)
  — storage, identity, incrementality; the layer under this one.
- [`data_architecture_ingestion_practices.md`](../data_architecture_ingestion_practices.md)
  — the new-provider recipe §5 extends, and the open questions G4 and
  R4 speak to.
- [`step_protocol.md`](../step_protocol.md) — where R2's third category
  has to be written down for it to mean anything.
