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
`DATALIB_BIND=127.0.0.1:0` and `--url-stdout`, and announces its bound
URL as the first line of its stdout, a pipe only the shell holds; the
shell reads it, opens the window, and kills the child on exit. That
announced URL carries the backend's per-process API token as `?token=…`
(every route requires it — see `datalib/backend/http/src/auth.rs`); the
webview trades it for an HttpOnly session cookie on the first load and
is redirected to the clean URL, so nothing else in the shell has to know
about auth. The URL never goes through a file. A file's mode does not
keep out another process of the same user, and with the token that
process could read a library through the server without opening its
folder (`src/backend_output.rs`).

**Not owned by Bazel** — this crate is a standalone cargo workspace (see
the `[workspace]` table in `Cargo.toml`) so that Bazel's crate_universe,
which reads `datalib/backend/Cargo.toml` via `crate.from_cargo`,
never has to resolve the tauri dependency tree. Drive it with cargo/pnpm:

```sh
# Run it — one command. Bazel-builds the bundled binaries via the
# config's beforeBuildCommand, compiles the shell, bundles the .app, and
# launches it. Optional data-root arg skips the folder picker. Every
# build below bundles `-c opt` binaries, as the release tarballs do;
# DATALIB_COMPILATION_MODE=fastbuild builds quicker and syncs about
# twice as slowly.
./run.sh
./run.sh ~/Datalib/Default

# Release bundle → target/release/bundle/macos/Datalib.app. The CLI is
# pinned by package.json + pnpm-lock.yaml here (never `pnpm dlx`, which
# resolves from the live registry — the signing job runs this with the
# Developer ID certificate in its environment).
pnpm install --frozen-lockfile --ignore-scripts && pnpm exec tauri build

# Signed + notarized release build (.app + .dmg) — the same script the
# release workflow's macos-app job runs in CI. It also stamps the
# binaries (DATALIB_STAMP=1, i.e. `--config=release`, so `datalib-dag
# --version` names the tag rather than `dev`) and refuses a bundle whose
# binaries are not optimized and stamped
# (scripts/release/check_binaries.sh). Signing secrets come from
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
any other library in the libraries folder, `~/Datalib`, or in the
protected folder (below), by name. The libraries folder is not in
`~/Documents`, `~/Desktop` or `~/Downloads` because macOS asks before
an app reads those.
Each shows the source
count, size and last sync that `datalib-http` last wrote to its
`system/library-summary.json`; one whose folder is gone stays listed as
not found. A folder icon at the end of each row opens the library's
folder in Finder (`launcher_open_folder`, for a library only). A
library outside the libraries folder, or one whose folder is gone, can be
forgotten, with an × left of the icon: it leaves the recent list and its folder is
left as it is. One in the libraries folder is always listed while it is
there, so it has no Forget; it has a Delete in the same place, a trash
can (`launcher_delete`). Delete asks first, in a native dialog the shell
raises so the page cannot skip it, then moves the library's folder to
the Trash and takes it off the recent list. On macOS it trashes through
`NSFileManager` rather than by scripting Finder, so it needs no
permission to control Finder; the cost is that Finder may not offer
"Put Back" for it. It is refused while a
server or a sync run holds the library (`launcher::in_use` asks the
library's `system/lock` and `system/runner-lock`). A library outside
the libraries folder is never deleted from this screen. Once `Default`
is deleted, "New library" offers that name again.

Libraries used to go in `~/Documents/Datalib`. While the recent list
names a library there, each such library's row carries a "Legacy
location" tag and a banner under the list offers to move them all to
the libraries folder (`move_from_documents`: each such library is renamed
into the libraries folder under its own name, a library that is
`~/Documents/Datalib` itself becomes `Default`, and one whose name is
taken stays). Only the recent list says which libraries are there; the
app never lists `~/Documents/Datalib`, so a person with nothing there
is not asked by macOS for permission to read it. This is
temporary: a TODO in `src/launcher.rs` says when to remove it.

"New library" takes a name or a folder: a
name is a library in the libraries folder, the first one called
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
`//datalib/tauri:raw_store_test`, `src/protected.rs` (the protected
folder, below) for `//datalib/tauri:protected_test`,
`src/backend_output.rs` (the backend's URL and output) for
`//datalib/tauri:backend_output_test`, and `src/navigation.rs` (where a window may go) for
`//datalib/tauri:navigation_test`. Those
targets are the only way any of this crate's logic reaches `bazelisk
test //...`, since the shell is a standalone cargo workspace Bazel does
not build. Anything in those files that reaches for `tauri` breaks its
target.

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
the bundle puts them. Once the spawned backend has taken its library,
the shell keeps the backend's output in the library, in
`system/server-output.log`, and the previous run's in
`system/server-output.previous.log`. That is the only record of a panic.
Startup failures quote the output's last lines in the error dialog. An
earlier build kept this output in `$TMPDIR/datalib-http-<pid>.log`, with
the token in it; the shell removes any such file, and any
`datalib-http-<pid>.url`, when it starts a backend.

`app-icon.png` is ✊ on a rounded tile: `app-icon/app-icon.html` drawn
at 1024×1024 by headless Chromium. The fist is "Raised fist" from
Microsoft's [Fluent Emoji](https://github.com/microsoft/fluentui-emoji)
(MIT, `app-icon/LICENSE-fluentui-emoji`; the notice ships in the .app's
`licenses/fluentui-emoji/`). `icons/` is generated from it with
`pnpm exec tauri icon app-icon.png -o icons` (delete the `android/` and
`ios/` folders it also writes).

## A protected library

On macOS 15 or later, "New library" can put the library in a folder only
Datalib opens without asking: the app group container
`~/Library/Group Containers/LDDYAR29MP.com.imbue.datalib/Libraries/`.
macOS guards a group container: any other app that reaches in
(Terminal, an AI agent's shell, Finder) makes it ask the person first,
and a yes lasts as long as that app runs. An app with Full Disk Access
is the exception: a Terminal holding it got in with no prompt, and so
can whatever that Terminal runs. An app gets into its own
group container without asking when the group's name starts with the
Team ID that signed it, so only the Imbue-signed app can use this; a
build from source is asked too, or refused. The option is marked
experimental, and is meant to stop an agent running shell commands as
you from reading the library or using its sign-ins, not a program set
on stealing from you.

> **The keychain does not protect a protected library's sign-ins. Only
> the folder does.** A library's keychain item lets in the app's Node,
> and macOS judges a program by who signed it, not by who started it. So
> any program running as you, an AI agent's shell included, can run that
> Node and get the key without a prompt. All that stands between that
> program and the sign-ins is macOS keeping it out of
> `<library>/system/latchkey`. So anything that lets a program reach
> those files gives it the sign-ins too: saying **Allow** for that
> program, Full Disk Access, or any copy of the files outside the folder.
> That is why deleting a protected library skips the Trash. Check any
> change to where this store lives, or who may read it, against this.

- **Before macOS 15 the folder is not offered.** macOS only guards a group
  container from 15 on, so on an earlier version the option is greyed out
  and says why, and the shell refuses to make one
  (`make_protected_container`). A library already in the folder, made by
  an earlier build or on a newer macOS, still opens with its own latchkey
  store, and its row says "Not protected on this macOS".
- **The server's output stays in the library** (`system/server-output.log`),
  since a log line can quote what a library holds. The URL with the token
  stays out of it. The server prints that URL to stderr only at a terminal,
  and hands it to the shell over a pipe.
- **Only the app's own executable is entitled** (`Datalib.entitlements`,
  `bundle.macOS.entitlements` in `tauri.conf.json`). macOS counts the
  programs an app starts as that app, so `datalib-http` and everything
  it runs reach the library because the app started them. If macOS
  judged each program by its own signature instead, an entitled
  `datalib-doltlite` would let anyone who runs it in; unentitled, the
  library fails to open instead, which says so.
- **macOS makes the folder**, the first time the app asks for it
  (`make_protected_container`, `NSFileManager`); the shell only ever
  creates libraries inside the one it names.
- **Each library has a latchkey store of its own, and starts empty.**
  The server of a protected library starts with `LATCHKEY_DIRECTORY`
  naming `<library>/system/latchkey` and
  `LATCHKEY_KEYRING_SERVICE_NAME` naming a keychain item of its own,
  `Datalib Latchkey - <folder name>`, and every latchkey it runs
  inherits both (`protected::latchkey_env`). Nothing comes from
  `~/.latchkey` or latchkey's shared `latchkey` item, which stay as they
  were; a person signs in through the wizard. latchkey makes the item
  the first time it needs a key, and the program that makes an item is
  on its list, so macOS has no reason to ask.
- **The wizard offers no terminal command in one.** A latchkey run from
  a terminal has neither variable, so it signs in to `~/.latchkey`. The
  server tells the wizard through `protected_library` in `/api/health`,
  set by `DATALIB_PROTECTED_LIBRARY`. A source whose only sign-in is
  those commands (CalDAV and CardDAV on a server of your own) says it
  cannot sign in here yet.
- **Deleting one is permanent.** After a confirmation that says so, the
  app removes the folder, then the keychain item with
  `/usr/bin/security`, and nothing goes to the Trash, because there the
  sign-ins would sit outside the folder with the key still available
  (`launcher_delete`). If only the item is left, the app says so and
  names it.
- **The item goes by the folder's name.** A folder given a new name
  looks for a new item, and latchkey refuses its store until the old
  name is back.
- **An earlier build copied `~/.latchkey` to `<container>/latchkey`.**
  Nothing reads that copy now, so a library that used it signs in again;
  its raw stores are untouched. The copy stays where it is until someone
  removes it from Terminal, and macOS asks first.
- **Nothing hands it to another app.** The folder button is disabled on
  a protected row, and Browse refuses a protected library's raw stores
  (`refuse_if_protected`): Finder or Terminal would have to be let in,
  and stay in.
- **The release checks the signed app starts** (`datalib-tauri
  --version` in `build-signed-app.sh`): macOS kills a program whose
  signature cannot back its entitlements, before any window.

What the first release with it should be checked for, on a Mac
(#1525): the app makes and opens a protected library with no prompt;
syncs and sign-ins work in it, with no keychain prompt, and the item
`Datalib Latchkey - <folder name>` appears in Keychain Access; from
Terminal, `ls` of the folder and
each of `Resources/binaries/` run directly get a prompt naming
Terminal rather than getting in; one Allow for Terminal lets in
everything that Terminal runs until it quits. Run those checks from a
Terminal without Full Disk Access: one that holds it got in with no
prompt (macOS 26.7.2), so it proves nothing. A person can list what they have
granted with the query in `docs/user/first_time_user.md`, "Which apps
have you let in?".

## Behaviour worth knowing

- The full backend runs against the chosen data root, one at a time,
  in one window: the main window shows the libraries screen, then the
  library's page, and goes back to the libraries screen when the
  library closes. Switching library stops the open one's server and
  closes the windows it opened. The window's navigation rules
  (`app_window`, deciding by `src/navigation.rs`) read the open
  server's origin at each navigation, since each library's server has a
  port of its own. A window shows that server's pages, the libraries
  screen, and a frame's `about:srcdoc` or `about:blank`; a web, mail or
  phone link goes to the OS; every other scheme, a file dropped on the
  window among them, goes nowhere. A link asking for a new window gets a
  second window of the app only for one of the app's pages.
- View → Zoom In / Zoom Out / Actual Size (⌘= or ⌘+, ⌘−, ⌘0) zoom the
  page, as in a browser: the way to make text larger, since the status
  bar's density moves spacing only. The shell holds the level
  (`src/zoom.rs`), applies it to every window and again after each page
  load — so it survives the move from the libraries screen to a library
  — and keeps it in the app's config directory for the next launch.
  Tauri's `zoom_hotkeys_enabled` is not used: on macOS it injects a
  script whose count of the level starts again at 100% on every page.
- The main window reopens at the size (and maximized state) it was
  closed at, via `tauri-plugin-window-state`, which keeps it in
  `.window-state.json` under the app's config directory. It cannot be
  made narrower than 720px, so the toolbar's search box stays in view.
- No blocking model download at startup: qmd's models are fetched on
  first need by the steps and the search applet (`datalib_qmd_models`),
  the same as the web packaging — the shell passes nothing besides
  `--no-open` and the `--url-stdout` handshake.
- There is no `datalib:` link handler; the shell does not link
  `tauri-plugin-deep-link`. So the `datalib:doc/…` name a 🆔 button
  becomes in a copied selection opens nothing outside the app yet.
