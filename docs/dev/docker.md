# Running frankweiler in Docker

The `ghcr.io/imbue-ai/datalib` image bundles the four release
binaries (`datalib-dag`, `datalib-step`, `frankweiler-http`,
`latchkey-curl-impersonate`) and the `latchkey` CLI on top of an
Ubuntu 24.04 base, so you can register service credentials and run syncs
from a single self-contained container instead of dropping arbitrary
binaries onto your host PATH.

Published for `linux/amd64` and `linux/arm64`. The build is driven by
[`frankweiler/docker/Dockerfile`](../../frankweiler/docker/Dockerfile) and
published from [`.github/workflows/release.yml`](../../.github/workflows/release.yml)
on every `v*` tag.

## 🛑 Why this exists — and why you should care 🛑

`datalib-dag` exists to mirror conversations and personal data out of
services you are logged into (Slack, Anthropic, Notion, GitHub, GitLab,
…). The credentials those mirrors require are **live session cookies and
API tokens that confer the full power of your account on those
services**. Any process running as your user that can spawn
`datalib-dag` or read your `latchkey` store can therefore *act as
you* on those services with no further prompt, MFA, or confirmation gate.

The Docker image is here so you don't have to install these scary
binaries — or paste these scary credentials — directly into your host
shell environment. By isolating both the binaries and the credentials
inside a container with a deliberately narrow bind-mount contract, you
limit the blast radius to **just** the paths you explicitly map in.
That isolation only works **if you stick to the bind-mount layout
described below**. Bind-mounting `$HOME` or running the container with
`--privileged` defeats the entire point.

## Quickstart

```sh
IMG=ghcr.io/imbue-ai/datalib:latest
docker pull "$IMG"

# Pick host paths for the two bind mounts.
LATCHKEY_DIR="$HOME/.frankweiler-docker/latchkey"
DATA_ROOT="$HOME/datalib"
mkdir -p "$LATCHKEY_DIR" "$DATA_ROOT"

# Drop a config.yaml into the data root. config.yaml is the DAG `steps:`
# format (see docs/dev/step_protocol.md); the frankweiler-http Setup tab
# scaffolds/validates it and offers one-click migration of a legacy
# `sources:` config (GET /api/config/migrate).

# 1. Register a self-hosted service entry.
docker run --rm -it -v "$LATCHKEY_DIR:/root/.latchkey" "$IMG" \
    latchkey services register claude-ai --base-api-url=https://claude.ai/

# 2. Store the credential. Paste your live session cookie value into
#    your clipboard first (see docs/user/first_time_user.md section 2 for
#    where to copy it from).
docker run --rm -it -v "$LATCHKEY_DIR:/root/.latchkey" "$IMG" \
    sh -c 'latchkey auth set claude-ai -H "Cookie: sessionKey=$(cat)"'

# 3. Verify the credential is stored and decryptable.
docker run --rm -v "$LATCHKEY_DIR:/root/.latchkey" "$IMG" \
    latchkey auth list

# 4. Run a sync. Latchkey RO, data root RW.
docker run --rm \
    -v "$LATCHKEY_DIR:/root/.latchkey:ro" \
    -v "$DATA_ROOT:/data" \
    "$IMG" datalib-dag /data/config.yaml

# 5. Serve the HTTP backend (UI bundle not included in this image — point
#    a local Vite dev server or another openhost UI container at
#    http://127.0.0.1:8731/api).
docker run --rm -p 8731:8731 \
    -v "$LATCHKEY_DIR:/root/.latchkey:ro" \
    -v "$DATA_ROOT:/data" \
    "$IMG" frankweiler-http
```

## Bind-mount contract

| Host path                      | Container path             | Mode at sync time | Why                                                                                          |
|--------------------------------|----------------------------|-------------------|----------------------------------------------------------------------------------------------|
| `$LATCHKEY_DIR`                | `/root/.latchkey`          | `:ro`             | latchkey's encrypted credential store. Needs RW only during `services register` / `auth set` / `auth browser-prepare`. |
| `$DATA_ROOT`                   | `/data`                    | RW                | `config.yaml`, one directory per source stanza (`<name>/raw/` + `<name>/rendered_md/`), and the aggregates under `system/` (`system/backend_index/db.doltlite_db`, `system/qmd/`). |
| `~/.cache/qmd/models`          | `/root/.cache/qmd/models`  | RW (optional)     | qmd's embedding/reranker/expansion model cache (~2.25 GB). The image already ships the three default models pre-baked at `/root/.cache/qmd/models/`, so this mount is only needed if you've set `QMD_EMBED_MODEL=…` to override the default to an unbaked model, or you want to share a cache with a host `qmd` install. |

Default `ENV` inside the image already sets `FRANKWEILER_ROOT=/data` and
`LATCHKEY_CURL=/usr/local/bin/latchkey-curl-impersonate`, so
`frankweiler-http` finds the data root and `datalib-dag`'s download steps
find the Chrome-impersonating curl shim without further configuration.

## Latchkey encryption key — auto-provisioned inside the bind mount

On a host install, latchkey gets its symmetric encryption key from the
OS keyring (macOS Keychain, Linux Secret Service). Inside this image,
neither is available — there's no Secret Service running in a minimal
Ubuntu container. To avoid asking every user to invent and
persist a key by hand (lose the key, lose the credentials), the
container entrypoint
([`frankweiler/docker/entrypoint.sh`](../../frankweiler/docker/entrypoint.sh))
auto-provisions one on first run:

- If `LATCHKEY_ENCRYPTION_KEY` is already set in the env when you `docker
  run`, that value wins. Use this for ephemeral / no-persistence runs.
- Otherwise, the entrypoint looks for `/root/.latchkey/encryption_key`
  inside the bind mount. If absent, it generates one (`openssl rand
  -base64 32`, mode `0600`) into that file. Either way, the file's
  contents are exported as `LATCHKEY_ENCRYPTION_KEY` before `latchkey` /
  `datalib-dag` runs.
- This means the key is bound to the host directory you bind-mount.
  Move the dir, take the key with you. Lose the dir, lose the
  credentials.

**Security caveat:** the key lives in the same directory as the encrypted
`*.enc` blobs it protects. This is roughly equivalent to running a
desktop Linux box with an auto-unlocked keyring — protection against
accidental disclosure of `credentials.json.enc` alone, but no defense
against an attacker who can read both files. Keep the host directory
mode-restricted (`chmod 700 "$LATCHKEY_DIR"`) and treat it with the
same care you'd treat a file of bearer tokens, because that's
effectively what it is.

## Latchkey credential portability — IMPORTANT

`latchkey` encrypts `~/.latchkey/credentials.json.enc` and
`~/.latchkey/browser_state.json.enc` using a key obtained via
`@napi-rs/keyring`. On a host macOS install, that key is held by the
macOS Keychain; on a desktop Linux install, by the Secret Service
(GNOME Keyring, KWallet, …); inside this container, by latchkey's
file-based fallback (no Secret Service is running in a minimal Ubuntu
container).

The fallback uses a file living alongside the encrypted blobs, so:

- **A `~/.latchkey` directory created inside this container can always be
  read back by this container** — including across `docker run`
  invocations, host reboots, and `docker pull` of a newer image tag,
  provided the bind mount points at the same host path each time.
- **A `~/.latchkey` directory you populated on your macOS or desktop
  Linux host probably CANNOT be decrypted inside this container.** The
  keyring entry the host wrote isn't reachable from inside the
  container, so the `.enc` blobs can't be opened. Symptom: `latchkey
  auth list` shows the credential as present but
  `latchkey curl …` (and therefore `datalib-dag`) fails to send the
  expected headers.

**Recommendation:** dedicate a fresh host directory (the docs above use
`$HOME/.frankweiler-docker/latchkey`) to this container's latchkey
store, and only ever populate it via `docker run …  latchkey auth set
…`. Don't try to bind-mount your existing host `~/.latchkey`.

The CI release pipeline runs an end-to-end roundtrip
(`services register` → `auth set` → `auth list`) inside the freshly built
image on every tag push. If you ever see that smoke step fail in
[release.yml](../../.github/workflows/release.yml), latchkey's
encryption-at-rest behavior has changed and this whole flow needs
revisiting.

## What does NOT work inside the container

- **`latchkey auth browser <service>`.** The browser flow launches
  Playwright-driven Chrome and needs a display. The container has no X
  server and no Playwright browsers baked in. Use the header-paste
  flows (`latchkey auth set <service> -H "…"`) instead. The header
  values you need are documented in the `datalib-dag` error output
  when a service is missing credentials, in each provider's `DOWNLOAD.md`
  under `frankweiler/backend/etl/providers/<name>/`, and in
  [docs/user/first_time_user.md](/docs/user/first_time_user.md).
- **Tauri desktop UI / Vite dev server.** This image is the backend
  only. To serve the UI in a browser, run `frankweiler-http` (port
  8731) and point the upstream openhost UI container or a local
  `pnpm dev` at it.

## Permissions, signals, file ownership

The image runs as `root` inside the container so that latchkey's
keyring fallback file (which lives under `/root/.latchkey`) and the
data root (`/data`) are owned by a predictable user. Files written into
the bind-mounted host paths will be owned by `uid=0` on the host. If
that's awkward (e.g. you want to edit `config.yaml` from your normal
host user without `sudo`), pass `--user $(id -u):$(id -g)` on every
`docker run` — but be aware that latchkey may need to recreate its
internal state under the new uid the first time.

PID 1 is [tini](https://github.com/krallin/tini), so `docker stop`
delivers SIGTERM cleanly to `datalib-dag` (which in turn forwards it
to its running step subprocesses).

## Building locally

```sh
# Pull tarballs from the latest tagged release and build for both arches
# (no push, just verify the build):
scripts/build_docker.sh

# Same, but load the host-native arch into your local docker daemon so
# you can `docker run ghcr.io/imbue-ai/datalib:<version>` it:
scripts/build_docker.sh --load

# Build against tarballs you produced locally via `bazel build
# //frankweiler/backend:dist` (and then named to match the release
# filenames):
scripts/build_docker.sh --tarball-dir /path/to/tarballs --load

# Push to your own registry:
REPO=your-fork/datalib \
IMAGE_NAME=ghcr.io/your-fork/datalib \
scripts/build_docker.sh --push
```
