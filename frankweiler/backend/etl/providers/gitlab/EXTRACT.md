# GitLab Extract

`gitlab-download` mirrors GitLab merge requests + their discussion
threads via `gitlab.com/api/v4`. Same event-store JSONL layout as the
other providers:

```
<out>/
  self_identity/{created,updated}/events.jsonl     # /user
  merge_request/{created,updated}/events.jsonl     # /projects/.../merge_requests/{iid}
  discussion/{created,updated}/events.jsonl        # /projects/.../merge_requests/{iid}/discussions
  sync_state.json                                  # per-scope last-seen-at
```

GitLab discussions are natively threaded — each discussion record
already carries its full `notes[]` array — so we store one record per
discussion and let the translate stage unroll into per-note rows.

## Auth

One latchkey service: `gitlab` (PRIVATE-TOKEN). Latchkey injects the
`PRIVATE-TOKEN` header; this crate doesn't touch credentials.

## Discovery scopes

`gitlab-download` runs each `--scope` (default: `created_by_me`,
`assigned_to_me`, `reviewer`) against the global `/merge_requests`
endpoint with `updated_after=<since>&state=all`. The `reviewer` scope
expands to `reviewer_id={self.user_id}` since GitLab requires an
explicit user id.

GitLab REST doesn't expose a "commenter:@me" or "mentions:@me" filter
the way GitHub does. Coverage of incoming review pings comes from the
author / assignee / reviewer trio; pure @mentions on third-party MRs
would need `/todos?action=mentioned` (not currently fetched).

## Incremental sync

Same model as github: `<out>/sync_state.json` keeps `last_seen_at` per
scope, `since_for_scope` floors to `now - refresh_window_days`, `--full`
bypasses, empty out_dir forces full backfill.

## Single-MR mode

`--merge-request namespace/project!IID` (or a gitlab.com MR URL) skips
discovery and pulls one MR + all its discussions.

## Run it

```sh
export LATCHKEY_CURL=$PWD/target/debug/latchkey-curl-shim
cargo run -p frankweiler-etl-gitlab --bin gitlab-download -- \
    --out /tmp/gitlab-mirror \
    --merge-request generally-intelligent/generally_intelligent!7643
```
