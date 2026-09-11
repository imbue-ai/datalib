# The impersonating curl

Some of the hosts datalib mirrors — `claude.ai` and `chatgpt.com` today
— sit behind Cloudflare's bot wall, which rejects any client whose TLS
handshake does not look like a browser's. A stock `curl` gets a
`403` with `cf-mitigated: challenge`, whatever cookies it carries. So
requests to those hosts go out through a curl that presents Chrome's
TLS and HTTP/2 fingerprint: upstream
[`curl-impersonate`](https://github.com/lexiforest/curl-impersonate),
a patched curl with a patched BoringSSL and a built-in
`--impersonate <browser>` flag.

This page is about where that binary comes from, why it is built the
way it is, and how to change it. How a request *reaches* it is the
dispatch curl's job; see the header of
`datalib/backend/etl/src/bin/latchkey_curl_dispatch.rs`.

## The three pieces

| | |
|---|---|
| `latchkey-curl-dispatch` | The `curl` that `LATCHKEY_CURL` points at. Std-only Rust. An invocation carrying the private `X-Imbue-Impersonate` header is rewritten — that header and any `User-Agent` dropped, `--compressed --noproxy '*' --impersonate <profile>` put in front — and handed to the impersonating curl; anything else goes to the system curl untouched. |
| `latchkey-curl-impersonate` | Upstream `curl-impersonate`, unmodified, under the name everything in this tree looks for. It ships next to the dispatch in every release tarball and in the app bundle, and the dispatch finds it as a sibling of itself. **On its own it does not impersonate**: without `--impersonate` it is a plain curl 8.x and gets the same `403` as one. |
| `DATALIB_IMPERSONATE_PROFILE` | The `--impersonate` target, default `chrome150`. Passed through as-is; a name curl-impersonate does not know fails the request with exit 43 and a message naming a valid one. |

Only the dispatch understands the marker header, so **point
`LATCHKEY_CURL` at the dispatch, never at the impersonator directly.**
Pointing it at the impersonator used to work — the previous shim
impersonated unconditionally — and now gives you a plain curl that
forwards the marker header to the third party. Leaving `LATCHKEY_CURL`
unset is usually right: `datalib_etl::latchkey::ensure_curl_dispatch`
finds the dispatch in bazel's runfiles, in `bazel-bin`, or next to the
running binary.

To make one hand-run request impersonate, pass the marker yourself:

```sh
LATCHKEY_CURL="$(bazelisk info bazel-bin)/datalib/backend/etl/latchkey_curl_dispatch" \
    latchkey curl -sS -H 'X-Imbue-Impersonate: 1' https://claude.ai/api/organizations
```

## Where the binary comes from

**We build it from source, in our own CI, and pin what we built.**
Upstream publishes prebuilt binaries, and they are fine — but there is
no attestation tying those bytes to the source, and this binary is the
last process to hold a user's session cookie before it leaves the
machine. Building it ourselves makes every byte traceable to a commit
we named and a run whose log we own. It is also cheap: ~2 minutes per
leg, from a clean tree, with distro compilers.

The pieces, in the order they run:

1. [`third-party/curl-impersonate/pin.env`](../../third-party/curl-impersonate/pin.env)
   names the upstream tag **and commit** (the tag alone can move) and
   the release tag we publish under.
2. [`.github/workflows/curl-impersonate.yml`](../../.github/workflows/curl-impersonate.yml)
   clones that commit on each of the six runners `release.yml` uses and
   runs [`build.sh`](../../third-party/curl-impersonate/build.sh) —
   upstream's CMake build plus our flags, staged as one
   `curl-impersonate-<triple>.tar.gz` holding the binary and the license
   notices of everything linked into it. The musl legs run the same
   script inside an `alpine:3.21` container. The workflow then publishes
   the six tarballs as a GitHub release of this repo, with a
   `SHA256SUMS`. Upstream's CMake fetches curl, BoringSSL, nghttp2,
   nghttp3, ngtcp2, brotli, zstd and zlib by URL **with a pinned sha256
   each**, so nothing in the build is unpinned. `build.sh` runs by hand
   too (`build.sh <triple> <upstream checkout> <out dir>`; a mac leg
   takes about two minutes).
3. `MODULE.bazel` declares those six tarballs as `http_archive`s with
   their sha256s, and
   [`//third-party/curl-impersonate`](../../third-party/curl-impersonate/BUILD.bazel)
   selects the one for the target platform.
4. `//datalib/backend/etl:latchkey_curl_impersonate` copies it next to
   the dispatch under the installed name. Nothing else in the tree
   changed when the shim was replaced: release staging, the Tauri
   sidecar list and every provider test's runfiles all name that
   target.

Two build choices worth knowing about:

- **IDN is off** (`-DUSE_LIBIDN2=OFF`). We never fetch an
  internationalized hostname, and libidn2 is LGPL — the one component
  in upstream's own Linux builds that would have kept an MIT release
  from being clean.
- **The musl legs use a wrapper C compiler**,
  [`cc-static-cxx`](../../third-party/curl-impersonate/cc-static-cxx),
  which appends `-lstdc++` to link lines. BoringSSL is C++, curl is C,
  and upstream's CMake puts the C++ runtime *before* the objects — fine
  for a shared libstdc++, fatal for a static one. Upstream builds its
  musl legs with zig, whose driver orders the runtime itself; we prefer
  the distro gcc and order it in the wrapper.

Linux binaries read the host's CA store (`/etc/ssl/certs`), the way
the system curl does; `SSL_CERT_FILE` overrides it. macOS binaries use
the system trust store through Apple's Security framework.

**The musl legs are reproducible.** The `aarch64-unknown-linux-musl`
binary in release `curl-impersonate-v2.2.2-1`, built on a GitHub arm64
runner, is byte-for-byte identical (sha256 `adafa92f…dc28e7`) to one
built the same way in Docker on a Mac. So the pin is checkable by
anyone, not only trusted:

```sh
git clone --depth 1 --branch v2.2.2 https://github.com/lexiforest/curl-impersonate.git upstream
docker run --rm -v "$PWD:/work" -w /work -e UPSTREAM_COMMIT=<commit from pin.env> alpine:3.21 sh -c '
    apk add --no-cache bash ninja cmake make patch linux-headers build-base perl go file tar >/dev/null
    third-party/curl-impersonate/build.sh aarch64-unknown-linux-musl upstream out'
sha256sum out/curl-impersonate-aarch64-unknown-linux-musl/curl-impersonate   # compare with the release's
```

The Alpine image tag is what pins the compiler; a new `alpine:3.21`
point release could move the bytes. The darwin and linux-gnu legs
were not checked for this and are not expected to reproduce (Xcode and
Ubuntu toolchains move with the runner image).

## Bumping the pin

Chrome moves, and a stale profile eventually stops being camouflage.
When upstream tags a version with a newer `chromeNNN`:

1. **Read the delta.** The whole difference between this binary and
   stock curl + BoringSSL is `patches/` in the upstream repo. Diff
   those between the old and new tags. `boringssl.patch` is the one to
   read line by line — it is under a thousand lines, almost all in
   `ssl/` (extension order, GREASE, cipher lists), and anything under
   `crypto/` deserves a hard look. `curl.patch` is larger, but most of
   it is the profile table in `lib/impersonate.c`.
2. Edit `pin.env`: new `UPSTREAM_TAG` and `UPSTREAM_COMMIT`, `BUILD=1`,
   and `RELEASE_TAG=curl-impersonate-<tag>-1`.
3. Push the branch under `curl-impersonate/<anything>` so the workflow
   runs and publishes the release (the `workflow_dispatch` trigger only
   works once a workflow file is on `main`, which a pin bump on a
   branch is not).
4. Copy the six sha256s from the run's `SHA256SUMS` into
   `MODULE.bazel`, and set `CURL_IMPERSONATE_RELEASE` to the new tag.
   `//third-party/curl-impersonate:pin_test` fails until it matches
   `pin.env`.
5. Bump `DEFAULT_PROFILE` in the dispatch if the point was a newer
   Chrome, and check the JA4 against a real browser: load
   `https://tls.browserleaks.com/json` in Chrome and compare with what
   `latchkey-curl-impersonate --impersonate chromeNNN` gets from the
   same URL. JA4 is stable across handshakes (JA3 is not, because
   Chrome shuffles extension order); the `akamai_hash` is the HTTP/2
   fingerprint and should match too.

To rebuild the *same* upstream commit — a runner image changed, the
workflow was fixed — bump `BUILD` and the `-N` suffix of `RELEASE_TAG`
instead. A release tag that already exists fails the workflow rather
than overwriting bytes some checkout may already pin.

The release tags start with `curl-impersonate-`, not `v`, so `git
describe --match 'v[0-9]*'` in `tools/workspace_status.sh` ignores
them and `datalib-dag --version` keeps reporting datalib's own tag.

## What this replaced

Until 2026-09 the impersonating curl was `latchkey-curl-impersonate`, a
Rust program over the `wreq` HTTP client and its `boring2` BoringSSL
fork. Issue #134 has the audit of that stack and the reasons for
leaving it: a six-crate dependency tree from one author at `rc`
versions, of which the version pinned here turned out to carry an LGPL
crate (`wreq-util 3.0.0-rc.11`); and a TLS layer that upstream then
swapped wholesale (`wreq 0.16` replaced `boring2` with a new binding),
so keeping up meant a fresh audit rather than a diff. Measured on the
day of the switch, both presented byte-identical JA4 and HTTP/2
fingerprints for Chrome 131 and both passed Cloudflare on claude.ai and
chatgpt.com; curl-impersonate's newest profile additionally matched a
real Chromium 152 exactly.
