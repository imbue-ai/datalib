# GitLab Extract

`gitlab-ingest` mirrors GitLab merge requests and their discussion
threads from `gitlab.com/api/v4` into one doltlite raw store,
`<out>/entities.doltlite_db`. The sync — what is listed, looked at and
held, what is owed, the fetch loop, pruning deleted children, `--full`
— is the one GitHub uses, and so is what a failure costs; read
[`../github/INGEST.md`](../github/INGEST.md) §"What is owed" and
§"When part of a sync fails" for them, with `merge_requests:<project>!<iid>`
for the row on an MR and `listing:search <scope>` for a scope. What
differs is below.

| table | endpoint |
|---|---|
| `self_identity` | `/user` |
| `merge_requests` | `/projects/{id}/merge_requests/{iid}` |
| `discussions` | `/projects/{id}/merge_requests/{iid}/discussions` |
| `listed_change_requests` | every MR a listing named, at the `updated_at` it gave |
| `coverage` | the stretch of `updated_at` each listing scope has looked at |

GitLab discussions are natively threaded — each discussion record
already carries its full `notes[]` array — so the store keeps one row
per discussion and render unrolls it into per-note rows. Before a
payload is stored, a numeric `v=` parameter is stripped from every
`avatar_url` (`src/ingest/canonicalize.rs`), so an avatar bump alone
is not a change.

An MR is listed at the `updated_at` the listing gives, to the
millisecond, and the listing is bounded by whole stamps
(`updated_after`, `updated_before`); GitLab's listing has no cap the
way GitHub's search has. An MR is held with its whole discussion list
in one transaction, so one whose discussions would not list stores
nothing and stays owed. `max_mrs` is GitHub's `max_prs`: it counts the
MRs fetched, not the ones listed at a stamp the store already holds.

## Auth

One latchkey service: `gitlab` (PRIVATE-TOKEN). Latchkey injects the
`PRIVATE-TOKEN` header; this crate doesn't touch credentials.

## Discovery scopes

Each `--scope` (default: `created_by_me`, `assigned_to_me`, `reviewer`)
queries the global `/merge_requests` endpoint with
`scope=<scope>&state=all`, bounded by `updated_after` and
`updated_before` to the stretch that scope has not looked at.
`reviewer` becomes `reviewer_id=<your user id>`, since GitLab has no
reviewer scope.

GitLab REST has no "commenter" or "mentions" filter the way GitHub
does. Incoming review pings are covered by the author / assignee /
reviewer trio; a bare @mention on someone else's MR would need
`/todos?action=mentioned`, which is not fetched.

## Single-MR mode

`--merge-request namespace/project!IID` (or a
`https://gitlab.com/<project>/-/merge_requests/<iid>` URL; repeatable)
skips discovery and pulls just those MRs and their discussions,
whatever the store holds. The config's `api.merge_requests` list does
the same.

## Config

The `api` block of a `gitlab` source (`gitlab_config`):
`refresh_window_days`, `max_mrs`, `merge_requests`.

## Run it

```sh
bazelisk build //third-party/latchkey-curl-shims
export LATCHKEY_CURL=$PWD/bazel-bin/third-party/latchkey-curl-shims/latchkey-curl-router
bazelisk run //datalib/backend/etl/providers/gitlab:gitlab_ingest -- \
    --out /tmp/gitlab-mirror \
    --merge-request <group>/<project>!<iid>
```
