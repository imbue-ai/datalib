# Untrusted data stays data

**Status: proposed; nothing here is built.** Written after the
2026-10-19 audit of the two weeks before it, checked against the tree at
`35110ed0b`. Line numbers are at that commit.

## 1. The problem

Five times now, an audit has found the same shape of hole: something
from outside — a message body, a link in one, a URL a web page sends the
browser to — reaches the app's own origin, and the app treats it as
code, as a URL to follow with its own authority, or as a capability.

| found | what it was |
|---|---|
| [2026-09-17](../audits/2026-09-17.md) §1 | Message bodies drawn as raw HTML in the page that holds the session: a stored XSS, and from there a config step whose `command` runs. |
| 2026-09-17 | DACTAL's `?dq=` reached `eval`, and a cross-site top-level navigation carries the `SameSite=Lax` cookie. |
| [2026-10-02](../audits/2026-10-02.md) | A body `<iframe>` pointing at our own `blobs/` route; own-path `/api/remote_media?…&source=<other>` URLs in a body. |
| 2026-10-19 | The URL path is card source. `router/columns.ts:36` `decodeColumns` turns each segment into source, `ContainersView.vue:183` adds it to the layout, and `cards/cardSource.ts:21` runs it with `new Function`. A relative link in an email reaches it through `DocCard.ce.vue:73` (it `window.open`s any same-origin link); any web page reaches it by navigating to `127.0.0.1:8731/<code>`. |

Each fix closed its door. Three things about the design keep making the
next one:

1. **One origin holds everything.** Hostile content, a UI that runs
   strings (`new Function`, CSP `'unsafe-eval'` in
   `http/src/embed.rs:22`), and an API that can write a shell command
   and run it all sit in one origin, under one cookie the browser sends
   by itself. Any slip that turns data into code there is code
   execution on the person's machine.
2. **URLs carry code and authority.** Card source in the path, `?dq=`,
   own-path URLs the server believes. A URL is the one input an
   attacker controls completely. `cards.md` §"Typed tables" already says
   "a URL arriving as data would be a capability" — but only for a
   grid's actions.
3. **The sanitizer allowlist only grows.** Chips need relative and
   `datalib:` links, charts need `data-time-series`, blobs need frames.
   Each widening is reasonable on its own; the property that matters is
   about all of them together, and only an audit looks at that.

## 2. The rule

**Data never becomes code, a URL followed with the app's authority, or
a capability.** Two consequences carry most of the weight:

- **Authority depends on who asks, not on what route they hit.** A
  process that read `system/api-token` (mode 0600) already runs as the
  person and could edit `config.toml` itself; giving it everything costs
  nothing. The page, whose credential the browser attaches on its own,
  gets everything *except* writing code.
- **Every string that could become code or a navigation passes one
  parser** that knows the handful of shapes the app means, and refuses
  the rest.

What this does not cover: script that does get into the page can still
*read* the mirror. DOMPurify and the document frame's `script-src
'none'` stay the defence for that. This plan makes the next slip cost
data, not the machine.

## 3. The pieces

### 3.1 Only a token-holding process writes code

`auth.rs` already tells credentials apart (`Credential::{Presented,
Query, Cookie}`, `auth.rs:275`). The gate attaches an `Authority` to the
request as an extension:

| credential | `Authority` |
|---|---|
| `Authorization: Bearer` or `X-Datalib-Token` (`Presented`) | `Machine` |
| the cookie, `?token=`, the trusted proxy's owner header | `Page` |

A handler that writes code takes a `Machine` extractor (axum
`FromRequestParts` that answers 403 for `Page`), so the rule is in the
handler's signature rather than in a list someone has to remember.
"Code" is:

- in `PUT /api/config`: any change to a step's or applet's `command` or
  `env`, or to the top-level `binary_dir`. `env` counts because `PATH`,
  `NODE_OPTIONS` and `DYLD_INSERT_LIBRARIES` are code too. A pure
  `code_changes(old: &DagConfig, new: &DagConfig) -> Vec<CodeChange>` in
  `dag/src/config.rs` decides it; `put_config` (`http/src/lib.rs:1045`)
  refuses a `Page` request when it is non-empty and accepts every other
  edit as today. The source wizard, the account box, group reorder
  (#1120) and `tomlPatch` edits (#1211) never touch these fields, so they
  keep working.
- `PUT /api/lib/{name}` (`lib.rs:609`): its body is JavaScript the page
  later `import()`s (`frontendRegistry.ts:118`).

Callers that change:

- **Agents** send the bearer token (`agent_config_guide.md`,
  `agent_cards_guide.md`), so they keep working unchanged.
- **The gallery's "build a component with an agent"**
  (`handoff.ts:153`) PUTs a seed module from the page. It becomes
  `POST /api/lib/{name}/seed`, which writes a seed the server holds;
  the page names it but sends no code.
- **The config card's text editor.** A `Page` save that changes a
  command gets a 403 whose message says what to do. In the desktop app
  that is a Tauri command which shows the change in a native dialog and,
  on yes, sends it with the token the shell already holds. The page can
  call the command but cannot answer the dialog. In a browser, or behind
  the proxy, the message says to edit `config.toml` or ask an agent.

Test, watched failing on the old code: a cookie `PUT /api/config` that
adds a step `command` gets 403 and leaves the file alone. The same edit
with the bearer token is written. A cookie edit elsewhere in a file that
already has commands is written.

### 3.2 Nothing from data is evaluated in the main origin

There are three ways into `compileCardSource` and `import()`:

1. **The URL.** It stops carrying source. A segment is parsed as a
   `CardSpec`: `name(<json>, …)` or `comp.<ns>.<name>(<json>, …)`, with
   `name` checked against `viewLibs` or the component manifest and the
   arguments read with `JSON.parse`. `cardSources.ts` writes the same
   shape back out. Every builder in the tree already makes this shape,
   and so do the card URLs agents are given. The five copies of
   `docSource` (DocCard, GridCard, SearchList, perseusView, UmapCard)
   become one in `cardSources.ts`.

   A segment that does not parse opens an inert card: it shows the text
   and has a "Run as card source" button. Running it is then the same
   deliberate step as typing it into a card's header. An old bookmark
   holding free-form JS gets that card, not an error.
2. **What a person typed in a card's header.** This stays code: it is
   the person's own console. `ShadowCard.vue:169` is the one caller of
   `compileCardSource`. Its argument becomes `CardSpec | TypedSource`,
   and a `TypedSource` (a branded string type) can only be made by the
   header editor and the inert card's button. So "who may hand me
   source" is answered by the type checker.
3. **Components** (`/modules/{hash}`, from applets and `/api/lib`).
   After §3.1, only a `Machine` caller or an applet named in the config
   can write one.

The layout tree in `/api/ui/state/{name}` holds card sources too, but
only page code writes it. Getting a source in there already takes code
in the origin, so it is not an entry point.

**Later, and a decision of its own:** run typed card source and
components in a sandboxed, opaque-origin frame that talks to the card
host over `postMessage`, as DACTAL does today
(`cards/libs/dactalView.ts`). Then `'unsafe-eval'` leaves the main
page's CSP, and the rule holds by construction instead of by the type
above. It is a large change, because components draw into shadow roots
in the main DOM today, so it is not in the PR sequence below.

### 3.3 One chokepoint for every link

`ui/src/appLink.ts` gets a pure `classifyHref(href, base)`:

```ts
type AppLink =
  | { kind: "document"; uuid: string; section: string | null } // /chat/<uuid>
  | { kind: "entity"; link: DatalibLink }                       // datalib:
  | { kind: "contact"; uri: string }                            // mailto: tel: sms: slack:
  | { kind: "external"; url: string }                           // http(s) elsewhere
  | { kind: "refused"; href: string };                          // everything else
```

A same-origin path that is not a shape the app knows is `refused`: it
is drawn as text with a toast, never opened. Every place that follows a
link goes through it:

- `DocCard.ce.vue:73`
- `externalLinks.ts:87,125`
- `GridCard.ce.vue:1459`
- `UmapCard.ce.vue:469`
- `SearchList.ce.vue`'s `:href="picked.source_url"`
- the four copies of the chip-menu handler (PersonCard, ChatBody,
  GridCard, `search/field.ts`), which become one `runChipMenu` in
  `contacts.ts`

On the render side, as a second layer, `etl/render/src/message.rs:96`
writes a `source_url` only when it is `http(s)` or `mailto`.

### 3.4 One fetch-metadata policy, for every credential

Today `is_cross_site_write` (`auth.rs:159`) runs only on the proxy
path, and `Credential::Cookie` goes straight through (`auth.rs:429`).
A pure `fetch_policy(method, path, headers, authority) -> Verdict` runs
in front of every route:

- **`Sec-Fetch-Site: cross-site` or `same-site`:** allowed only for a
  `GET`/`HEAD` navigation (`Sec-Fetch-Mode: navigate`) to `/` or a
  public route; anything else is refused. A navigation to any other
  path is redirected to `/`, which closes the "web page navigates to
  `/<code>`" door whatever §3.2 does.
- **No `Sec-Fetch-Site`:** a script, if it presented the token. With
  only the cookie, fall back to `Origin`, and refuse a write when that
  is absent too.
- **`Host`:** `127.0.0.1:<port>`, `localhost:<port>`, `[::1]:<port>`,
  and under `DATALIB_TRUSTED_PROXY` the forwarder's host(s). Any other
  `Host` is refused. That defeats DNS rebinding in every mode.

Proxy mode (#1209) stays. Imbue Studio and Minds frame the app through
it. The `Host` line is what makes its identity header hard to forge
from a browser. A secret from the proxy (or a unix socket) for local
processes is separate work.

The table of cases is the unit test: method × site × mode × credential
× path. The Tauri webview's requests are same-origin and unaffected.

### 3.5 Guardrails that keep it fixed

- **A lint check** in `scripts/lint_repo.py` refuses these sinks in
  `datalib/ui/src` outside a short allowlist, with the reason written
  next to each entry:
  - `new Function`, `eval(`
  - `innerHTML =`, `v-html`, `insertAdjacentHTML`, `document.write`
  - `window.open(`
  - `import(` of a non-literal

  The allowlist today: `cardSource.ts`, `ChatBody.ce.vue:547` (the
  sanitized frame), `CardControls.vue:93`, `appLink.ts`,
  `frontendRegistry.ts`.
- **A hostile email in the TNG fixture.** It is made up, like the rest
  of the fixture, and its body carries every trick so far:
  - a full-page relative link (`<a href="/…" style="position:fixed;inset:0">`)
  - a `javascript:` href
  - a `<div data-time-series="…">` naming a real fixture series
  - an `<iframe src="blobs/…">`
  - an own-path `/api/remote_media?…&source=<other>` image
  - a `datalib:` link with a malformed payload
  - a `\` in a `data-section-uuid`

  `document-sandbox.spec.ts` gains a spec that opens it and clicks
  across it, then asserts that:
  - no window or card opened;
  - no request reached `/api/config`, `/api/lib` or `/api/requests`;
  - no chart was drawn.

  It lands first, with each part that is still open marked `test.fail()`.
  Each PR below flips its own part. Adding an email moves the fixture's
  counts and goldens, so the same PR regenerates them through their
  `.update` targets.
- **Docs:** one paragraph in AGENTS.md ("Untrusted data stays data",
  pointing at the reference doc this becomes), and in `cards.md`
  §"The containers layout" (its **The URL** paragraph), "the URL is
  data: a `CardSpec`".

## 4. Order

Each step is one PR, small enough to review on its own:

0. The hostile email and its spec, parts marked `test.fail()`.
1. §3.4, the fetch-metadata and `Host` policy. Small. It closes the
   web-page door now, and the same-site writes from other loopback
   ports.
2. §3.2 (1–2) and §3.3: `CardSpec` URLs and the link chokepoint. They
   close the email-link door.
3. §3.1: `Machine` authority for code writes, and the Tauri confirm
   dialog.
4. §3.5's lint, with its allowlist as small as 1–3 left it.
5. (decide separately) §3.2's sandboxed frame, and dropping
   `'unsafe-eval'`.

## 5. Open questions

- **How do agents inside Imbue Studio and Minds reach the server:**
  with the token file, or through the forwarder? Through the forwarder
  they would be `Page`, and would lose config commands and `/api/lib`
  under §3.1. They would then need a token, or the proxy would need to
  mark them.
- **Should the config card's editor offer the Tauri confirm at all,**
  or just refuse and point at the file? The dialog is more code; the
  refusal is one sentence.
- **Should the inert card in §3.2 offer "Run" at all?** Dropping the
  button makes a URL never become code, and a person can still paste
  the text into a header.
