# Datalib Tauri shell

Tauri v2 bin crate (bundle identifier `com.imbue.datalib`). On
launch a native folder picker asks for the data root; the app then
spawns the bundled **`datalib-http` binary** — the same binary the
web packaging runs — on an ephemeral 127.0.0.1 port and opens the main
window at that URL. That server serves both the rust-embed'd Vue UI and
`/api/*`, so the UI's relative `fetch('/api/…')` transport works
unchanged — same code as the hosted packaging, two front doors.

The backend is deliberately **not** linked in-process: the shell is a
thin process manager, so there is no backend crate graph in this cargo
workspace, no doltlite static-link plumbing, and no drift between what
the web and desktop packagings run. `datalib-http`, `datalib-dag`,
`datalib-step` and the two latchkey curl binaries are Bazel-built (fully
cached) and shipped under the
.app's `Contents/Resources/binaries/`; see `tauri.conf.json`'s
`beforeBuildCommand` + `bundle.resources` and `resolve_http_bin` in
`src/main.rs`. Port handshake: the child gets
`DATALIB_BIND=127.0.0.1:0` and `--url-file <tmp>` and announces its
bound URL there; the shell polls for the file, opens the window, and
kills the child on exit.

**Not owned by Bazel** — this crate is a standalone cargo workspace (see
the `[workspace]` table in `Cargo.toml`) so that Bazel's crate_universe,
which ingests `datalib/backend`'s workspace via `crate.from_cargo`,
never has to resolve the tauri dependency tree. Drive it with cargo/pnpm:

```sh
# Run it — one command. Bazel-builds the bundled binaries
# via the config's beforeBuildCommand, compiles the shell, bundles the
# .app, and launches it. Optional data-root arg skips the folder picker.
./run.sh
./run.sh ~/Documents/datalib

# Release bundle → target/release/bundle/macos/Datalib.app.
pnpm dlx @tauri-apps/cli@^2 build

# Signed + notarized release build (.app + .dmg) — the same script the
# release workflow's macos-app job runs in CI. Signing secrets come from
# Vault (restricted/datalib-release/*); they're under restricted/, so log
# in with the all-secrets role first:
#   vault login -method oidc role=employee_all_secrets
./build-signed-app.sh

# Compile-only inner loop (no bundling), for a fast type/borrow check —
# the shell has no backend deps, so this is seconds from cold. Note: on
# macOS this bare binary has no app context, so `cargo run` can't
# present the native folder picker (it spins) — launch the bundled .app
# instead, or pass a data root so boot skips the picker, plus a backend
# to spawn since there's no bundle to find one in:
#   DATALIB_HTTP_BIN=$(bazelisk info bazel-bin)/datalib/backend/http/datalib_http_bin \
#     cargo run -- ~/root
cargo build
```

The window always points at the spawned backend serving its embedded
UI, so Tauri's own dev-server (`devUrl` / `beforeDevCommand`) is unused —
there is no `tauri dev` Vite workflow here, and `frontendDist` points at
a committed placeholder (`dummy-dist/`) that is never loaded. Boot takes
a data root from the first positional arg or `$DATALIB_DATA_ROOT`;
with neither set it falls back to the native folder picker.

Backend resolution at runtime: `$DATALIB_HTTP_BIN` (dev override,
point it at a fresh Bazel build without rebundling) → the bundled
`Resources/binaries/datalib-http`. The child finds the pipeline
binaries itself: `$DATALIB_DAG_BIN` / `$DATALIB_BINARY_DIR`
(inherited) → a sibling of its own executable, which is exactly where
the bundle puts them. The spawned backend logs to
`$TMPDIR/datalib-http-<pid>.log`;
startup failures quote the log tail in the error dialog.

`icons/` is generated from `app-icon.png` (placeholder) via
`pnpm dlx @tauri-apps/cli icon app-icon.png -o icons`.

## v0 status

- Full backend available: grid, search, chat preview, sync API all work
  against the picked data root. Canceling the picker exits the app.
- No blocking model download at startup: the backend pulls qmd models
  lazily (the sync path warms the shared cache; a cold cache pays a
  one-time download on the first semantic search). Same behavior as the
  web packaging — the shell passes nothing besides `--no-open` and the
  `--url-file` handshake.
- Deep-link handler (`datalib://` via `tauri-plugin-deep-link`)
  is not wired yet; it will forward to
  `datalib/ui/src/router/deeplink.ts`.
