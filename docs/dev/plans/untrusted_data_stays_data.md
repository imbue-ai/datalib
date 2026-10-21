# Untrusted data stays data

**Status: agreed 2026-10-20; §3.1 and §3.3 built (#1268), §3.2 in #1269.** This
replaces the first version of this plan (#1241, `bbc500272`), which
proposed five pieces. Two of them were dropped after review, and two were
changed; §5 says which and why. Line numbers are at `56d70dd61`.

## 1. The problem

Five audits have found the same shape of hole: something from outside (a
message body, a link in one, a URL a web page sends the browser to)
reaches the app's own origin, and the app treats it as code, as a URL to
follow with its own authority, or as a capability.

| found | what it was |
|---|---|
| [2026-09-17](../audits/2026-09-17.md) §1 | Message bodies drawn as raw HTML in the page that holds the session: a stored XSS, and from there a config step whose `command` runs. |
| 2026-09-17 | DACTAL's `?dq=` reached `eval`, and a cross-site top-level navigation carries the `SameSite=Lax` cookie. |
| [2026-10-02](../audits/2026-10-02.md) | A body `<iframe>` pointing at our own `blobs/` route; own-path `/api/remote_media?…&source=<other>` URLs in a body. |
| 2026-10-19 | The URL path is card source. `router/columns.ts` `decodeColumns` turns each path segment into source, `ContainersView.vue` `routeNode` opens a card on it, and `cards/cardSource.ts` runs it with `new Function`. A relative link in an email reaches it (`DocCard.ce.vue` `onFrameLink` opens any same-origin link), and so does any web page, by navigating to `127.0.0.1:8731/<code>`. |

The last one is the one this plan closes outright. The design facts that
keep producing the others:

1. **One origin holds everything.** Hostile content, a UI that turns
   strings into code (`new Function`, and the CSP's `'unsafe-eval'` that
   permits it), and an API that can write and run a shell command share
   one origin and one cookie the browser sends by itself.
2. **The server cannot tell its own page from another page.** It reads
   no header that says where a request came from, except in proxy mode.

## 2. The rule

**Data never becomes code.** A card's source, wherever it comes from (a
URL, a saved layout, the header a person types into), can only name a
function the app already has and give it values. There is nothing to
check for, because there is no way to say anything else.

## 3. What we build

### 3.1 Card source is a call, by construction

Today card source is a JavaScript expression run with `new Function`
(`datalib/ui/src/cards/cardSource.ts:21`). It becomes a small language
with one form:

```
source := id "(" [ arg ("," arg)* [","] ] ")"
id     := name ("." name)*          gridView, comp.user.tetris
arg    := a JSON5 value
```

- **The arguments are [JSON5](https://json5.org/).** It is JSON plus
  unquoted keys, single quotes and trailing commas, so what people
  already type (`gridView({ q: "picard" })`, the agent guide's own
  example) keeps parsing. It is still data only. Builders keep writing
  strict JSON, which is valid JSON5.
- **`id` is looked up, never walked.** A built-in is a key of `viewLibs`;
  a component is `comp.<namespace>.<name>`, found by exact match in the
  component manifest. Lookups use own properties only, so a name like
  `constructor` or `__proto__` reaches nothing.
- **One grammar everywhere:** the URL path, the layout saved in
  `/api/ui/state/{name}`, and the source box in a card's header in edit
  mode. `compileCardSource` becomes parse, look up, call. `new Function`
  leaves the tree.
- **`'unsafe-eval'` leaves the app page's CSP**
  (`datalib/backend/http/src/embed.rs:22`). With no evaluator in the
  page, a string that reaches one by mistake still cannot run: the
  browser refuses. The DACTAL page keeps its own `'unsafe-eval'`; it
  runs in a sandboxed frame with an opaque origin.
- **Source that does not parse** opens a card that says so and shows the
  text. There is no "run it anyway": nothing in the page can. What used
  to be an inline function in a header becomes a component
  (`comp.user.*`, `agent_cards_guide.md`).

What a person loses is typing arbitrary JavaScript into a card's header.
The way to run your own code in a card is a component, which is how the
agent guide already teaches it.

A component's arguments now come from whoever wrote the link. The
grammar guarantees nothing but a known function and plain values reaches
a card; it cannot stop a component from misusing those values.
`cards.md` gains the rule for component authors: **opening a card never
writes.** A component may not save, sync or delete because of the
arguments it was opened with.

**Existing data.** A saved layout or a bookmark whose source is already
a call keeps working unchanged; every builder in the tree writes one.
One that is not opens the "does not parse" card with its text, and the
person can rewrite it.

### 3.2 The server reads where a request came from

One pure function, `fetch_policy`, runs in front of every route for
every kind of credential, beside the token check in `require_token`
(`datalib/backend/http/src/auth.rs:401`). It reads only headers a page
cannot set:

1. **A navigation is allowed from anywhere, to any path:** a `GET` or
   `HEAD` with `Sec-Fetch-Mode: navigate`. Once §3.1 is in, opening any
   card URL is safe, so an agent's card link clicked on a web page keeps
   working.
2. **Any other request marked `Sec-Fetch-Site: cross-site` or
   `same-site` is refused** with 403. This closes a page on another port
   of `127.0.0.1` (the same "site") sending a body-less `POST` that the
   browser does not preflight, such as `/api/steps/<id>/turn_off`, whose
   handler accepts an empty body (`datalib/backend/http/src/lib.rs`
   `by_of`).
3. **A request with no `Sec-Fetch-Site`** (`curl`, an older browser) is
   let through when it presents the token. A cookie-only write without
   it must carry an `Origin` naming the server, or it is refused.
4. **`Host` must name the server:** an IP literal, `localhost` or a name
   under it (Imbue Studio opens an app at
   `<label>.host-<hex>.localhost:8421`), or a name listed in
   `DATALIB_ALLOWED_HOSTS` (a share gateway passes the browser's `Host`
   through, so default-workspace-template lists `*.<share domain>`). The
   port is not checked. This defeats DNS rebinding, which today, in
   proxy mode, lets a page forge `X-Imbue-Identity: {"owner":true}`.
   The applets already check `Host` the same way (`applets.md`).

The case table (method × `Sec-Fetch-Site` × `Sec-Fetch-Mode` × credential
× proxy mode × `Host`) is the unit test.

Rule 1 relies on two more things, which this work checks rather than
assumes: every document served as data keeps its `sandbox` CSP
(`DATA_DOCUMENT_CSP`, already true), and **no `GET` route has a side
effect**. A navigation from any site may land on any path with the
cookie attached.

### 3.3 Guardrails

- **A lint** in `scripts/lint_repo.py` refuses, in `datalib/ui/src`:
  `new Function`, `eval(`, and `import(` of a non-literal anywhere; and
  `innerHTML =`, `v-html`, `insertAdjacentHTML`, `document.write`
  outside a short allowlist, with the reason written next to each entry.
- **A hostile email in the TNG fixture**, made up like the rest. Its
  body carries the tricks so far: a full-page relative link to card
  source, a link to `/api/…`, a `javascript:` href, an `<iframe
  src="blobs/…">`, a malformed `datalib:` link. A Playwright spec opens
  it, clicks across it, and asserts that no card ran code from a URL and
  that no request reached `/api/config`, `/api/lib` or `/api/requests`.
  Adding an email moves the fixture's counts and goldens; the same change
  regenerates them through their `.update` targets.
- **Docs:** a paragraph in `AGENTS.md`, and `cards.md` describes card
  source as the grammar above, with the "opening a card never writes"
  rule.

## 4. Order

1. §3.1, card source as a call. It closes the 2026-10-19 hole from both
   doors, the email link and the web page.
2. §3.2, the request policy.
3. §3.3, the lint and the hostile email. Both fixes are in by then, so
   the spec lands passing.

## 5. Not adopted

- **`Machine` vs `Page` authority** (the first version's §3.1): only a
  request carrying the bearer token could write a step's `command`,
  `env` or `binary_dir`, or `PUT /api/lib`. It limits what the *next*
  XSS can do, not this one, and it costs the config editor's command
  editing, the gallery's agent seed flow, and possibly agents inside
  Studio and Minds. Not adopted; revisit if another in-origin script
  bug turns up.
- **One `classifyHref` chokepoint for every link** (§3.3 there). With
  §3.1 in, an in-app link can no longer run code, so refusing unknown
  same-origin links buys little, and as written it refused every card
  link in a document and every `file:` link. The simpler model, which
  this work does not build: a same-origin link goes to the card
  mechanism, and the layout (solidified or not) decides where it opens;
  anything else goes to one "let the OS open it" function that accepts
  `http`, `https`, `mailto` and `tel`.
- **Writing a message's `↗` link only for `http(s)` and `mailto`.** The
  body of the same message can carry any link, so checking this one
  field gains nothing.
- **Redirecting navigations from other sites to `/`.** It only mattered
  while a URL could carry code, and it broke card links clicked on a web
  page.
- **Running card code in a sandboxed frame.** §3.1 removes the evaluator
  instead.

## 6. Open

- **`GET` routes with side effects.** §3.2's rule 1 lets a navigation
  from any site reach any path with the cookie, so it assumes none. The
  audit done with §3.2 found two that break it:
  - `GET /api/remote_media` fetches a caller-named remote URL into the
    content store (up to 50 MB), and its allow-list check trusts the
    `source=` and `document=` the caller supplies without checking the
    URL appears in them.
  - The search applet's first free-text query starts `qmd mcp` and a
    node embedder.

  Smaller ones: `?refresh=1` on `/api/manage/rows` and
  `/api/pipeline/storage` walks the data root; reading a finished
  connect or probe result drops it from memory. The first two are the
  next work.
