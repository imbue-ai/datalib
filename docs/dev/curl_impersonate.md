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

The two binaries involved are built and released by
[`imbue-ai/latchkey-curl-shims`](https://github.com/imbue-ai/latchkey-curl-shims);
its README is the reference for what each does, how the impersonator is
built from source, and how its upstream pin is bumped. This page is
about how datalib consumes that release and what in this tree depends
on the two names.

## The three pieces

| | |
|---|---|
| `latchkey-curl-router` | The `curl` that `LATCHKEY_CURL` points at. An invocation carrying the private `X-Imbue-Impersonate` header is rewritten — that header and any `User-Agent` dropped, `--compressed --noproxy '*' --impersonate <profile>` put in front — and handed to the impersonating curl; anything else goes to the system curl untouched. It can also send a request out through the latchkey gateway on the user's own computer, chosen by URL from the file `LATCHKEY_DESKTOP_PROXY_CONFIG` names; datalib plays no part in that decision. |
| `curl-impersonate` | Upstream `curl-impersonate`, unmodified. It ships next to the router in every release tarball and in the app bundle, and the router finds it only as a sibling of that exact name. **On its own it does not impersonate**: without `--impersonate` it is a plain curl 8.x and gets the same `403` as one. |
| `DATALIB_IMPERSONATE_PROFILE` | The `--impersonate` target, default `chrome150`. Passed through as-is; a name curl-impersonate does not know fails the request with exit 43 and a message naming a valid one. |

Only the router understands the marker header, so **point
`LATCHKEY_CURL` at the router, never at the impersonator directly.**
Pointing it at the impersonator gives you a plain curl that forwards
the marker header to the third party. Leaving `LATCHKEY_CURL` unset is
usually right: `datalib_etl::latchkey::ensure_curl_router` finds the
router in bazel's runfiles, next to the running binary, or on `PATH`,
and `DATALIB_CURL_ROUTER` names one explicitly.

To make one hand-run request impersonate, pass the marker yourself:

```sh
bazelisk build //third-party/latchkey-curl-shims
LATCHKEY_CURL="$(bazelisk info bazel-bin)/third-party/latchkey-curl-shims/latchkey-curl-router" \
    latchkey curl -sS -H 'X-Imbue-Impersonate: 1' https://claude.ai/api/organizations
```

## Where the binaries come from

`latchkey-curl-shims` builds the impersonator from source in its own CI
and pins the upstream commit it builds; the router is a small Rust
program in the same repo. Each release ships one
`latchkey-curl-shims-<triple>.tar.gz` per platform holding both
binaries and the license notices of everything linked into the
impersonator, plus a `SHA256SUMS`.

In this tree:

1. `MODULE.bazel` declares the six tarballs as `http_archive`s, under
   `LATCHKEY_CURL_SHIMS_RELEASE`, with their sha256s.
2. [`//third-party/latchkey-curl-shims`](../../third-party/latchkey-curl-shims/BUILD.bazel)
   selects the archive for the target platform and copies the two
   binaries out under their public names, side by side, which is how
   the router finds the impersonator. That package is what every
   consumer names: the runfiles of `datalib_etl` (so every provider
   test has both), `//datalib/backend:dist`, release staging, the
   Tauri sidecar list and the Docker image.

Nothing here is compiled. The workflow that used to build the
impersonator in this repo is gone, and the `curl-impersonate-v*` tags
that workflow published are history; `tools/workspace_status.sh` still
ignores them when it derives datalib's own version.

## Bumping the pin

When `latchkey-curl-shims` publishes a new release — a newer Chrome
profile, a router change:

1. Set `LATCHKEY_CURL_SHIMS_RELEASE` in `MODULE.bazel` to the new tag
   and copy the six sha256s from the release's `SHA256SUMS`.
2. `bazelisk build //third-party/latchkey-curl-shims` and run one
   impersonating request (above) to see it work.
3. If the release changed the default profile, nothing here needs to
   follow: `DATALIB_IMPERSONATE_PROFILE` is only an override.

The names are the contract. If a release ever renames a binary, the
sibling lookups in `datalib/backend/etl/src/latchkey.rs` and the copy
rules in `third-party/latchkey-curl-shims/BUILD.bazel` have to move
with it, and so do the staged names in `release.yml`, `tauri.conf.json`
and the Dockerfile.
