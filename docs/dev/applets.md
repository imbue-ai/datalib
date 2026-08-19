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

**1. Print a frontend manifest.**

```
<command> --frontend-manifest --applet-id <id> --module-dir <dir> [--params <json>]
```

Write each component's ES module into `<dir>`, named after the sha256
of its own bytes, then print on stdout:

```json
{
  "components": [{ "name": "channels", "module": "7ae808…" }],
  "gallery": [{
    "source": "slack_work.channels(\"slack_work\")",
    "title": "Slack — Work",
    "description": "Browse the channels mirrored into Work."
  }]
}
```

Exit 0. Nothing is served during this — it runs to completion.

**2. Serve on a port.** `-p <port>` binds `127.0.0.1:<port>`. The
gateway proxies `/v/<id>/<path>` to `<path>` on that port.

That is the whole contract. There is no protocol version, no
handshake, and no registration call, so a shell script is a viable
applet.

## Why the manifest is a flag rather than an endpoint

Three things need it before any applet is worth running: the component
gallery, the registry that resolves a name in card source, and the
module URLs the browser imports from. A flag answers all three for one
`exec`, so opening the app costs zero applet processes — a server
starts only when a card actually asks one for data.

## Why the command is told its own id

Gallery entries are **full card-source snippets**, not names. Two
instances of one command differ only in configuration, so a snippet
that has to address its own instance
(`slack_work.channels("slack_work")`) needs information the binary
cannot know about itself.

This is also why component names are not global. The applet id is the
namespace and the component name is a member of it, so a name only has
to be unique inside one manifest — which one author controls. Two
applets cannot collide, so nothing arbitrates a collision.

## Two instances of one command

The case the design is built around:

- Both report the **same module hash**, because the bytes are the
  same. The gateway stores one file; the browser keeps one module
  instance per URL and therefore evaluates the component once and
  binds it twice.
- Both report **different gallery snippets**, each naming its own
  instance.
- If the two are on **drifted builds**, they report different hashes,
  get different code, and stop sharing. That is correct rather than a
  special case, and nothing has to detect it.

## The module store

Modules live in one flat, content-addressed directory at
`<root>/system/modules/<sha256>`, served at `/modules/<sha256>` with
immutable cache headers. Consequences worth knowing:

- **Relative imports do not work.** A flat store has no directory
  structure, so `import "./util.js"` would resolve to
  `/modules/util.js`. Ship each component as one self-contained
  module, or rewrite its imports to `/modules/<hash>` at build time.
- **The digest must move when the bytes move.** It is the cache key,
  the URL, and the module identity at once; a stale digest serves old
  code from an immutable URL that nothing can invalidate. The gateway
  re-hashes every file a manifest points at and refuses a mismatch.
- **The store is a derived cache.** It is marked as one for backups,
  and it is safe to delete — the next boot rebuilds it.

## When discovery runs

Once, at server start. Nothing re-runs it when `config.toml` changes,
so adding an applet needs a restart today — the UI polls
`/api/applets`, but the answer cannot change under it. Re-running
discovery on `PUT /api/config` is the obvious next step and the polling
is already in place for it.

## Failure

An applet that fails discovery still appears in `/api/applets`,
carrying its error and no components: a configured applet that
silently vanished would look like a config that never saved. A request
to an applet that will not start answers `502` with a JSON `error`
carrying the child's last stderr lines, so the card shows the reason
instead of an empty state. That failure is remembered: without it,
every subsequent request would pay the ten-second readiness timeout
again and leave another dead child behind.

A `--frontend-manifest` run is bounded at 30 seconds. The bound exists
because a command that starts *serving* when asked to *describe* is an
easy mistake for a binary that has both modes, and discovery happens
during boot after the listener is already bound — so without a timeout
the symptom would be a browser tab whose requests queue forever with
nothing logged.

## Reference implementation

`datalib/backend/applets/slack` — `datalib-view-slack`. It reads the
cross-provider `.grid_rows.json` sidecar contract as untyped JSON
rather than linking the schema or provider crates, which keeps an
applet a small program. The gateway side is
`datalib/backend/http/src/applets.rs`.
