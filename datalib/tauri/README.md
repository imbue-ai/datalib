# Datalib Tauri shell

Tauri v2 bin crate (bundle identifier `com.imbue.datalib`). On launch
the **libraries screen** (`launcher-dist/index.html`) asks which
library to open — one it lists, an existing folder via the native
picker, or a new one — and the app then
spawns the bundled **`datalib-http` binary** — the same binary the
web packaging runs — on an ephemeral 127.0.0.1 port and opens the main
window at that URL. That server serves both the rust-embed'd Vue UI and
`/api/*`, so the UI's relative `fetch('/api/…')` transport works
unchanged — same code as the hosted packaging, two front doors.

The backend is deliberately **not** linked in-process: the shell is a
thin process manager, so there is no backend crate graph in this cargo
workspace, no doltlite static-link plumbing, and no drift between what
the web and desktop packagings run. `datalib-http`, `datalib-dag`,
`datalib-step`, `datalib-applet`, `datalib-migrate-config`,
`datalib-doltlite`, the two latchkey curl binaries and the `latchkey`
launcher are Bazel-built (fully cached) and shipped under the .app's
`Contents/Resources/binaries/`, with the Node runtime beside them
(`stage-runtime.sh`); see `tauri.conf.json`'s
`beforeBuildCommand` + `bundle.resources` and `resolve_bundled` in
`src/main.rs`. Port handshake: the child gets
`DATALIB_BIND=127.0.0.1:0` and `--url-file <tmp>` and announces its
bound URL there; the shell polls for the file, opens the window, and
kills the child on exit. That announced URL carries the backend's
per-process API token as `?token=…` (every route requires it — see
`datalib/backend/http/src/auth.rs`); the webview trades it for an
HttpOnly session cookie on the first load and is redirected to the
clean URL, so nothing else in the shell has to know about auth. The
url-file (tightened by the backend) and the child's log (by the shell)
are both mode 0600, since both end up holding that token in a shared
temp dir.

**Not owned by Bazel** — this crate is a standalone cargo workspace (see
the `[workspace]` table in `Cargo.toml`) so that Bazel's crate_universe,
which ingests `datalib/backend`'s workspace via `crate.from_cargo`,
never has to resolve the tauri dependency tree. Drive it with cargo/pnpm:

```sh
# Run it — one command. Bazel-builds the bundled binaries
# via the config's beforeBuildCommand, compiles the shell, bundles the
# .app, and launches it. Optional data-root arg skips the folder picker.
./run.sh
./run.sh ~/Documents/Datalib/Default
# It signs the bundle before launching it, ad hoc unless
# DATALIB_CODESIGN_IDENTITY names a signing identity. macOS remembers
# "allow access to Documents" for a signature that verifies: an ad-hoc
# one until the next rebuild, a real identity's across rebuilds.
DATALIB_CODESIGN_IDENTITY="Developer ID Application: …" ./run.sh

# Release bundle → target/release/bundle/macos/Datalib.app. The CLI is
# pinned by package.json + pnpm-lock.yaml here (never `pnpm dlx`, which
# resolves from the live registry — the signing job runs this with the
# Developer ID certificate in its environment).
pnpm install --frozen-lockfile --ignore-scripts && pnpm exec tauri build

# Signed + notarized release build (.app + .dmg) — the same script the
# release workflow's macos-app job runs in CI. Signing secrets come from
# Vault (restricted/datalib-release/*); they're under restricted/, so log
# in with the all-secrets role first:
#   vault login -method oidc role=employee_all_secrets
./build-signed-app.sh

# Compile-only inner loop (no bundling), for a fast type/borrow check —
# the shell has no backend deps, so this is seconds from cold. It does
# need the bundle resources staged (`binaries/`, `runtime/`), which the
# config's beforeBuildCommand does; a bare `cargo check` in a clean
# checkout fails with "resource path `binaries/…` doesn't exist" until
# then. Note: on macOS this bare binary has no app context, so the
# launcher's folder picker can't open under `cargo run` — launch the
# bundled .app instead, or pass a data root so boot skips the launcher,
# plus a backend to spawn since there's no bundle to find one in:
#   DATALIB_HTTP_BIN=$(bazelisk info bazel-bin)/datalib/backend/http/datalib_http_bin \
#     cargo run -- ~/root
cargo build
```

The window always points at the spawned backend serving its embedded
UI, so Tauri's own dev-server (`devUrl` / `beforeDevCommand`) is unused —
there is no `tauri dev` Vite workflow here, and `frontendDist` points at
`launcher-dist/`, whose single page is the libraries screen — the one bundled
page this shell has. Boot takes a data root from the first positional
arg or `$DATALIB_DATA_ROOT`; with neither set the libraries screen
opens and asks.

## The libraries screen

It lists the recent libraries (`~/.datalib/recent-roots.json`), then
any other library in `~/Documents/Datalib`. Each shows the source
count, size and last sync that `datalib-http` last wrote to its
`system/library-summary.json`; one whose folder is gone stays listed as
not found. A folder icon at the end of each row opens the library's
folder in Finder (`launcher_open_folder`, for a library only). A
library outside the Datalib folder, or one whose folder is gone, can be
forgotten, with an × left of the icon: it leaves the recent list and its folder is
left as it is. One in the Datalib folder is always listed while it is
there, so it has no Forget.

Before libraries each got a folder, the one library was
`~/Documents/Datalib` itself. When the screen finds a library there it
offers to move it into `~/Documents/Datalib/Default` (`move_legacy`:
every entry renamed into a staging folder, which is then renamed to
`Default`; it refuses if `Default` exists). Until it is moved, the
folder's subfolders are its own and are not listed as libraries, and
no new library goes inside it. This is temporary: a TODO in
`src/launcher.rs` says when to remove it. "New library" takes a name or a folder: a
name is a library in `~/Documents/Datalib`, the first one called
`Default`; `/…` and `~/…` are taken as they are, and "Create elsewhere…"
fills one in. A folder that is already a library is opened instead,
and one with other files in it is refused. The new library's server
starts with `--init`, so it opens on the Dashboard.

The app's top bar leads back here: "Data Liberation ✊" closes the
library and opens this screen, and the library's name opens a menu of
the other libraries to switch to (`library_menu`, `library_switch`, `libraries_show`, granted to the
app's page by `capabilities/switch-libraries.json`).

`src/launcher.rs` holds every decision the screen makes — which
libraries it lists, whether a directory is a data library at all,
where a new one goes and what is already there — and is
written **free of `tauri` and of every dependency but `serde_json`** on
purpose: it is compiled a second time, as its own crate, by
`//datalib/tauri:launcher_test`. `src/raw_store.rs` (what Browse does
with a raw store: open it read-only in DB Browser for SQLite or the
bundled doltlite shell) is kept free of `tauri` the same way, for
`//datalib/tauri:raw_store_test`. Those two targets are the only way
any of this crate's logic reaches `bazelisk test //...`, since the
shell is a standalone cargo workspace Bazel does not build. Anything in
either file that reaches for `tauri` breaks its target.

Picking a folder that turns out to be *empty* is not an error here: the
shell opens it, and the app's own first-run screen
(`ui/src/views/FirstRunView.vue`) explains what initializing it will
write before writing anything. Two screens, one each side of the
backend boundary — the shell knows about folders, the app knows about
configs.

Backend resolution at runtime: `$DATALIB_HTTP_BIN` (dev override,
point it at a fresh Bazel build without rebundling) → the bundled
`Resources/binaries/datalib-http`. The child finds the pipeline
binaries itself: `$DATALIB_BINARY_DIR` (inherited) → a sibling of its
own executable, which is exactly where
the bundle puts them. The spawned backend logs to
`$TMPDIR/datalib-http-<pid>.log`;
startup failures quote the log tail in the error dialog.

`app-icon.png` is ✊ on a rounded tile: `app-icon/app-icon.html` drawn
at 1024×1024 by headless Chromium. The fist is "Raised fist" from
Microsoft's [Fluent Emoji](https://github.com/microsoft/fluentui-emoji)
(MIT, `app-icon/LICENSE-fluentui-emoji`; the notice ships in the .app's
`licenses/fluentui-emoji/`). `icons/` is generated from it with
`pnpm exec tauri icon app-icon.png -o icons` (delete the `android/` and
`ios/` folders it also writes).

## Behaviour worth knowing

- The full backend runs against the chosen data root, one at a time,
  in one window: the main window shows the libraries screen, then the
  library's page, and goes back to the libraries screen when the
  library closes. Switching library stops the open one's server and
  closes the windows it opened. The window's navigation rules
  (`app_window`) read the open server's origin at each navigation,
  since each library's server has a port of its own.
- The main window reopens at the size (and maximized state) it was
  closed at, via `tauri-plugin-window-state`, which keeps it in
  `.window-state.json` under the app's config directory. It cannot be
  made narrower than 720px, so the toolbar's search box stays in view.
- No blocking model download at startup: qmd's models are fetched on
  first need by the steps and the search applet (`datalib_qmd_models`),
  the same as the web packaging — the shell passes nothing besides
  `--no-open` and the `--url-file` handshake.
- There is no `datalib://` deep-link handler; the shell does not link
  `tauri-plugin-deep-link`.
