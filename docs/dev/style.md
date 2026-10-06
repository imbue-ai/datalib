# Style: how code in this repo is shaped

The short rules — comments, enums, timestamps, dynamic SQL,
fallbacks, unordered collections — live in [`AGENTS.md`](/AGENTS.md)
so that every agent loads them. This doc holds the one rule that needs
more than a paragraph to explain and to apply, and how to audit the
tree for docs that have drifted from the code and for copies.

## Functional core, imperative shell

The rule: **compute the decision as a pure function over values, then
act on it in a thin layer that does nothing else.** The pure part is
the *core*; the layer that reads the world into values and writes the
core's answer back out is the *shell*. The shell calls the core; the
core never calls the shell. The pattern is written up at
[functional-architecture.org](https://functional-architecture.org/functional_core_imperative_shell/);
what follows is what it means here.

### What "pure" means here

A core function takes values and returns values. It does not take a
`&Path`, a pool, a sink, a channel, a `Progress`, or `now`; it does
not spawn, write, emit, log a decision, or read the clock. Given the
same inputs it returns the same answer, every time, on any machine.
Its return is a *description* of what to do — an enum, a plan, a list
of passes, the row's next state — not the doing.

A shell function is the opposite and is allowed to be boring: read
the store into a value, call the core, write what came back, emit the
event. It should have almost nothing in it that a person would want
to write a test for, because everything worth testing was moved to
the core.

### Why this repo wants it

Three reasons, all of them things this tree has already paid for:

- **The decisions are the bugs.** Staleness, resume cursors, refresh
  windows, "queued" versus "running", one-more-pass-after-a-seal —
  every hard bug in the pipeline's history was a wrong decision, not
  a wrong write. A wrong decision inside an `async fn` that also
  fetches and upserts can only be reproduced by fetching and
  upserting.
- **Tests that wait on the world are slow and flaky.**
  [`AGENTS.md` § Tests wait on the observable](/AGENTS.md) is the
  shell-side rule; this is the core-side rule that makes most of those
  tests unnecessary. A pure decision is tested synchronously: values
  in, value out, no tokio, no tempdir, no `until(flag)` loop. The
  best-tested code in the tree is exactly the code that is shaped
  this way (below).
- **The supervisor is a pure tick.** `dag/src/supervisor/tick.rs` is
  one function from values to every step's state and the starts, stops
  and request closures to make. The loop only works while that
  function stays pure; this rule is how it stays that way.

### The templates already in the tree

Read one of these before writing a new decision; they are the house
shape.

| core | shell that feeds it | what it decides |
|---|---|---|
| `supervisor::tick::tick(shape, intent, facts) -> Tick` in `dag/src/supervisor/tick.rs` | the loop in `supervisor/round.rs` | what each step is doing, and what to start, stop and close |
| `RenderPlan::decide(stored, params, version_changed)` in `datalib_step/src/render.rs` | `render_source` | diff from the cursor, or render everything |
| `Adjustments::plan(prev, inputs)` and `select_targets` in `slack/src/ingest/mod.rs` | `fetch` | what a config change means for the walk; which conversations to walk |
| `Scan::changes_since(prev) -> Changes` in `etl/src/fsscan.rs` | `scan` | which files were added, modified, moved, removed |
| `config::check_text(text) -> ConfigCheck` in `dag/src/config.rs` | `load_graded` | what a config means and every problem in it |
| `scope_config::{turned_on, limit_relaxed, filter_widened}` | the provider's `plan` | whether a knob widened |
| `ui/src/config/{rowMenu,sourceSteps,browsePresets}.ts` | the Vue components | which menu entries, which steps, which columns a Browse card opens with |

Every one of these has synchronous tests, and the tick's walk a sync
through its states event by event, which is only writable because the
function is pure.

### Where to split

The heuristic: **if a function takes a `&Path`, a pool, a sink, or
`now`, *and* contains a `match` or an `if` a person would want a test
for, split it there.** The `if` and everything it depends on become a
function over values; the I/O stays behind.

Concretely:

- **Name the decision as a type.** `StepState`,
  `RenderPlan::{FromCursor, Everything}`, `StampDecision::{ReuseHash,
  Rehash}`, `Changes`, `StatusView`. A
  decision with a name can be asserted on, logged, and shown in a
  UI; a decision that is control flow can only be run.
- **Pass `now` in.** A core function that needs the time takes it as
  an argument; the shell reads the clock once (`DATALIB_DAG_NOW`
  where a step has it).
- **A value that already carries the bytes should not also write
  them.** `RenderedMarkdown` holds `sections` that concatenate to the
  `.md`; the write belongs in the sink that receives it, not in the
  renderer that built it.
- **A plan before a loop.** When a walk has passes — a forward walk
  from a cursor, a refresh window, a backfill — compute the list of
  passes first, as a value, then loop over it. Slack's
  `export_channel` computes them inline between fetches and is the
  counter-example.
- **Reducers on the frontend.** A handler that turns `(rows, event)`
  into `rows` is a pure function in a `.ts` file with a test, called
  from the component; it is not a method on the component.

### Where not to

Don't push it into the throughput code. `bulk_upsert`, `grid_index`'s
row loop, the blob CAS: there the I/O *is* the function and there is no
decision worth extracting. The pattern's own page names the same
exception — inline mixing where the performance cost of the split is
real — and here it is the only one.

And don't build an effect interpreter for one call site. A core that
returns `Vec<Effect>` earns its keep when the shell is a loop that
handles many events (the supervisor's tick); for a single decision, an
enum the caller matches on is the whole pattern.

### Tests

A core function's tests are synchronous, take values, and read as a
table: this input, that decision. Name them for the rule they pin
(`a_param_change_renders_everything`), not the mechanism. A shell
function's tests are the integration tests the repo already writes —
against a store, a tempdir, a subprocess — and there should be few of
them, because a shell has few branches. If a shell test is asserting
on a decision, the decision is on the wrong side of the line.

The audit that measured the tree against this rule and lists what to
move is [`audits/2026-09-21_fcis.md`](audits/2026-09-21_fcis.md).

## Auditing the tree for drift and repetition

Two things creep in between audits: prose that no longer says what the
code does, and code or prose copied from one place to another. Neither
fails a test, so every so often someone goes looking. This is how.

### Docs against the tree

The reference docs (the ones [`AGENTS.md`](/AGENTS.md)'s doc map lists,
and the `README.md`, `INGEST.md` and `TRANSLATE.md` files beside the
code) say what the tree does now. To check one, take every concrete
claim in it and find the thing it names:

- **Names**: every path, crate, type, function, field, table, column,
  endpoint, env var, CLI flag, config key and step id. `grep -rn` it; a
  name that isn't there is a wrong doc.
- **Counts**: "four providers", "eight checks", "three log tests". Count
  them.
- **Behaviour**: "X is retried", "Y is written only by Z". Read the code
  that does it.
- **Commands**: every command the doc tells someone to run, and every
  bazel target it names (look for it in the `BUILD.bazel`).

Fix what is wrong in place, so the doc says what the tree does. Cut
what describes something that isn't there, unless it is a plan, and
plans go under `docs/dev/plans/`. While you are in the file, cut
history ("used to", "before #NNN", "as of <date>"), since git keeps it.
Where a rule has a home elsewhere, replace the second copy with a link
to it. Don't add a "current as of" banner or a "the code wins"
disclaimer. A doc either says what the tree does or gets fixed.

Split the docs among several agents by area, with no file shared, and
have each one report what it could not verify. The plans and audits
under `docs/dev/plans/` and `docs/dev/audits/` are records, not
reference. Leave them alone unless one links to something you removed.

### Repeated code

jscpd finds exact token-for-token copies. Run it from the repo root:

```sh
npx -y jscpd@4.0.5 datalib scripts tests \
  --format "rust,typescript,javascript,python,bash,markup" \
  --ignore "**/third-party/**,**/node_modules/**,**/*.snap,**/fixtures/**/*.json*" \
  --min-tokens 60 --min-lines 8 --reporters json,console --output /tmp/jscpd
```

The console table gives the duplicated share per language, and
`/tmp/jscpd/jscpd-report.json` lists each clone as two file:line ranges.
At the last count about 2% of lines were copies. Issue #791 is a worked
example: its list of targets was cleared by #802 and #812. jscpd does
not read `.vue` files, and it misses copies where the names were
changed, so read the neighbours of anything it flags.

A clone is a candidate, not a verdict. For each one, decide:

- **Is it the same logic, which must change together?** Then give it
  one home both callers can already reach: the same file, the same
  crate, or a shared crate both already depend on (`chat-common`,
  `timeseries_render`, `forge-*-common`, `agent_sessions*`,
  `datalib_etl_render::processor`, `datalib_id::Minter`,
  `datalib_etl::entity_store`). A new dependency edge is fine only if it
  respects the graph's direction: render depends on ingest, never the
  reverse, and `chat-common` stays render-only.
- **Does it only look alike?** Leave it. Examples: code that differs
  only in each provider's types, table code such as `source_catalog.rs`
  or `grid_rows_builder.rs`, the match arms in `dispatch.rs`, and the
  strum `as_str`/`parse` pair [`AGENTS.md`](/AGENTS.md) prescribes.
- **Is it test setup?** Give it a `support.rs` in that test crate.

### Repeated prose

jscpd compares lines, so a paragraph that was copied and then re-wrapped
gets past it. `scripts/find_repeats.py` compares words instead:

```sh
scripts/find_repeats.py                        # every tracked doc, runs of 14+ words
scripts/find_repeats.py --min 10 --paths docs/dev AGENTS.md
```

Keep each repeat in one home and link to it from the others. A user doc
may keep a sentence it needs to stand alone.
