# Style: how code in this repo is shaped

Reference, current as of 2026-09-21. The short rules — comments,
enums, timestamps, dynamic SQL, fallbacks, unordered collections —
live in [`AGENTS.md`](/AGENTS.md) so that every agent loads them; this
doc holds the one rule that needs more than a paragraph to explain and
to apply. When this doc and the tree disagree, the tree wins; fix the
doc in the same change.

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
- **The supervisor is a pure tick.**
  [`plans/supervisor.md` § 2.3](plans/supervisor.md) replaces the
  runner's loop with one function from values to a list of starts.
  That design only works if the function stays pure; this rule is how
  it stays that way.

### The templates already in the tree

Read one of these before writing a new decision; they are the house
shape.

| core | shell that feeds it | what it decides |
|---|---|---|
| `supervisor::tick::tick(shape, intent, facts, budgets) -> Tick` in `dag/src/supervisor/tick.rs` | the loop in `supervisor/round.rs` | what each step is doing, and what to start, stop and close |
| `RenderPlan::decide(stored, params, version_changed)` in `datalib_step/src/render.rs` | `render_source` | diff from the cursor, or render everything |
| `Adjustments::plan(prev, inputs)` and `select_targets` in `slack/src/ingest/mod.rs` | `fetch` | what a config change means for the walk; which conversations to walk |
| `Scan::changes_since(prev) -> Changes` in `etl/src/fsscan.rs` | `scan` | which files were added, modified, moved, removed |
| `config::check_text(text) -> ConfigCheck` in `dag/src/config.rs` | `load_graded` | what a config means and every problem in it |
| `scope_config::{turned_on, limit_relaxed, filter_widened}` | the provider's `plan` | whether a knob widened |
| `ui/src/config/{rowMenu,sourceSteps,activity,browsePresets}.ts` | the Vue components | which menu entries, which steps, what the activity column shows |

Every one of these has synchronous tests, and the tick's walk a sync
through its states event by event, which is only writable because the
function is pure.

### Where to split

The heuristic: **if a function takes a `&Path`, a pool, a sink, or
`now`, *and* contains a `match` or an `if` a person would want a test
for, split it there.** The `if` and everything it depends on become a
function over values; the I/O stays behind.

Concretely:

- **Name the decision as a type.** `Decision::{Run, Skip, Block}`,
  `RenderPlan::{FromCursor, Everything}`, `Changes`, `StatusView`. A
  decision with a name can be asserted on, logged, and shown in a
  UI; a decision that is control flow can only be run.
- **Pass `now` in.** A core function that needs the time takes it as
  an argument; the shell reads the clock once (`DATALIB_DAG_NOW`
  where a step has it). `mergeJob` in `SourcesCard.ce.vue` reads
  `new Date()` inline and is the counter-example.
- **A value that already carries the bytes should not also write
  them.** `RenderedMarkdown` holds `sections` that concatenate to the
  `.md`; the write belongs in the sink that receives it, not in the
  renderer that built it.
- **A plan before a loop.** When a walk has passes — a forward walk
  from a cursor, a refresh window, a backfill — compute the list of
  passes first, as a value, then loop over it. Slack's
  `export_channel` computes them inline between fetches and is the
  case to fix.
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
