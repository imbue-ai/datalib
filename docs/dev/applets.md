# Writing an applet

An **applet** is a config-declared program that contributes a piece of
the app: card components for the frontend, and the HTTP endpoints
those components read. It is the second kind of entry in
`config.toml`, alongside `steps`.

| | step | applet |
| --- | --- | --- |
| Lifetime | runs to completion during a sync | long-lived server, spawned on demand |
| Produces | artifacts on disk | HTTP responses and card components |
| Scheduled by | `datalib-dag`, from artifact dependencies | the http gateway, from an incoming request |
| Declares | `command`, `inputs`, `outputs`, `params` | `command`, `params` |

An applet declares no `inputs`/`outputs` because it is never
scheduled and owns no artifacts: it reads what steps already wrote.

## The config entry

```toml
[[applets]]
id = "slack_work"
command = "datalib-applet slack"
[applets.params]
tree = "slack/rendered_md"
```

There is no `title` key, and an unknown key is rejected by name rather
than ignored. The label the gallery shows is written by the applet
into its own namespace metadata, so a config-level title would be a
second spelling of a label that lives elsewhere — one the gallery
never reads. An applet that wants a configurable label takes it
through `params`, the way the slack applet takes `workspace`.

`command` is split shell-style (the same `shlex` call the runner uses)
and resolved through `binary_dir`, then `~/.datalib/bin`, then the
inherited `PATH` — the same order a step's command resolves in, so a
program installed in `~/.datalib/bin` works for either kind of entry.
`params` is forwarded as
`--params <json>`, the working directory is the data root, and `env` is
merged into the child — all as for a step, so there is one set of
rules.

The child also gets three variables:

| variable | value |
| --- | --- |
| `DATALIB_DAG_DATA_ROOT` | absolute path of the data root (also the cwd) — the step protocol's spelling, reused deliberately |
| `DATALIB_APPLET_ID` | this instance's config id, the same value `--applet-id` carries |
| `DATALIB_APPLET_BASE` | `/applet/<id>/`, the prefix the gateway proxies here. An applet that emits absolute URLs must build them from this rather than assuming the mount layout |

`id` must be a valid JavaScript identifier. It is both the mount
prefix (`/applet/<id>/`) and a name injected into card source, and card
source is evaluated with `new Function`, so a dotted or digit-leading
id would be a syntax error at render time rather than at config load.

`config::validate_applets` checks this (and id uniqueness) when the
gateway builds its registry. Note what it does *not* do: the DAG
runner's `config::load` does not call it, so an invalid applet id does
not stop a sync. And a rejection currently drops the **whole** applet
list with a message on stderr rather than the one bad entry — the
server keeps booting on purpose, because refusing to start would leave
no way to fix the file. That is worth more now than when it was
written: search is itself an applet, so a bad entry anywhere in the
list costs you the grid, and booting anyway is what keeps the Setup tab
reachable to repair it.

## What a command has to do

One invocation, one process:

```
<command> -p 0 --frontend-dir <root>/system/frontend/<id> [--params <json>]
```

**Write the directory, bind a port, then print
`DATALIB_APPLET_PORT=<port>` to stdout** — in that order. The gateway
waits for that line and then scans the store, so the line is its signal
that the write finished. An applet that announced first would race the
scan and intermittently come up with no components.

`-p 0` means "any port": the OS picks and the announcement reports
which. The port travels child-to-gateway, not the other way, and that
direction is load-bearing. A port the gateway picked would have to be
bound here, released, and then raced for by the child — and the only
readiness question left to ask would be "is anything accepting on that
port?", which whoever else won the race answers just as convincingly.
That is not hypothetical: under a loaded `bazelisk test //...` the
gateway adopted a stranger's listener, scanned the store before its own
applet had written a byte, and served an empty gallery with no error
while the real child died of `EADDRINUSE`.

The directory's last segment is the namespace, and it is the only
channel by which a command learns which instance it is. Two instances
of one binary differ only in configuration, so the argument a gallery
entry passes — usually the instance's own id — has to come from
outside.

Everything on stdout other than that one line is ignored. stderr is the
log: the gateway forwards it line by line and keeps the tail, which
becomes the error message if the applet never announces.

**stdin is a liveness pipe, not an input channel.** Nothing is ever
sent through it. The gateway holds the write end for exactly as long as
it runs, so when it stops — cleanly, on a signal, or on a SIGKILL that
runs no code at all — the kernel closes that end and the applet's read
hits EOF. An applet that sees EOF on stdin should exit.

The gateway sets `DATALIB_APPLET_PARENT_PIPE=1` to say that stdin means
this. Without it, treat stdin as ordinary: an applet run by hand has a
terminal there, or `/dev/null`, and reading it would swallow input or
take an instant EOF as bad news.

Honouring this is what stops an applet outliving the gateway. It is not
merely tidy: an orphan keeps its port and its data root open, nothing
ever reaps it, and they accumulate — a machine that had been running
the app and its tests for a week was holding 186 of them (#238). The
gateway kills its applets on every exit it can still run code for; this
is the one path where it cannot, so the applet has to notice by itself.

One trap if you write the exit path yourself: by the time EOF arrives,
stderr is a pipe to a process that no longer exists, so writing to it
takes `EPIPE`. `eprintln!` *panics* on a failed write, and a panic in
the watching thread leaves the process running — the leak you were
fixing, one line from the exit that fixes it. Write best-effort
(`let _ = writeln!(…)`), the way `announce_port` does.

That is the whole contract. There is no protocol version, no handshake,
and no registration call.

## Applets are started eagerly and kept running

Every configured applet starts at boot and stays up. Starting one on
its first request cannot work now that the write and the serve are one
invocation: components would only exist once something had already
opened a card that used them, which is what the gallery needs them for.

So a data root with twelve applets runs twelve processes. Idle shutdown
would trade some of that back and is not built; if it arrives, a
restarted applet simply rewrites the same files, since the write is
idempotent.

Starts run in parallel, so boot is bounded by the slowest applet rather
than their sum, and each is capped at 20 seconds. The cap matters
because this happens after the HTTP listener is already accepting:
without it, one hanging applet would leave a browser tab whose requests
queue forever with nothing logged. An applet that *exits* costs nothing
like that long — closing stdout without announcing is end-of-story, and
the gateway says so immediately.

## The frontend store

Every custom component lives under `<root>/system/frontend/`, one
directory per **namespace**:

```
system/frontend/
  user/                    components a person or an agent wrote
    9f2a1c….js             a component, named by the sha256 of its bytes
    tetris.json            metadata: what `comp.user.tetris` is
  slack_work/              written by the slack_work applet
    7ae808….js
    channels.json
```

Two kinds of file, and that is all:

| File | Meaning |
| --- | --- |
| `<sha256>.js` | An ES module whose default export is the component factory. The server re-hashes it and skips the file if the name does not match its contents. |
| `<name>.json` | Either `{title, description, component_hash, component_args}` or `{renamed_to}`. |

Each component document does two things: it defines
`comp.<namespace>.<name>` in the app, resolved by loading the module at
`component_hash`; and it registers a gallery entry whose card source is
that qualified name called with `component_args` spelled as JSON
literals — so `["slack_work"]` yields
`comp.slack_work.channels("slack_work")`.

**There is one mechanism.** Nothing that reads this store knows what an
applet is. An applet's only privilege is being *called* to write a
directory; the files it leaves are scanned, hash-validated and served
exactly like ones a user dropped in by hand. Writing the two files
yourself into `system/frontend/user/` defines a component with no
applet and no config entry involved.

### Why the name is a hash and the metadata is separate

Addressing code by content buys two things. The browser keeps one
module instance per resolved URL, so byte-identical components in two
namespaces resolve to the same `/modules/<hash>` and are evaluated
once. And editing a component becomes an ordinary write of a new file
plus a one-line metadata update — the old bytes stay addressable for
anything mid-render, and the URL changes, which is the only way a
module registry that never evicts will re-evaluate anything.

The `.js` therefore has to stay byte-identical to what the browser
evaluates, which is why title, description and arguments live in a
sibling `<name>.json` rather than as frontmatter.

## Starting an applet is destructive to its namespace, and `user` is reserved

An applet about to start has its namespace directory deleted first, and
rewrites it as it comes up. Every namespace belonging to no configured
applet is deleted too. That is what keeps the store honest: an applet
removed from `config.toml` takes its components with it, and a
component removed from a restarting applet's output actually
disappears.

An applet that keeps running across a config reload (see [When the
store is re-read](#when-the-store-is-re-read)) keeps its directory
untouched. It would rewrite the same bytes anyway — the write is
idempotent for unchanged config — so deleting it would only open a
window where the gallery could scan a namespace that is missing.

`user` is never touched, because nothing regenerates it — which is
exactly why an applet may not take that id. The config loader rejects
it (`datalib_dag::config::RESERVED_APPLET_ID`); an applet allowed to
claim `user` would have the user's own work deleted on the next
refresh.

## Why components come from a directory, not an endpoint

The gallery lists components, card source resolves against them, and
the browser imports their code — all of which has to work before
anything knows to ask a particular applet for anything. Reading them
off the filesystem is what makes that possible; an endpoint would mean
you could not list a component until something had already opened it.

## Authentication

Every route is behind the per-process API token (`datalib/backend/http/src/auth.rs`),
and the applet routes are no exception: the gate is an outermost layer,
so `/api/frontend`, `/modules/<hash>` and `/applet/<id>/…` all inherit it.

Nothing in a component has to carry the token. The browser holds it as
a same-origin cookie, which it attaches to the component's own
`fetch("/applet/<id>/…")` and to the `import("/modules/<hash>")` that loaded
it. An applet author therefore writes no auth code — but the corollary
is that a component may only reach the gateway from the page's own
origin. Fetching a `/applet/` URL from an iframe on another origin, or from
outside the browser without a token, gets a `401`.

The applet's own server needs no token logic either. It binds loopback
and is reached only through the gateway, which is already past the
gate; the `DATALIB_APPLET_TOKEN` it receives is a separate, much weaker
guard against a stray local process (see the runtime contract above).

## Two instances of one command

The case the design is built around:

- Both write **byte-identical component code**, so the store holds one
  address and the browser evaluates it once.
- Both write **different `component_args`**, each naming its own
  instance, so the gallery shows two rows that call the same component
  with different arguments.
- If the two are on **drifted builds**, they write different bytes,
  get different hashes, and stop sharing. That is correct rather than a
  special case, and nothing has to detect it.

## When the store is re-read

At server start, and again whenever it changes. Two triggers, kept
separate because they cost different amounts:

- **`config.toml` moved** → reconcile the running applets against the
  new list, then rescan.
- **the store's own files moved** → rescan only.

Conflating them would make a `PUT /api/lib` restart every applet. Both
checks are `stat`-only when nothing changed, so they sit on
`GET /api/frontend` itself — which is what turns a saved config, or a
file dropped in by hand, into a live gallery update without a restart.

**That reconcile is lazy, and the ordering matters.** A new applet
writes nothing into `system/frontend/` until it has been *started*, and
it is only started by this reconcile. So the client cannot wait for
`system/frontend/` to move before asking: it has to refetch on
`config_changed` as well as on `frontend_changed`
(`ui/src/cards/frontendRegistry.ts`). While the UI polled this endpoint
every four seconds the ordering was invisible — something always asked
again soon enough. See `backend/http/src/watch.rs` for the push channel
that replaced the poll.

**Reconciling** compares the new applet list against the one the
gateway last started, entry by entry:

| The entry | What happens |
| --- | --- |
| unchanged, and its process is alive | left alone — not stopped, not restarted, namespace untouched |
| unchanged, but its process has died | stopped (reaped) and started again |
| changed in any field | stopped and started again, namespace rebuilt |
| new | started, namespace rebuilt |
| gone from the config | stopped, namespace deleted |

A changed entry has to restart because an applet writes its components
as it starts, so its output cannot follow the edit otherwise. The
comparison is over the whole entry rather than a chosen few fields, so
no field can quietly become load-bearing without the restart following
it.

The config's `binary_dir` counts too: it is compared alongside the
entries, and a change to it restarts everything, since the same
`command` can resolve to a different program underneath it.

An applet whose process died is restarted by the same rule, which also
means a config edit retries one that failed to start last time.

## Failure

An applet whose write fails is named in `/api/frontend`'s
`applet_errors`, with the child's stderr: its namespace is absent or
stale, and a configured applet that silently vanished would look like a
config that never saved. Files the store *could* read but not use — a
`.js` whose name does not match its bytes, metadata naming a component
that is not there — are reported per namespace in `problems`, for the
same reason.

A request to an applet that is configured but not running answers
`502` with a JSON `error` naming it — a different message from one
that is not configured at all, since the two want different fixes.

The gateway forwards an applet's stderr line by line as it arrives and
keeps the tail, rather than reading the pipe to EOF when the applet
fails. That matters because the pipe is held by the child *and* by
anything it spawned: a wrapper script whose own child is still alive
would otherwise block the start path until that grandchild exited. An
applet that exits without announcing is the one case where the tail is
worth waiting for — its last words are the reason — so the start path
waits for that reader to finish there, and only there, bounded by two
seconds for the same grandchild reason.

## An applet that contributes no components

`unified_index` is the other shape the contract allows and had no
instance of until now: a server that contributes **endpoints only**. The
grid, the document view and the document picker are builtins in the app
bundle, so there is nothing to write into a namespace — the gateway
still passes `--frontend-dir`, and the applet ignores it.

It is also the applet the app cannot run without. A data root whose
config does not declare it has no search, which is why the scaffold, the
config examples and `datalib-migrate-config`'s output all write it. The
UI calls it directly at `/applet/unified_index/…`; there is no `/api/`
alias, and `datalib-http` does not know those routes exist.

That is what makes the separation checkable rather than aspirational:

```sh
bazel query 'somepath(//datalib/backend/http:datalib_http, \
                      //datalib/backend/unified_index:datalib_unified_index)'
```

comes back empty. The index crate is linked by `datalib-step`, which
writes the indexes, and by `datalib-applet`, which serves them.

## Reference implementation

`datalib/backend/applets` — `datalib-applet`, one subcommand per
applet (today just `slack`), the same shape as `datalib-step`. One
binary rather than one per applet keeps the shared machinery in one
place and ships one file instead of a growing list; adding an applet is
a subcommand plus a module, not a new crate and five packaging edits.

The Slack applet reads the
cross-provider `.grid_rows.json` sidecar contract as untyped JSON
rather than linking the schema or provider crates, which keeps an
applet a small program. It ships in `//datalib/backend:dist` alongside
`datalib-step`, so a config can name it bare. The store is
`datalib/backend/http/src/frontend.rs` (which knows nothing about
applets) and the calling side is `datalib/backend/http/src/applets.rs`.

Its three-level shape mirrors the Slack app, and follows from how the
data is rendered: one document per *thread*, with every message in it
carrying that thread's `markdown_uuid` and a `message_index`.

| Level | Where it lives | What it shows |
| --- | --- | --- |
| channels | `/channels` | every channel, with thread and message counts |
| one channel | `/channel?name=…` | each thread's **opening message** (index 0), replies collapsed behind a "N replies" link |
| one thread | `documentView` | the whole conversation |

The third level is deliberately not an endpoint. The thread document is
real rendered markdown with formatting, media and edges, so the card
opens it with the builtin document view rather than reimplementing all
of that badly. The first two navigate in place, so a workspace never
costs more than one column until you open a thread.

An earlier version went straight from channel to document, which picked
one arbitrary thread and made a 45-message channel look like it held a
single message — the general lesson being to check what a
`markdown_uuid` actually identifies before treating it as "the document
for this thing".
