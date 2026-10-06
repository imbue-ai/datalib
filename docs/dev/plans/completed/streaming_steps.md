# Streaming steps: letting a consumer start before its producer finishes

**Status: landed — the build plan that did it is deleted (git has it),
its sink contract now lives in
[`dag/README.md`](../../../../datalib/backend/dag/README.md#what-a-sink-owes-its-consumers),
and the supervisor ([`../supervisor.md`](../supervisor.md)) later took
over its scheduling; kept as the record of the design. The
paragraphs below are the proposal as written (2026-09-03).** Its
doltlite section is cut to the premise; what the engine does now is in
[`doltlite.md`](../../doltlite.md). What is still worth reading here is
the argument: § "The rule that keeps this safe" and § "Why Bazel's
persistent workers are the better model", which
[`../supervisor.md`](../supervisor.md) builds on.

**The build plan** (deleted; `git log -- docs/dev/plans/completed/streaming_steps_plan.md`).
Measuring the tree before writing it overturned two things below. The
last section here recommends starting with `<source>.download →
<source>.render`; the plan starts with `render → grid_index` instead,
because that consumer's content reads are three queries in one shared
file against forty spread across ten provider crates. And the
plan does not build the persistent-worker/stdin channel described under
"What actually has to change" — a streaming pass is an ordinary step
invocation dispatched early, which is Bazel's `local` fallback promoted
to being the only path.

## The idea in one paragraph

This changes what the DAG is *for*. Right now an edge means two things
at once: "B consumes A's output" and "B may assume A is finished." The
second one is what forces the user to stare at a spinner. Splitting
those two meanings apart is the whole change. And now that we know
pinning a consistent view of a database costs nothing, the storage
layer is no longer the thing standing in the way. What's left is the
notification path and per-step offset state.

## Where we are today

`datalib-dag` runs each step as a subprocess. It reads the config, works
out which steps feed which other steps, and runs them in dependency
order.

It is not single-threaded. The scheduler runs four steps at a time by
default (`parallelism` in `scheduler.rs`), so two sources that have
nothing to do with each other already download at the same time. That
part is fine and this proposal doesn't touch it.

The serialization we're talking about is the one *inside* a single
source's chain. `slack.download` writes a store; `slack.render` reads
that store and writes markdown; `grid_index` reads the markdown. Today
each of those waits for the previous one to exit before it starts. If
the download takes ten minutes, nothing appears in the grid for ten
minutes, even though the first useful rows landed in the first ten
seconds.

That wait is not there because anyone designed it in. It falls out of
the fact that an edge carries both meanings at once. We only ever needed
the ordering. We got the "wait for the end" for free, and it turns out
we're paying for it.

## What we thought was in the way, and isn't

The obvious worry is that reading a database somebody else is still
writing gives you garbage. For doltlite it does not have to, and this
was the finding the proposal rested on (measured on 0.50.3 with
[`hack/doltlite_concurrent_reader/`](../../../../hack/doltlite_concurrent_reader/)):

- **A plain `SELECT` can tear.** The rows a writer has written and not
  yet committed live in the file, so a reader in another process could
  see part of one commit mixed with part of a batch still being written.
- **Reading a commit cannot.** `dolt_at_<table>('<hash>')` reads that
  commit and nothing else, writes nothing to the file, and needs nothing
  held open: a pin is just a hash, so it can be passed between processes
  as a string and survives a peer's `dolt_gc()`.
- **Catching up is a diff.** A consumer that re-pins reads only what
  moved, with `dolt_diff_<table>('<from>', '<to>')`.

So the storage side needed nothing new; what was left was the
notification path and per-step offset state. Doltlite has moved since
this was written: a reader now also has a held read transaction and a
detached open of `<file>@<hash>`, and the tree's writers seal on a
branch of their own so a reader on `main` sees only sealed state.
[`doltlite.md`](../../doltlite.md) § "How doltlite behaves" has the
facts as they are now.

## What actually has to change

The storage side is done and needs nothing from us. Four things remain.

### 1. An edge gains a second property

Today an edge is one fact: B's input is A's output. It needs to carry a
second: whether B may start before A has finished.

The property belongs on the **edge**, not on the step. A step can
perfectly well want to stream one of its inputs and require a finished
one for another. Something computing a global total has to wait for
every row; the same step might happily tail a different input. If we
attach the flag to the step, we can't express that.

It also shouldn't live in the config. Whether a step can cope with a
half-written input is a fact about how that step was written, not a
preference the user has. So the step should *announce* it and the runner
should believe it — the same way it already trusts a step's declared
outputs.

### 2. The notification path

A consumer that has already started needs to hear "there's a new commit,
here's the hash."

The nice thing is that we already have most of this. Steps talk to the
runner over stdout with NDJSON, and one of the messages is `outcome`,
which reports a content version per output — and a dolt commit hash is
already the blessed form of that version. **A checkpoint is just an
`outcome` sent early, and sent more than once.**

Going the other way, toward a running consumer, we need a channel we
don't currently use. We have one: `subprocess.rs` sets child stdin to
`Stdio::null()`. So the runner would write NDJSON down to the child, the
child already writes NDJSON up to the runner, and the protocol becomes
symmetric.

### 3. Per-step offset state

A consumer has to remember the last commit it fully processed, so that
after a crash or between runs it knows where to resume.

This needs no new protocol at all. `step_protocol.md` already says that
resume cursors and bookkeeping are private to the step and belong under
its own output tree. An offset is exactly that. A step that wants one
writes it where it already writes everything else.

### 4. The consumer contract

On start, pin to the newest commit and work from `dolt_at_`. When a
checkpoint arrives, finish the current unit, then re-pin and process the
delta with `dolt_diff_`. Record the new offset once the unit is durable.

A consumer that ignores checkpoints entirely still has to be correct. It
just does one pass over one pin, which is what every step does today.
That keeps "any executable can be a step" true.

## The rule that keeps this safe

**A missed notification must make a consumer slow, not wrong.**

This is the one invariant to protect, and it's worth understanding why
we get it almost for free.

Kafka is a log of *events*: "this row changed." If a consumer misses
one, it has no way to reconstruct what it missed, so its state is now
permanently wrong. Missing a message is a correctness bug.

Dolt is a log of *states*: each commit is the whole table as it stood.
A consumer that misses a checkpoint can always fall back on "re-read
everything at my current pin" and be exactly right, just slower. It can
also skip several checkpoints and catch up with a single diff across the
whole gap.

That's a genuinely strong property and it's easy to throw away by
accident. It survives as long as a checkpoint is only ever a hint that
lets a consumer do less work. The moment a consumer *needs* the
notifications to be correct — because it's accumulating state that can't
be rebuilt from a pin — we've built Kafka's worst failure mode into our
own storage engine, and we won't find out until something crashes at an
awkward moment.

## Why this isn't Kafka

Setting Kafka up here would mean running a second durable system to get
a replayable ordered log, when the commit graph already is one.

The comparison isn't close, either. Kafka's offsets are opaque integers
per partition. Ours would be commit hashes: content-addressed, so they
verify themselves, and already the exact thing the scheduler compares to
decide whether a step needs to re-run. We'd be reusing a concept rather
than introducing one.

What Kafka is actually good at — fan-out to many consumers across
machines, retention policies, backpressure, partition rebalancing — is
all stuff we don't need on one laptop with one user and a handful of
steps.

The deeper reason to stay away is about where complexity ends up. Kafka's
characteristic failure is that pipeline topology becomes an operations
problem: something you configure, tune, and debug separately from the
code that does the work. The DAG should stay declarative, and the
notification path should stay an implementation detail nobody configures.

## Why Bazel's persistent workers are the better model

Bazel had our problem, in different clothes. Starting a JVM for every
compile action cost more than the compile did. Their fix was to keep the
process alive and feed it work.

The mechanics are close to what we'd build. Bazel appends
`--persistent_worker` to the tool's command line. The tool then loops:
read a `WorkRequest` from **stdin**, do the work, write a `WorkResponse`
to **stdout**. Either newline-delimited JSON or length-delimited
protobuf, and the rule says which.

`WorkRequest` carries `arguments`, `inputs` (each with a `path` and a
`digest`), `request_id`, `cancel`, `verbosity`, and `sandbox_dir`.
`WorkResponse` carries `exit_code`, `output`, `request_id`, and
`was_cancelled`.

Four pieces of that map onto things we already have:

- **stdin as the request channel** is precisely the unused channel
  described above. We'd be completing a loop we've half-built.
- **`inputs[].digest`** is how a worker checks whether its cached state
  is still valid. Our version of that is the commit hash, and ours is
  better — content-addressed, and already meaningful to the scheduler.
- **`cancel` / `was_cancelled`** — we already have cancellation in the
  step protocol.
- **`request_id`** distinguishes concurrent requests when one process
  handles several at once.

Two design decisions of theirs are worth copying outright.

**Support is a capability the rule declares**, via
`execution_requirements = {"supports-workers": "1"}`, not something the
user turns on. That's the same conclusion we reached independently about
where the streaming flag belongs.

**The strategy has a fallback built in**: `--strategy=Mnemonic=worker,local`
means "use a worker, and if that doesn't work out, just run it the
ordinary way." Bazel never lets the fast path become load-bearing. That
is the same instinct as our rule about missed notifications, expressed in
their vocabulary.

And one warning of theirs to take seriously. Bazel's docs are blunt that
a long-lived tool "may leak information between requests internally, for
instance through a cache" — which is why `--worker_sandboxing` exists.
Our version of that hazard is specific and easy to walk into: a
long-lived consumer holding a pinned connection *is* carrying state
between requests. The pin that makes chunk N consistent is the same
object that will quietly serve stale data for chunk N+1 if nobody
re-pins. Re-pinning should be structural — something the framework does
between units — rather than something each step author has to remember.

Docs: [Persistent workers](https://bazel.build/remote/persistent),
[Multiplex workers](https://bazel.build/remote/multiplex), and the
message definitions in
[`worker_protocol.proto`](https://github.com/bazelbuild/bazel/blob/master/src/main/protobuf/worker_protocol.proto).

## What this buys, honestly

It buys **latency, not throughput**. The machine does the same total
work. What changes is when the first useful result shows up.

That is worth being clear-eyed about, because there's a real limit on
it. The scheduler already runs four steps at once, so independent
sources already overlap. Streaming only compresses the chain *within* a
source. If you're syncing ten sources, the machine is probably busy
already and the wall-clock saving is small.

The argument for doing it anyway is about what the user sees. Rows
appearing in the grid while a download is still running is a different
product from a progress bar followed by everything at once — even when
the two finish at the same moment. For a single-user desktop app that
perceived latency largely *is* the product.

The cost is per-step, and it's the expensive part. Every streaming
consumer becomes a resumable incremental processor with durable offset
state and a re-pinning loop. That's real work, and it has to be done
once per step that opts in.

Which points at how to start: **do this for one edge, end to end, and
learn from it.** `<source>.download → <source>.render` is the right
first one. It's the longest wait, the benefit is the most visible, and
render is already incremental so it has somewhere to put an offset. Get
that working before touching `render → grid_index`, and before building
anything that looks like a general streaming framework.

## Open questions

- **How does a step announce that an edge can stream?** Bazel uses a
  static declaration on the rule. Our steps are arbitrary executables,
  so the options are a manifest, a `--capabilities` probe the runner
  calls once, or a field in the first `outcome`. Undecided.
- **How often should a producer checkpoint?** Too often and we thrash
  consumers; too rarely and we're back to the spinner. Probably a
  per-step call, but a sensible default matters. *Measured since:* a
  commit is ~20 ms and pins only the pages its transaction rewrote, so
  the store-side cost depends on the key, not the cadence — nothing for
  time-clustered keys, a copy of every touched leaf for scattered ones
  (`datalib/backend/etl/README.md` § "What a write costs"). The
  remaining cost is the consumers woken, which `Checkpointer` already
  rate-limits.
- **What does the UI show** when a step is running and its consumer is
  running too? The task board currently assumes a step is either waiting
  or working.
