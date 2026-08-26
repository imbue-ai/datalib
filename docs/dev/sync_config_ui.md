# Design: the sync config UI as a portable component

**Status: proposal, 2026-08-26. Nothing here is built yet.** Per
[`AGENTS.md`](../../AGENTS.md), don't cite this file as a description of
the tree. When a slice lands, rewrite the section it makes real and
delete the rest.

Companion to [`source_wizard.md`](source_wizard.md) ([#174]), which
designs what the wizard *does* — the catalog descriptor, the probe verb,
the screens, the TOML it writes. **This document is about where that UI
lives and who else can host it.** Read that one for the wizard's
internals; this one for the boundary around it.

[#174]: https://github.com/imbue-ai/datalib/pull/174

## The proposal in one paragraph

Build the sync config UI once, in datalib, as a route that is embeddable
from day one. datalib hosts it as part of its own app; the Minds chrome
hosts the same route in a cross-origin iframe, the same way it already
hosts workspace UIs. Push the *service definitions* it depends on down
into latchkey, where both products already look for them. Nothing about
this puts datalib — or a user's private mirror — inside a Minds
workspace.

## The constraint that shapes everything

datalib mirrors a person's private history: their Slack, their Claude and
ChatGPT conversations, their mail. A Minds workspace is a Claude Code
agent with exposure to untrusted content and network egress. Put the
mirror on that filesystem and all three legs of the lethal trifecta are
armed at once, by construction, for every workspace.

So datalib does not run in a workspace. That is a constraint, but it is
worth noticing that **it removes work rather than adding it**: it deletes
the "datalib as a workspace app" option, and with it the pluggable
credential-provider seam that option required (datalib would have needed
one credential path for standalone and another for in-workspace). What
is left is one credential path and one place the data lives.

## The boundary that lets us have both

Minds already draws the line this needs. The chrome — the desktop client
on the user's machine — is the trusted embedder; a workspace is a
cross-origin iframe treated as hostile. That is the entire point of
[`docs/embed-contract.md`][embed] and the `frame-ancestors` policy
`mngr_forward` appends to workspace responses.

"Usable inside Minds" is ambiguous between two asks that land on opposite
sides of that line:

- **"I'm in Minds and I want to set up a sync."** Configuration only. No
  private data crosses anything. Lives in the chrome. Safe.
- **"I want my agent to search my mirror."** That *is* the trifecta.

Keep them separate and the design stays simple. This document is
entirely about the first. The second should be a deliberate, separately
designed decision with its own grant model — not something you inherit
because datalib happened to be running in the container.
[`http/src/auth.rs`](../../datalib/backend/http/src/auth.rs) already
reasons about exactly this threat class for the browser case, and is the
natural place to hang such a model if it is ever wanted.

[embed]: https://github.com/imbue-ai/mngr-internal/blob/main/apps/minds/docs/embed-contract.md

## What moves where

The case rests on this table. Each concern lands where its *concepts*
live, and nothing moves that shouldn't.

| Concern | Owner | Why |
|---|---|---|
| Service definitions (login URL, login flow, base API URLs) | **latchkey** | Both products already depend on it; it is the only place that can serve both without either depending on the other |
| Credential establishment UI (Connect button / token form) | **datalib**, embeddable | One implementation, several hosts |
| Credential status + health | latchkey (`services info`) | Already exists; nothing to build |
| Agent grants (detent rules per `(service, account)`) | **Minds** | datalib has no concept of an agent, and shouldn't acquire one |
| Sync configuration (what to mirror, from when) | **datalib** | Minds has no concept of a data root |
| The mirror itself | **datalib, on the user's machine** | The trifecta constraint above |

The two rows in bold that change hands are the credential UI (built once
in datalib instead of twice) and the service definitions (pushed down
into latchkey instead of living in Minds and datalib separately).

## The simplification, counted

Today "sign the user into a third-party service" is implemented, or
planned, **four times** across three repos:

1. **latchkey** — `auth browser`, the login-flow registry, `services
   info`. The actual mechanism.
2. **`mngr_latchkey`** — a Python re-derivation of what latchkey already
   knows: `core.py` (1,846 lines) parses `services info`, carries the
   `auth browser` error taxonomy and the `browser-prepare` retry;
   `credential_commands.py` (193) parses latchkey's
   `setCredentialsExample` string back into form fields.
3. **Minds' UI** — `PermissionsTab.ts` (981) plus
   `workspacePermissions.ts` (487) and `ui_api_permissions.py` (484).
4. **datalib** — phases 2 and 3 of [#174] propose a Rust re-derivation of
   (2) and a Vue re-derivation of (3).

And the service catalog exists **three times**: latchkey's own service
list; Minds' `services.json`, generated from detent's schemas with
`HIDDEN_BUILTIN_SERVICES` skipped; and Minds' `additional_services.json`
(34 lines), which holds `claude-ai` — a definition that exists there only
because it had nowhere better to live, and which needed a whole
propagation mechanism to reach remote gateways.

After the shuffle: **one mechanism (latchkey), one credential UI
(datalib), one grant model (Minds).** datalib never writes (4). Minds can
eventually delete `additional_services.json` and its propagation path.

That is the case. It is not "datalib would like a favour" — it is a net
deletion of two of the four implementations, and the one that never gets
written is datalib's.

### This is already partly proven

A spike (branch `claude-ai-chatgpt-services` in `imbue-ai/latchkey`) adds
`claude-ai` and `chatgpt` as built-in latchkey services. Both now report
`authOptions = ["browser", "set"]`, which is exactly the field that turns
a credential screen from a paste-a-token field into a Connect button — in
datalib's planned wizard *and* in Minds' existing `PermissionsTab`, for
free, with no coupling between them.

Notable findings from building it, because they are the kind of thing
that decides whether this is a week or a quarter:

- **`claude-ai` is pure configuration.** claude.ai authenticates with one
  `sessionKey` cookie, so the service configures latchkey's existing
  generic `cookie-capture` login flow rather than implementing anything.
  The credential it stores is byte-identical to what `auth set -H
  "Cookie: sessionKey=…"` produces today, so existing accounts keep
  working and a colliding user registration is silently skipped in favour
  of the built-in — a transparent upgrade, not a migration.
- **`chatgpt` cannot use that flow.** Its session cookie authenticates
  the *page*, which calls `/api/auth/session` to mint the short-lived
  bearer token `/backend-api` actually wants. Capturing the cookie would
  store something the API refuses. It needs a real (small) service that
  watches for that response. Symmetry was the natural assumption and it
  is wrong.
- **`cookie-capture` needs latchkey ≥ 3.3.0 and datalib pins 3.1.0**
  (`LATCHKEY_VERSION` in `datalib/backend/core/src/node_runtime.rs`).
  Verified, not assumed: `npx -y latchkey@3.1.0 services register --help`
  lists no `--login-flow` option at all. Bumping that pin is a
  prerequisite for any of this.
- **latchkey's own credential check cannot reach either host.** Both sit
  behind Cloudflare's managed challenge, which answers vanilla curl with
  a 403 interstitial, so `services info` reports `invalid` and no account
  for credentials that are fine. The browser login is unaffected — it
  drives a real Chrome. Everything works once `LATCHKEY_CURL` points at a
  Chrome-impersonating curl, which is datalib's normal state and which
  Minds already ships. Whether latchkey should carry the
  `X-Imbue-Impersonate` marker itself is an open question below.

## The component contract

### One route, two hosts

The UI is **a route in datalib's existing SPA**, not a separate build.
Embedding is a query parameter that suppresses datalib's own chrome:

```
http://localhost:8731/sources/new              # datalib's own app
http://localhost:8731/sources/new?embed=1      # same code, no nav/header
```

There is no second artifact, no bundle for Minds to import, and no
version skew: the running datalib serves its own UI, so exactly one
version is ever in play. This is the same shape as Minds' own
`system_interface`, which serves the dockview UI at the workspace origin
for the chrome to frame.

Designing the embed seam in from the start is cheap; retrofitting it is
not. That is the one thing this document asks of [#174]'s phase 1.

### Auth across the boundary

This is the part that does not work by default, and the reason it is
worth writing down before building.

datalib's front door is a per-process token accepted four ways
(`Authorization: Bearer`, `X-Datalib-Token`, `?token=`, or a cookie), and
the browser path normally ends in an `HttpOnly; SameSite=Lax` cookie —
see [`auth.rs`](../../datalib/backend/http/src/auth.rs). Two things break
that inside an iframe:

1. **`SameSite=Lax` will not survive the embed.** The chrome serves
   workspace origins over `https://…localhost:8421`; datalib binds
   `127.0.0.1:8731` over plain http. Different host *and* different
   scheme, so the framed document is cross-site under schemeful
   same-site, and a Lax cookie is not sent with its subresource requests.
   `SameSite=None` is not available either, since it requires `Secure`
   and datalib is http on loopback.
2. **`EventSource` cannot set headers.** The sync progress stream
   (`openJobStream` in `ui/src/api.ts`) is SSE, so a
   bearer-token-in-header scheme authenticates the other ten call sites
   and silently fails on the one that streams job progress.

So embed mode should:

- take the token from `?token=` once, hold it **in memory** (not a
  cookie), and strip it from the URL with `history.replaceState`;
- attach it as `Authorization: Bearer` in `api.ts` — which is the single
  fetch layer for all eleven call sites, so this is one file;
- append `?token=` to the `EventSource` URL specifically, since that
  carrier is the only one it has.

The chrome gets the token by reading `<data_root>/system/state/api-token`
from disk. It runs on the user's machine, so it can; and a token the
chrome holds is strictly less exposure than a UI the chrome reimplements.

### Framing policy

datalib currently sends **no `frame-ancestors` and no
`X-Frame-Options`** — grep both across `backend/http/src` and `ui/src`
and there are zero hits. Any page can frame datalib's UI today. The Lax
cookie means a hostile framer cannot ride the user's session, so this is
not an open door, but embedding should *add* a policy rather than rely on
that: serve `frame-ancestors 'self' <configured embedder origins>`, with
the embedder origin passed to `datalib-http` the way `mngr_forward` takes
`--embedder-origin`. This is a security improvement that the embedding
work pays for.

### Messages

Mirror Minds' contract policy exactly, because it is good and because
matching it means one mental model: no version on the wire, receivers
ignore unknown types and unknown fields, shipped types are immutable,
evolve by adding.

| Direction | Type | Payload | Meaning |
|---|---|---|---|
| datalib → host | `datalib:height` | `{ px }` | Content height, so the host can size the frame |
| datalib → host | `datalib:credential-established` | `{ service, account }` | A sign-in completed. Minds uses this to offer its *grant* step |
| datalib → host | `datalib:config-saved` | `{ sourceCount }` | The config was written |
| host → datalib | `datalib:theme` | `{ mode, tokens? }` | Follow the host's light/dark and palette |
| host → datalib | `datalib:close` | `{}` | Host chrome asked the flow to end |

`credential-established` is the load-bearing one: it is the seam where
datalib's job (establish a credential) hands off to Minds' job (decide
what an agent may do with it). Neither side has to understand the other's
model.

## What "replacing what's there" does and does not mean

It is worth being precise, because the overclaim is easy to make and easy
to refute.

**Can be shared:** establishing a credential — the Connect button, the
browser-login progress and its error states, the token form derived from
`setCredentialsExample`, the "connected as `thad@imbue-ai` ✓" state.
That work is identical for both products and is currently written twice.

**Cannot be shared, and shouldn't be:** Minds' per-`(service, account)`
detent grants, the permission-request queue, the toggles that decide what
an *agent* may reach. datalib has no concept of an agent and should not
grow one to make a component fit.

So the honest end state is not "datalib's page replaces Minds' page." It
is: Minds' Add-connection flow delegates the credential step to the
shared component and keeps the grant step, which is the half that is
actually about Minds. That is a smaller claim, and it is one that
survives contact with the code.

It is also **optional and last**. Nothing earlier in the phasing depends
on it.

## Phasing

| Phase | Delivers | Depends on |
|---|---|---|
| **0** | `claude-ai` + `chatgpt` as built-in latchkey services; bump datalib's `LATCHKEY_VERSION` past 3.3.0 | nothing — spike already built |
| **1** | datalib's sync config UI, with `?embed=1` and the message contract designed in ([#174] phases 1–2) | 0 |
| **2** | `frame-ancestors` + embed-mode auth in `datalib-http`; Minds chrome embeds the route behind a "datalib is installed" check | 1 |
| **3** | *Optional.* Minds' Add-connection delegates credential establishment to the component, keeps grants | 2 |

Phase 0 is worth doing on its own merits whatever happens to the rest: it
gives **both** products browser login for Claude and ChatGPT, deletes the
reason `additional_services.json` exists, and costs neither product a
dependency on the other. If this proposal is rejected entirely, phase 0
should still land.

## Objections, answered

**"You want datalib running in my workspaces."** No — the opposite. The
trifecta constraint rules it out explicitly. This asks for *less* than
the obvious integration: no container weight, no backup weight, no new
data on workspace filesystems.

**"Another cross-repo dependency."** It is the dependency both products
already have (latchkey), plus an iframe Minds already knows how to host.
And Minds already consumes datalib as a pinned release artifact — the
Chrome-impersonating curl, currently v0.26.0, bundled by
`download-binaries.js` and installed onto VPS gateways. This is the same
seam, used for a second thing.

**"My chrome is mithril; I don't want Vue in it."** You don't get Vue.
You get a cross-origin iframe, exactly like a workspace.

**"What if datalib isn't installed?"** The affordance doesn't render.
Minds' existing Permissions tab is untouched and keeps working. Every
phase is additive.

**"Version skew between the two apps."** There is none to have: the
running datalib serves its own UI. No bundling, no lockstep release, no
npm package to keep in step.

**"Who owns the UX?"** Minds owns placement, theming, and whether the
entry point exists at all. datalib owns the flow inside the frame. Same
split as workspace content today.

## Open questions

1. **Should latchkey carry the impersonation marker itself?** Adding
   `X-Imbue-Impersonate: 1` to the `credentialCheckCurlArguments` of the
   Cloudflare-fronted services would make `services info` correct
   wherever the dispatch curl is present — which is both products — at
   the cost of a datalib/Minds-specific header in upstream latchkey. One
   line; needs a decision, not a spike.
2. **Does the same-site analysis hold in Electron's partition?** The
   reasoning above is from the spec, not from a measurement. A 20-minute
   test — frame a token-authenticated datalib route from the chrome and
   watch whether the cookie rides along — settles it, and it is worth
   doing before phase 2 rather than during.
3. **Where does the chrome learn datalib's port?** `DATALIB_BIND`
   defaults to `127.0.0.1:8731`, but it is configurable. A well-known
   file next to the API token is the obvious answer; worth confirming
   there isn't already one.
4. **Does `frame-ancestors` want to be per-embedder or a single
   allowlist?** `mngr_forward`'s `--embedder-origin` is the precedent to
   copy or deliberately diverge from.
