# GitHub Extract

`github-download` mirrors GitHub pull requests + their conversation via
`api.github.com`. Output is the same event-store JSONL layout used by
the rest of the workspace — one append-only stream per entity, keyed by
stable GitHub IDs:

```
<out>/
  self_identity/{created,updated}/events.jsonl     # /user
  pull_request/{created,updated}/events.jsonl      # /repos/.../pulls/{n}
  issue_comment/{created,updated}/events.jsonl     # /repos/.../issues/{n}/comments
  pr_review/{created,updated}/events.jsonl         # /repos/.../pulls/{n}/reviews
  pr_review_comment/{created,updated}/events.jsonl # /repos/.../pulls/{n}/comments
  sync_state.json                                  # per-scope last-seen-at
```

Each line is `{ "key": {...}, "raw": {...}, "ts": "<iso8601>" }`. `key`
holds the denormalized stable identity used for change detection; `raw`
carries the untouched GitHub payload for the translate stage.

## Auth

One latchkey service: `github` (Bearer token, e.g. a fine-grained PAT
with `pull_request: read` on the target repos). Latchkey injects the
`Authorization` header; this crate doesn't touch credentials.

## Discovery scopes

`github-download` runs each `--scope` (default: `author:@me`,
`commenter:@me`, `mentions:@me`) through the search-issues API as
`is:pr <scope> updated:>=<since>`. Union of results is what gets
fetched. `mentions:@me` is the cheap way to catch incoming review pings
on PRs the user otherwise wouldn't touch.

## Incremental sync

`<out>/sync_state.json` carries the last successful run time per scope.
On rerun, the `since` for each scope is `min(state[scope], now - refresh_window_days)`
— meaning a tight stored timestamp never narrows tighter than the safety
window, which protects against edits to old PRs that wouldn't otherwise
show up in a `last_seen_at`-only filter.

`--full` skips sync state and walks the full refresh window. An empty
out_dir implicitly forces a full backfill on first run.

## Single-PR mode

`--pull-request owner/repo#NUM` (or a `https://github.com/.../pull/NUM`
URL) skips discovery entirely. Useful for smoke tests and the live
snapshot test.

## Run it

```sh
export LATCHKEY_CURL=$PWD/target/debug/latchkey-curl-shim   # for parity with other providers
cargo run -p frankweiler-etl-github --bin github-download -- \
    --out /tmp/github-mirror \
    --pull-request imbue-ai/mngr#1650
```
