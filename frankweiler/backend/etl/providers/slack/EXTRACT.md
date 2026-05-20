# Slack Extract

`slack-download` mirrors a Slack workspace into
`<out>/raw_api/<method>/events.jsonl`. Each Slack API page becomes one
envelope record `{_recorded_at, method, params, duration_ms, response}`.

## Auth

The downloader does not handle Slack tokens directly. It shells out to
[`latchkey curl`](https://github.com/imbue-ai/latchkey), which signs
requests using a token stored in the host keyring under the `slack`
service. `latchkey` must be on `PATH` for the binary to run.

Required Slack OAuth scopes (user token):

  * `channels:history`, `groups:history`, `im:history`, `mpim:history`
  * `channels:read`, `groups:read`, `im:read`, `mpim:read`
  * `users:read`, `auth:test`

### File downloads

File bytes live on `https://files.slack.com/`, which the `slack`
service's `baseApiUrls` covers as of latchkey 2.11.2. No extra service
registration is needed — the same `slack` credential signs both
`slack.com/api/` and `files.slack.com/` requests.

## API surface used

| Method                      | Purpose                                  |
|-----------------------------|------------------------------------------|
| `auth.test`                 | Identify the workspace + the calling user |
| `conversations.list`        | Enumerate channels                       |
| `users.list`                | Enumerate workspace users                |
| `conversations.history`     | Per-channel forward pass + refresh window |
| `conversations.replies`     | Threaded replies for every parent message |

`shapes.rs` is the shape-of-the-response catalog: which path holds the
items, what counts as the cursor key, how to dedup.

## Resume + dedup

There is no checkpoint file. The dedup index over
`events.jsonl` doubles as the resume cursor:

  * For each channel, take `max(ts)` across all recorded `history`
    pages and start the next forward pass there.
  * For the trailing refresh window (default `DEFAULT_REFRESH_WINDOW_DAYS`,
    30 days), re-query that range — the dedup pass collapses no-op
    refreshes to zero writes.

A page is skipped if every item in it matches a prior capture by
canonical content hash, so re-running soon after a successful run is
cheap.

## Rate limits

Slack returns `429 Retry-After`; `api::call_slack` honors the header
and backs off. Persistent failures bubble up as `SlackError`. There's
no in-process rate limiter beyond that — Slack's own headers are
the contract.

## Sample data

A curated [Star Trek: TNG-themed
fixture](tests/fixtures/slack_api/) demonstrates the raw wire format
and lives next to the code under test. The Python translator currently
reads it from this location as well.
