# The impersonating curl

Some of the hosts datalib mirrors — `claude.ai` and `chatgpt.com`
among them — sit behind Cloudflare's bot wall, which rejects any client
whose TLS handshake does not look like a browser's. A stock `curl` gets
a `403` with `cf-mitigated: challenge`, whatever cookies it carries. So
requests to those hosts go out through a curl that presents Chrome's
TLS and HTTP/2 fingerprint: upstream
[`curl-impersonate`](https://github.com/lexiforest/curl-impersonate),
a patched curl with a patched BoringSSL and a built-in
`--impersonate <browser>` flag. Which services send the marker that
asks for it is decided in one place, `HttpService::impersonates` in
`datalib/backend/etl/web/src/http.rs`: Claude, ChatGPT, Slack, GitHub and
GitLab.

The two binaries involved are built and released by
[`imbue-ai/latchkey-curl-shims`](https://github.com/imbue-ai/latchkey-curl-shims);
its README is the reference for what each does, how the impersonator is
built from source, and how its upstream pin is bumped. This page is
about how datalib consumes that release and what in this tree depends
on the two names.

## The three pieces

| | |
|---|---|
| `latchkey-curl-router` | The `curl` that `LATCHKEY_CURL` points at. An invocation carrying the private `X-Imbue-Impersonate` header is rewritten — that header and any `User-Agent` dropped, `--compressed --noproxy '*' --impersonate <profile>` put in front — and handed to the impersonating curl; anything else goes to the system curl untouched. `--noproxy '*'` keeps `HTTP(S)_PROXY` and the macOS proxy pane away from a request that carries credentials. It can also send a request out through the latchkey gateway on the user's own computer, chosen by URL from the file `LATCHKEY_DESKTOP_PROXY_CONFIG` names; datalib plays no part in that decision. |
| `curl-impersonate` | Upstream `curl-impersonate`, unmodified. It ships next to the router in every release tarball and in the app bundle, and the router finds it only as a sibling of that exact name. **On its own it does not impersonate**: without `--impersonate` it is a plain curl 8.x and gets the same `403` as one. |
| `DATALIB_IMPERSONATE_PROFILE` | The `--impersonate` target, default `chrome150`. The marker's value plays no part: datalib sends `1`, and `X-Imbue-Impersonate: chrome131` still gets `chrome150`. Passed through as-is; a name curl-impersonate does not know fails the request with exit 43 and a message naming a valid one. |

This table describes the router this tree pins (`LATCHKEY_CURL_SHIMS_RELEASE`,
below), which is the one the desktop app and a local run use. A request
sent through a latchkey gateway (`LATCHKEY_GATEWAY` set) meets the router
on the gateway's machine instead. This tree does not pin that one, so it
can be a later release, and from v0.7.1 a router hands the reply on as the
server sent it and leaves the decoding to its caller. That is why datalib
passes `--compressed` beside the marker on every impersonated request.
With the pinned router the flag is simply given twice.

Only the router understands the marker header, so **point
`LATCHKEY_CURL` at the router, never at the impersonator directly.**
Pointing it at the impersonator gives you a plain curl that forwards
the marker header to the third party. Leaving `LATCHKEY_CURL` unset is
usually right: `datalib_etl_web::latchkey::ensure_curl_router` takes
`DATALIB_CURL_ROUTER` if set, else finds the router in bazel's
runfiles, next to the running binary, or on `PATH`, and points
`LATCHKEY_CURL` at it (not in `LATCHKEY_GATEWAY` mode).

To make one hand-run request impersonate, pass the marker yourself:

```sh
bazelisk build //third-party/latchkey-curl-shims
LATCHKEY_CURL="$(bazelisk info bazel-bin)/third-party/latchkey-curl-shims/latchkey-curl-router" \
    latchkey curl -sS --compressed -H 'X-Imbue-Impersonate: 1' https://claude.ai/api/organizations
```

## Where the binaries come from

`latchkey-curl-shims` builds the impersonator from source in its own CI
and pins the upstream commit it builds; the router is a small Rust
program in the same repo. Each release ships one
`latchkey-curl-shims-<triple>.tar.gz` per platform holding both
binaries and the license notices of everything linked into the
impersonator, plus a `SHA256SUMS`. Every tarball, and each binary
inside it, carries a build provenance attestation (a SLSA statement
signed through Sigstore and stored with that repo) tying its bytes to
the workflow, commit and run that produced it:

```sh
gh attestation verify latchkey-curl-shims-<triple>.tar.gz -R imbue-ai/latchkey-curl-shims
```

In this tree:

1. `MODULE.bazel` declares the six tarballs as `http_archive`s, under
   `LATCHKEY_CURL_SHIMS_RELEASE`, with their sha256s.
2. [`//third-party/latchkey-curl-shims`](../../third-party/latchkey-curl-shims/BUILD.bazel)
   selects the archive for the target platform and copies the two
   binaries out under their public names, side by side, which is how
   the router finds the impersonator. That package is what every
   consumer names: the runfiles of `datalib_etl_web` (so every web
   source's tests have both), `//datalib/backend:dist`, release
   staging, the Tauri sidecar list and the Docker image.

Nothing here is compiled. The repo's old `curl-impersonate-v*` tags
are ignored by `tools/workspace_status.sh` when it derives datalib's
own version.

## Bumping the pin

When `latchkey-curl-shims` publishes a new release — a newer Chrome
profile, a router change:

1. Download the six tarballs and `SHA256SUMS` from the release, check
   the sums, and run `gh attestation verify` on each tarball (above).
   The sha256s in `MODULE.bazel` are what every later build trusts, so
   this is the moment to be sure they name attested bytes.
2. Set `LATCHKEY_CURL_SHIMS_RELEASE` in `MODULE.bazel` to the new tag
   and copy the six sha256s in.
3. `bazelisk build //third-party/latchkey-curl-shims` and run one
   impersonating request (above) to see it work.
4. If the release changed the default profile, nothing here needs to
   follow: `DATALIB_IMPERSONATE_PROFILE` is only an override.
5. Read the router's source at the new tag against §"The three pieces"
   and correct that section, which says what the pinned release does.

Moving from v0.2.1 to v0.7.1 or later changes what the router does, not
only which build you get:

- **It honours proxy settings on an impersonated request.** v0.2.1 refuses
  them on purpose, because the request carries credentials, so decide this
  one rather than inheriting it.
- It drops the caller's browser-identity headers as well as `User-Agent`:
  `Accept-Encoding`, `Accept-Language`, client hints and `Sec-Fetch-*`.
- It leaves the decoding to its caller, which datalib already handles.
- A profile named in the marker selects that profile, and `1` means the
  default.
- `LATCHKEY_DESKTOP_PROXY_CONFIG` is keyed by latchkey service name, not
  by base URL.

The names are the contract. If a release ever renames a binary, the
sibling lookups in `datalib/backend/etl/web/src/latchkey.rs` and the copy
rules in `third-party/latchkey-curl-shims/BUILD.bazel` have to move
with it, and so do the staged names in `release.yml`,
`datalib/tauri/tauri.conf.json` and `datalib/docker/Dockerfile`.
