# The sign-in suite

Playwright specs for the wizard's latchkey flows — pasting a key,
"Sign in with browser", Check connection, a picker's Load, a latchkey
gateway — run against a real `datalib-http`, the real Bazel-pinned
latchkey, the real curl router and curl-impersonate, and a real browser
that latchkey's own `ensure-browser` finds. What is faked is what lies outside the machine
or would touch the person running it.

```sh
bazel run //datalib/ui:e2e_auth                  # from the source tree; edit and re-run
bazel run //datalib/ui:e2e_auth -- -g Claude     # one test, Playwright's own flags after --
bazel test //datalib/ui:e2e_auth_test            # as CI runs it (manual: not in //...)
DATALIB_TEST_AUTH_KEEP=1 bazel run //datalib/ui:e2e_auth   # keep every world's files
```

It is not on the default `bazel test //...` path: each test starts a
backend and often a Chromium. `.github/workflows/latchkey-auth.yml` runs
it on a macOS runner, nightly and on demand.

## A world per test

`harness.mjs` gives every test its own world, so the files run in
parallel and nothing is shared:

- an empty data root with the starter config (`POST /api/config/init`),
  served by its own `datalib-http`;
- a latchkey store in a temp directory, `LATCHKEY_DIRECTORY`, with a
  random `LATCHKEY_ENCRYPTION_KEY` — so latchkey never reads the
  keychain;
- its own `HOME` and `DATALIB_CACHE_DIR`, so nothing falls back to the
  developer's.

A failed test attaches the latchkey runs, the requests the fake
internet saw, and the world's directory, which holds the backend's log.

## What stands in for what

| Real thing | In the suite |
|---|---|
| The third-party sites | `fake_sites.mjs`: one TLS server that answers by `Host` for `slack.com`, `claude.ai`, `chatgpt.com` and `connectapi.garmin.com`, in made-up TNG data. Only the endpoints the probes call, in the shapes they read. Anything else is a 404, Chrome's own background traffic included, so nothing leaves the machine. |
| The network, for curl | `LATCHKEY_CURL` is a two-line script that logs its argv and runs the real `latchkey-curl-router` with `--connect-to ::127.0.0.1:<port> -k`. latchkey still sees the real URL, so its own service definitions match; the router still sends the hosts datalib marks through curl-impersonate (`expectImpersonated` checks the User-Agent that arrives). |
| The browser | latchkey's real `ensure-browser` runs, with the sources the backend passes. `fake_node.mjs` then wraps whatever it found — the same binary, plus `--headless=new` and `--host-resolver-rules` pointing every host at the fake internet. The browser config is written only when the backend asks for one, so a login that skips `ensure-browser` fails. |
| The bundled `node` | The staged runtime's `node` runs `fake_node.mjs` before each latchkey: it logs the run (`world.latchkeyRuns()`), refuses one that has neither a key nor a gateway (it would open the real keychain), and does the browser wrapping above. |
| A misbehaving service | `internet.override(host, fault)` with a handler from `faults.mjs`: a refused credential, Cloudflare's challenge, a 429, a 503, a body that is not JSON. `faults.spec.ts` runs every source against each, and against no credential, no network (`offline`) and a stopped gateway (`gatewayDown`). |
| A person's own latchkey plugin | `world.installOwnGarminPlugin()` copies the vendored Garmin plugin into the world's store without datalib's stamp, so a spec can check a sign-in leaves it alone. |
| A list that pages | `internet.hold(predicate)` keeps matching requests unanswered until the spec releases them, so a half-loaded list is a state the spec waits for. |
| A machine with no browser | `worldOptions: { browser: "download" }`: discovery fails with latchkey's own error, and the download step "fetches" Playwright's Chromium once the spec calls `world.releaseDownload()`. `"none"`: the download fails too. The real download, a few hundred MB, is never run. |
| Minds' gateway | `worldOptions: { gatewaySeed }` starts a real `latchkey gateway` with its own store; the backend then gets `LATCHKEY_GATEWAY` and nothing else. |

Not covered yet: the real keychain, Fastmail's and Google's OAuth
logins, and a headed browser. The plan for those is
`docs/dev/plans/latchkey_auth_testing.md`.

## Writing a spec

- Use `test`, `expect` and the helpers from `world.ts`; `baseURL` is the
  world's backend.
- A behaviour the product does not have yet is a spec with
  `test.fail()` and a comment saying what fails today. It flips to a
  failure the day the product is fixed, which is the cue to remove it.
- Assert on what a person sees and on what crossed a boundary: the
  latchkey runs, the requests the fake internet saw. Not on timing.
