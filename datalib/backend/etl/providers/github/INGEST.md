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
| `listed_change_requests` | every PR a search named, at the `updated_at` it gave |
| `coverage` | the stretch of `updated_at` each search scope has looked at |

The sync (the searches, what is owed, the fetch loop, child pruning,
what a failure costs) is shared with GitLab in
`datalib/backend/etl/forge-ingest-common/`; this crate supplies the
endpoints.

## Auth

One latchkey service: `github` (Bearer token, e.g. a fine-grained PAT
with read access to pull requests on the target repos). Latchkey injects
the `Authorization` header; this crate doesn't touch credentials.

## Discovery scopes

Each `--scope` (default: `author:@me`, `commenter:@me`, `mentions:@me`)
goes through the search-issues API as `is:pr <scope>`, bounded by
`updated:` to the stretch that scope has not looked at yet. The union
of the results is what is listed. `mentions:@me` is the cheap way to
catch incoming review pings on PRs the user otherwise wouldn't touch.

## What is owed

Nothing is marked done (docs/dev/plans/sync_state.md). Three facts are
stored, and what to fetch is worked out from them each run:

- **Listed.** Every search result is a row of `listed_change_requests`:
  the PR's key (`<owner>/<repo>#<n>`) and the `updated_at` the search
  gave. A PR listed again keeps the newest stamp any search gave it. A
  row goes only when GitHub says the PR is gone.

  The search's `updated_at` is trusted to move whenever anything under
  the PR does. Measured against a live account: across 14 PRs with
  issue comments, reviews and review comments, no comment, review or
  review comment (by its `created_at`, `updated_at` or `submitted_at`)
  was newer than the `updated_at` the search gave, and in 30 more the
  search's stamp equalled `GET /pulls/{n}`'s. A deleted child cannot
  be measured that way.
- **Looked at.** A search covers a span of `updated_at` in `coverage`
  (scope `search:<scope>`), written in the transaction that stores its
  results. The first search of a scope has no floor and covers from the
  beginning of time up to the run's clock (or the newest result, when
  GitHub's clock is ahead). A later run wants
  `[now − refresh_window_days, now]` — unbounded below when the window
  is 0 — and searches only the gaps between that and what is covered:
  normally one, above the last search's top, open-ended so a PR
  updated after the run began is listed too. Widening the window opens
  a gap below; nothing else remembers the config.
- **Held.** A PR's `pull_requests_bookkeeping.held_version` is the
  `updated_at` it was fetched for, written in the one transaction with
  its record, its three child lists, and the deletion of the stored
  children those lists no longer name (a removed comment, a resolved
  review thread).

A PR is owed when it is listed at a stamp its sidecar does not hold.
The ones never fetched go first, fewest attempts first, then the ones
held at an older stamp, by key. `max_prs` takes the first N of one run;
the rest stay owed, with no row saying so. Each PR is a transaction of
its own, sealed on the step's cadence, so a run that dies at any
request leaves a store the next run finishes
(`tests/github_tests/interrupt.rs` cuts a run at every request and
checks that).

GitHub's search answers at most 1000 results, newest first. A search
cut off there covers only down to the oldest result it gave, and is a
`listing:search <scope>` row; the next run searches the stretch below
it (`updated:<=`) and lists the rest. A search GitHub reports as
`incomplete_results` covers nothing, so the next run asks again.

`--full` searches every scope with no bound and fetches everything
listed, whatever the store holds.

A store written before the listing existed is carried over on open
(rung 1 of `schema_raw::LADDER`): every stored PR is listed at the
`updated_at` its record carries and held at it when its last fetch
landed whole, each scope's cursor becomes the top of a span from the
beginning of time, and the warning rows a cap wrote go.

## When part of a sync fails

Only `/user` failing fails the step (without the account there is
nothing to search for — a credential GitHub refuses fails here), or a
read or write of the store. Anything else that fails is a `problems`
row, and the sync goes on with the rest.

- **The shared retry loop giving up** (rate limits or failures past
  the source's budget) stops the requests there, rather than failing
  every PR after it; the run keeps what it fetched. A `phase:fetch`
  row (`phase:search` when it was a search) says so, what the searches
  listed stays owed, and the next run fetches the rest; the row goes
  with it.
- **A search** that fails is a `listing:search <scope>` row — a
  warning when GitHub refused the credential (401/403), an error
  otherwise. It covers nothing, so the next run searches the same
  stretch again. A discovery run that gets to its end replaces the
  last run's rows, so the next run whose searches all answer clears
  them; one that gave up adds its rows and clears none.
- **A PR** that could not be fetched whole — its own record, or one of
  its comment or review lists — stores nothing of that fetch and stays
  owed, with a row on it (`pull_requests:<owner>/<repo>#<n>`): an
  error when no copy of the PR is stored, a warning when one is and is
  now stale. The next run fetches it again, whether or not a search
  names it, and its next whole fetch clears the row. Twenty-five
  failures in a row end the run's fetching, as a `phase:fetch` row.
- **A PR GitHub answers 404 or 410 for** — its repository deleted, or
  out of this credential's reach — is gone, not failing: it leaves the
  listing, its row goes, and no later run asks for it again. A copy
  the store holds is kept, as the last one there was.

A run that was stopped records none of this: once a stop is asked
for, every request fails at once, and that says nothing about GitHub.
What it listed before the stop is stored, and the next run fetches it.

## Single-PR mode

`--pull-request owner/repo#NUM` (also `owner/repo/pull/NUM` or a
github.com PR URL; repeatable) skips discovery entirely, fetching just
those PRs whatever the store holds: nothing is listed or covered, and
a PR an earlier run could not fetch whole waits for the next discovery
run. A give-up of the retry loop is a `phase:fetch` row. The config's
`api.pull_requests` list does the same.

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
