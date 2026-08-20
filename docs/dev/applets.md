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
| Declares | `command`, `inputs`, `outputs`, `params` | `command`, `title`, `params` |

An applet declares no `inputs`/`outputs` because it is never
scheduled and owns no artifacts: it reads what steps already wrote.

## The config entry

```toml
[[applets]]
id = "slack_work"
title = "Work Slack"
command = "datalib-view-slack"
[applets.params]
tree = "slack/rendered_md"
```

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
| `DATALIB_APPLET_BASE` | `/v/<id>/`, the prefix the gateway proxies here. An applet that emits absolute URLs must build them from this rather than assuming the mount layout |

`id` must be a valid JavaScript identifier. It is both the mount
prefix (`/v/<id>/`) and a name injected into card source, and card
source is evaluated with `new Function`, so a dotted or digit-leading
id would be a syntax error at render time rather than at config load.

`config::validate_applets` checks this (and id uniqueness) when the
gateway builds its registry. Note what it does *not* do: the DAG
runner's `config::load` does not call it, so an invalid applet id does
not stop a sync. And a rejection currently drops the **whole** applet
list with a message on stderr rather than the one bad entry — the
server keeps booting on purpose, because refusing to start would take
search and Setup down with it and leave no way to fix the file.

## What a command has to do

**1. Write its frontend namespace.**

```
<command> --write-frontend-dir <root>/system/frontend/<id> [--params <json>]
```

Write the files described in [the frontend store](#the-frontend-store)
into that directory and exit. Nothing is read from stdout; stderr is
the log, and its tail becomes the error message on a non-zero exit.

The directory's last segment is the namespace, and it is the only
channel by which a command learns which instance it is. Two instances
of one binary differ only in configuration, so the argument a gallery
entry passes — usually the instance's own id — has to come from
outside.

**2. Serve on a port.** `-p <port>` binds `127.0.0.1:<port>`. The
gateway proxies `/v/<id>/<path>` to `<path>` on that port.

That is the whole contract. There is no protocol version, no
handshake, and no registration call, so a shell script is a viable
applet.

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

## Refresh is destructive, and `user` is reserved

A refresh deletes every namespace directory except `user`, then asks
each configured applet to write its own. That is what keeps the store
honest: an applet removed from `config.toml` takes its components with
it, and a component removed from an applet's output actually
disappears.

`user` is never touched, because nothing regenerates it — which is
exactly why an applet may not take that id. The config loader rejects
it (`datalib_dag::config::RESERVED_APPLET_ID`); an applet allowed to
claim `user` would have the user's own work deleted on the next
refresh.

## Why the write is a flag rather than an endpoint

Components have to be readable before any applet is worth running: the
gallery lists them, card source resolves against them, and the browser
imports their code. Making the write a flag means all of that comes off
the filesystem, so opening the app costs zero applet processes — a
server starts only when a card actually asks one for data.

## Authentication

Every route is behind the per-process API token (`datalib/backend/http/src/auth.rs`),
and the applet routes are no exception: the gate is an outermost layer,
so `/api/frontend`, `/modules/<hash>` and `/v/<id>/…` all inherit it.

Nothing in a component has to carry the token. The browser holds it as
a same-origin cookie, which it attaches to the component's own
`fetch("/v/<id>/…")` and to the `import("/modules/<hash>")` that loaded
it. An applet author therefore writes no auth code — but the corollary
is that a component may only reach the gateway from the page's own
origin. Fetching a `/v/` URL from an iframe on another origin, or from
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

- **`config.toml` moved** → re-run every applet's write, then rescan.
- **the store's own files moved** → rescan only.

Conflating them would make a `PUT /api/lib` wipe and rewrite every
applet namespace. Both checks are `stat`-only when nothing changed, so
they can sit on the endpoint the UI polls — which is what turns a saved
config, or a file dropped in by hand, into a live gallery update
without a restart.

An applet whose config entry changed is also stopped as part of the
refresh, so the next request respawns it with the new `params`.

## Failure

An applet whose write fails is named in `/api/frontend`'s
`applet_errors`, with the child's stderr: its namespace is absent or
stale, and a configured applet that silently vanished would look like a
config that never saved. Files the store *could* read but not use — a
`.js` whose name does not match its bytes, metadata naming a component
that is not there — are reported per namespace in `problems`, for the
same reason. A request
to an applet that will not start answers `502` with a JSON `error`
carrying the child's last stderr lines, so the card shows the reason
instead of an empty state. That failure is remembered: without it,
every subsequent request would pay the ten-second readiness timeout
again and leave another dead child behind.

A `--write-frontend-dir` run is bounded at 30 seconds. The bound exists
because a command that starts *serving* when asked to *write* is an
easy mistake for a binary that has both modes, and a refresh happens
during boot after the listener is already bound — so without a timeout
the symptom would be a browser tab whose requests queue forever with
nothing logged.

## Reference implementation

`datalib/backend/applets/slack` — `datalib-view-slack`. It reads the
cross-provider `.grid_rows.json` sidecar contract as untyped JSON
rather than linking the schema or provider crates, which keeps an
applet a small program. It ships in `//datalib/backend:dist` alongside
`datalib-step`, so a config can name it bare. The store is
`datalib/backend/http/src/frontend.rs` (which knows nothing about
applets) and the calling side is `datalib/backend/http/src/applets.rs`.

Its two-level shape is worth copying: Slack renders one document per
*thread*, and every message in a thread carries that thread's
`markdown_uuid`. So `/channels` lists channels with thread and message
counts, `/threads?channel=…` lists one channel's threads, and only a
thread maps to a document the card opens. An earlier version went
straight from channel to document, which picked one arbitrary thread
and made a 45-message channel look like it held a single message —
the general lesson being to check what a `markdown_uuid` actually
identifies before treating it as "the document for this thing".
