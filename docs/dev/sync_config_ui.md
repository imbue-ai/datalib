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

The two rows in bold are the ones that change hands: the credential UI
becomes something datalib owns and other hosts embed, and the service
definitions move down into latchkey instead of being maintained
separately by each product.

## The simplification, counted

The count needs care, because two layers are easy to conflate:

- **The wrapper** — knowing how to invoke latchkey, parse `services
  info`, map the `auth browser` error taxonomy, and turn latchkey's
  `setCredentialsExample` string back into form fields.
- **The UI** — the catalog, the Connect button, the login progress and
  its error states, the token form.

latchkey is neither. It is the mechanism both of them wrap. Counted that
way:

| | Wrapper | UI |
|---|---|---|
| Today | latchkey (native) + `mngr_latchkey` — `core.py` 1,846 lines, `credential_commands.py` 193 | Minds — `PermissionsTab.ts` 981, `workspacePermissions.ts` 487, `ui_api_permissions.py` 484 |
| datalib standalone, regardless of this proposal | + datalib (thin: spawn, parse JSON, stream progress) | + datalib |
| After phase 3 | latchkey + datalib | datalib |

**This proposal does not delete datalib's implementation — datalib's is
the one that survives.** And before phase 3 the count goes *up*, not
down. Say that plainly, because the case does not rest on an immediate
deletion. It rests on three things that are each true on their own.

### 1. datalib builds this anyway

The trifecta constraint forces datalib to stand alone; standing alone
means owning credential establishment. The credential UI is therefore
not a cost of *this* proposal — it is a cost of datalib existing, and it
is being paid either way.

The marginal ask here is only that it be built behind a seam: the
`?embed=1` route, the embed-mode auth below, and a `frame-ancestors`
policy. That is days of work, not the UI. Build it closed instead and
the work still happens; Minds just can never benefit from it.

### 2. The unconditional win is the definitions, not the UI

Phase 0 is where something is actually deleted, and it needs no
cooperation from either product's UI. Today a service catalog exists
three times: latchkey's own service list; Minds' `services.json`,
generated from detent's schemas; and Minds' `additional_services.json` —
34 hand-written lines holding `claude-ai`, a definition that lives there
only because it had nowhere better to go, and that needed a whole
propagation mechanism to reach remote gateways. Push those definitions
upstream and the hand-maintained copy dies, along with the mechanism
that kept it in sync.

### 3. Upstream definitions shrink the UI everyone has to build

This is the mechanism by which the UI gets smaller rather than
multiplying, and it is the part most worth understanding.

A Connect button is trivial. A token form is where the complexity lives:
per-service prose about where to find the secret, parsing
`setCredentialsExample`, validation, the "now go run this in a terminal"
escape hatch. `credential_commands.py` exists *entirely* because some
services have no browser login.

So every service that gains `browser` support upstream removes a token
form — from **both** products at once. Of datalib's seven API sources,
`browser` reaches two today (slack, github). The spike takes that to
four (claude-ai, chatgpt). Fastmail is already in flight on its own
latchkey branch, which would make five. That leaves gitlab and notion.

The UI that never gets written is the token form for those five — and it
is deleted upstream, in latchkey, not in either product.

### Phase 3 is option value, not a promised saving

If Minds later delegates credential establishment to the shared
component, it can retire its credential half — the connect path in
`PermissionsTab` and the credential portion of `mngr_latchkey` — while
keeping grants. That is a real ~2,000-line prize, but it is *optional*
and it is *last*, so it should be argued as an option this proposal buys
cheaply, not as a saving it promises. Build the component embeddable and
Minds can take that option whenever it wants. Build it closed and the
option never exists.

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
That work is identical for both products. Minds has written it; datalib
is about to.

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
