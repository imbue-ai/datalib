# GitHub Extract

`github-ingest` mirrors GitHub pull requests and their conversation
from `api.github.com` into one doltlite raw store,
`<out>/entities.doltlite_db` (`<data_root>/<source_id>/ingest/` under a
sync). Every table keeps GitHub's payload untouched; the schema is in
`src/ingest/schema_raw.rs`.

| table | endpoint |
|---|---|
| `self_identity` | `/user` |
| `pull_requests` | `/repos/{repo}/pulls/{n}` |
| `issue_comments` | `/repos/{repo}/issues/{n}/comments` |
| `pr_reviews` | `/repos/{repo}/pulls/{n}/reviews` |
| `pr_review_comments` | `/repos/{repo}/pulls/{n}/comments` |

The sync loop (discovery, cursors, skip-if-unchanged, child pruning) is
shared with GitLab in `datalib/backend/etl/forge-ingest-common/`; this
crate supplies the endpoints.

## Auth

One latchkey service: `github` (Bearer token, e.g. a fine-grained PAT
with read access to pull requests on the target repos). Latchkey injects
the `Authorization` header; this crate doesn't touch credentials.

## Discovery scopes

Each `--scope` (default: `author:@me`, `commenter:@me`, `mentions:@me`)
goes through the search-issues API as `is:pr <scope> updated:>=<since>`.
The union of the results is what gets fetched. `mentions:@me` is the
cheap way to catch incoming review pings on PRs the user otherwise
wouldn't touch.

## Incremental sync

Each scope's cursor is the time its last successful search ran, in the
store's `sync_scope_state` table. The next run searches from that
cursor. A scope with no cursor searches from `now -
refresh_window_days` (default 30; `0` is unbounded). Widening
`refresh_window_days` pulls the cursor back to the new floor once
(`datalib_etl::scope_config`).

A listed PR whose `updated_at` matches the stored one is skipped. A
fetched PR's comment and review lists are each read whole, and a
stored child the list no longer names is deleted — a removed comment,
a resolved review thread. A list whose walk failed prunes nothing.

`--full`, or an empty store, searches with no date bound and refetches
every listed PR.

## Single-PR mode

`--pull-request owner/repo#NUM` (also `owner/repo/pull/NUM` or a
github.com PR URL; repeatable) skips discovery entirely. The config's
`api.pull_requests` list does the same.

## Config

The `api` block of a `github` source (`github_config`):
`refresh_window_days`, `max_prs` (a safety cap), `pull_requests`.
`latchkey_settings` picks which stored account to use.

## Run it

```sh
bazelisk build //third-party/latchkey-curl-shims
export LATCHKEY_CURL=$PWD/bazel-bin/third-party/latchkey-curl-shims/latchkey-curl-router
bazelisk run //datalib/backend/etl/providers/github:github_ingest -- \
    --out /tmp/github-mirror \
    --pull-request <owner>/<repo>#<num>
```
