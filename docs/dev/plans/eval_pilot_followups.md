# What the Minds evals pilot found, and what is left to fix

**Status: built 2026-11-03, but for the decisions and the outside work
below.** The audit read main at `f5533f7a4` against the evals team's reports
(v0.40.0 to v0.48.0) and datalib#1471. git has the full plan, with what
was merged and each decision taken, at `479b6562d`.

## Decisions for Thad

1. **Replies as declared scope**: `replies = true | false | [channels]`,
   default true. Or rank every thread after all history while a
   channel has no coverage yet, with no knob.
3. **"Fetch this first"**: a one-run priority input to the frontier
   (named channels or threads), or leave it to stop and a scoped
   restart.
8. **`datalib-dag wait [request]`**: a verb that blocks until a request
   closes and exits with its outcome, beside `GET /api/requests/{id}`
   (#1524).
13. **Saying who is calling**: Slack requests carry Chrome's
    User-Agent on purpose. An `X-Datalib-Step` header sent only on the
    gateway path, which the gateway strips, needs latchkey's help.

## Outside this repo

- **default-workspace-template**: its `SKILL.md:159` resolves links
  against `github.com/.../blob/v0.48.0/docs/`, the large HTML pages;
  use the docs it installs. Install under `/opt` with links in
  `/usr/local/bin` rather than `$HOME/.local`. `qi-imbue/datalib-inspiration`
  is pinned to v0.42.0 and no longer what workspaces get.
- The harness items (a skill written after the chat starts is not
  offered; `required_skills` passes on a read) are the evals team's.
