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

The sync loop (discovery, cursors, skip-if-unchanged, child pruning,
what a failure costs) is shared with GitLab in
`datalib/backend/etl/forge-ingest-common/`; this crate supplies the
endpoints.

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

Each scope's cursor is in the store's `sync_scope_state` table: the
run's pinned clock (`DATALIB_DAG_NOW`) of the last run that searched
that scope and got through what the searches listed. The next run
searches from that cursor. A run that was stopped moves no cursor, so
the next run lists the same span again. A scope with no cursor
searches from `now - refresh_window_days` (default 30; `0` is
unbounded). Widening
`refresh_window_days` pulls the cursor back to the new floor once
(`datalib_etl::scope_config`).

Every listed PR is fetched again: the search result's `updated_at` is
not trusted to say a PR is unchanged (GitLab's listing is, so GitLab
skips). A fetched PR's comment and review lists are each read whole,
and a stored child the list no longer names is deleted — a removed
comment, a resolved review thread. A list whose walk failed prunes
nothing.

`--full`, or an empty store, searches with no date bound.

`max_prs` caps how many PRs one run fetches. The ones past the cap are
owed: each gets a warning row (`pull_requests:<owner>/<repo>#<n>`),
the cursors move as usual, and the next runs fetch what is owed before
anything new, until the warnings are gone. A PR whose last fetch
failed is tried every run on top of the cap, so one that keeps failing
does not hold up the rest.

## When part of a sync fails

Only `/user` failing fails the step (without the account there is
nothing to search for — a credential GitHub refuses fails here), or a
read or write of the store. Anything else that fails is a `problems`
row, and the sync goes on with the rest.

- **The shared retry loop giving up** (rate limits or failures past
  the source's budget) stops the requests there, rather than failing
  every PR after it, but the run still keeps what it fetched. A
  `phase:fetch` row (`phase:search` when it was a search) says so, the
  cursors stay where they were, and the next run lists the same span
  and fetches the rest; the row goes with it.
- **A search** that fails is a `listing:search <scope>` row — a
  warning when GitHub refused the credential (401/403), an error
  otherwise. That scope's cursor stays where it was. A discovery run that
  gets to its end replaces the last run's rows, so the next run whose
  searches all answer clears them; one that gave up adds its rows and
  clears none.
- **A PR** that could not be fetched whole — its own record, or one of
  its comment or review lists — is a row on it
  (`pull_requests:<owner>/<repo>#<n>`): an error when no copy of the PR
  is stored, a warning when one is and is now stale. The next
  discovery run fetches it again even when no search names it any
  more, and its next whole fetch clears the row.
- **A PR GitHub answers 404 or 410 for** — its repository deleted, or
  out of this credential's reach — is gone, not failing: no row, no
  retry, and an earlier row of its goes. A copy the store holds is
  kept, as the last one there was.

A run that was stopped records none of this: once a stop is asked
for, every request fails at once, and that says nothing about GitHub.

## Single-PR mode

`--pull-request owner/repo#NUM` (also `owner/repo/pull/NUM` or a
github.com PR URL; repeatable) skips discovery entirely, fetching just
those PRs: no cursor moves, no listing row changes, and a PR an earlier
run could not fetch whole waits for the next discovery run. A give-up
of the retry loop is a row on the PR it stopped at. The
config's `api.pull_requests` list does the same.

## Config

The `api` block of a `github` source (`github_config`):
`refresh_window_days`, `max_prs` (a per-run cap; see above), `pull_requests`.
`latchkey_settings` picks which stored account to use.

## Run it

```sh
bazelisk build //third-party/latchkey-curl-shims
export LATCHKEY_CURL=$PWD/bazel-bin/third-party/latchkey-curl-shims/latchkey-curl-router
bazelisk run //datalib/backend/etl/providers/github:github_ingest -- \
    --out /tmp/github-mirror \
    --pull-request <owner>/<repo>#<num>
```
