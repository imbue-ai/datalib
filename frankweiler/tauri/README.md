# Frankweiler Tauri shell

Tauri v2 bin crate (bundle identifier `com.imbue.frankweiler`). On
launch a native folder picker asks for the data root (seeded from
`~/.config/frankweiler/config.yaml` when present); the app then boots
`frankweiler-http`'s axum router **in-process** on an ephemeral
127.0.0.1 port and opens the main window at that URL. That server
serves both the rust-embed'd Vue UI and `/api/*`, so the UI's relative
`fetch('/api/…')` transport works unchanged — same code as the hosted
packaging, two front doors.

**Not owned by Bazel** — this crate is a standalone cargo workspace (see
the `[workspace]` table in `Cargo.toml`) so that Bazel's crate_universe,
which ingests `frankweiler/backend`'s workspace via `crate.from_cargo`,
never has to resolve the tauri dependency tree. Drive it with cargo/pnpm:

```sh
# Run it — one command. Builds the doltlite archive (bazel) + UI bundle
# (pnpm) via the config's beforeBuildCommand, compiles, bundles the
# .app, and launches it. Optional data-root arg skips the folder picker.
./run.sh
./run.sh ~/Documents/mixed-up-files

# Release bundle → target/release/bundle/macos/Frankweiler.app.
pnpm dlx @tauri-apps/cli@^2 build

# Compile-only inner loop (no bundling), for a fast type/borrow check.
# Requires ../ui/dist to exist (`pnpm --dir ../ui build`) because
# tauri::generate_context! embeds it. Note: on macOS this bare binary
# has no app context, so `cargo run` can't present the native folder
# picker (it spins) — launch the bundled .app instead, or pass a data
# root so boot skips the picker: `cargo run -- ~/root`.
cargo build
```

The window always points at the in-process backend serving the embedded
UI, so Tauri's own dev-server (`devUrl` / `beforeDevCommand`) is unused —
there is no `tauri dev` Vite workflow here. Boot takes a data root from
the first positional arg or `$FRANKWEILER_DATA_ROOT`; with neither set it
falls back to the native folder picker.

`icons/` is generated from `app-icon.png` (placeholder) via
`pnpm dlx @tauri-apps/cli icon app-icon.png -o icons`.

## v0 status

- Full backend embedded: grid, search, chat preview, sync API all work
  against the picked data root. Canceling the picker exits the app.
- The Bazel-built **doltlite** archive is statically linked (via the
  `SQLITE3_LIB_DIR` override in `.cargo/config.toml` — same mechanism
  as MODULE.bazel's libsqlite3-sys annotation), so the shell can open
  real synced data roots (`backend_index.doltlite_db` is doltlite's
  own on-disk format, which stock SQLite rejects) and feedback
  `dolt_commit`s work. Plain `cargo build` therefore needs
  `bazelisk build //third-party/doltlite:sqlite3` to have run first;
  `tauri build`'s beforeBuildCommand does it for you.
- qmd startup failure degrades search (warning dialog) instead of
  hard-failing like the standalone `frankweiler-http` binary; no
  `qmd pull` at startup, so first-search model downloads happen lazily.
- The compile-time `$FRANKWEILER_UI_DIST` for the embedded UI comes
  from `.cargo/config.toml` (Bazel sets it via `rustc_env` instead).
- Deep-link handler (`frankweiler://` via `tauri-plugin-deep-link`)
  is not wired yet; it will forward to
  `frankweiler/ui/src/router/deeplink.ts`.
