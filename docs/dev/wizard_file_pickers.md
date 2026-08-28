# Wizard path fields must offer a native picker

**The rule.** Any wizard field that asks the user for a file or a
folder must offer a native OS picker dialog — not a bare text box the
user is expected to type a path into. The typed box stays, as a
fallback and a paste target; it is not the primary way in.

**Status (2026-08-28): built, for every path field that exists.** In
the desktop app each `kind: "path"` field renders a **Choose folder… /
Choose file…** button beside its input, wired to a real OS dialog. The
three fields today — WhatsApp's backup folder, Signal's snapshot
folder, Lightroom's `.lrcat` — all have one. In a plain browser the
button is absent and the typed input is all there is; see
[below](#the-browser-served-case-is-still-typed-only) for why, and what
would fix it.

The count of path fields only goes up: over half the twenty source
types read from local disk, so most descriptors still to be written
will carry one. This doc is the rule for those, and the description of
the machinery to reuse.

## Why a text box is the wrong control here

The user is not inventing these paths, they are *locating* something
that already exists — a `WhatsApp/` directory pulled off a phone, a
Lightroom catalog, a folder of Signal snapshots. They almost always
have it in front of them in Finder or Explorer while they type it into
us by hand. Everything that can go wrong in that transcription does:
`~` that we may or may not expand, a space that got quoted, a smart
quote from a pasted note, a volume spelled differently under `/Volumes`
than in the sidebar, a trailing `/Databases` because the help text
mentioned it.

None of it is caught at the point of the mistake. The wizard writes the
string into `config.toml`, and the user finds out on the next sync,
from a step failure — exactly the class of "check after the run that
could have happened before it" the wizard exists to eliminate.

A picker also relieves the help text of describing the folder's
contents so carefully: pointing at the right directory is a recognition
task in the file manager, not a spelling task in a form.

## How it works

Three pieces, one per layer:

1. **A Tauri capability** —
   [`capabilities/pick-local-paths.json`](../../datalib/tauri/capabilities/pick-local-paths.json)
   grants `dialog:allow-open` to the `main` window. The Rust side
   needed nothing new: `tauri-plugin-dialog` was already a dependency
   and already registered in
   [`main.rs`](../../datalib/tauri/src/main.rs), for the launcher's own
   folder picker (`launcher_pick`).
2. **`pickPath()`** in
   [`ui/src/desktop.ts`](../../datalib/ui/src/desktop.ts) — calls the
   dialog through `@tauri-apps/plugin-dialog` and returns one of three
   outcomes: `picked`, `canceled`, `unavailable`.
3. **The control** in
   [`SourceWizard.vue`](../../datalib/ui/src/components/SourceWizard.vue)
   — the button, shown only when `isDesktopApp()`, beside an input that
   stays editable either way.

Two details in there are load-bearing and easy to get wrong:

**The capability needs a `remote` block, with the trailing `/**`.**
This app does not bundle its frontend: it serves the UI from
`datalib-http` and loads it as an external URL, and Tauri withholds IPC
from a remote origin unless a capability lists it. `http://127.0.0.1:*`
without the trailing `/**` constrains the pathname to empty and matches
no route. **An unmatched pattern denies the call silently** — a button
that does nothing — so verify in the app, not in a browser tab. The
same note is on
[`reveal-local-files.json`](../../datalib/tauri/capabilities/reveal-local-files.json)
and [`open-external-urls.json`](../../datalib/tauri/capabilities/open-external-urls.json),
which is three capabilities that have each had to learn it.

**Cancel and denial are different outcomes.** A caller that only gets
`string | null` cannot tell "the user changed their mind" from "the
command was never authorized", and those need opposite responses:
cancel leaves the field alone and says nothing, denial has to be
visible or it looks like a dead button. Hence the three-armed
`PathPick`.

## The browser-served case is still typed-only

`<input type="file">` cannot stand in for the dialog. The browser hands
back a sandboxed `File` and never a filesystem path, and
`webkitdirectory` yields relative names only. Nor is the path even
necessarily local: it is a path on the machine running the *backend*,
which in the browser-served case need not be the user's machine at all.

The fix is a server-side browse endpoint (`GET /api/fs/browse`,
sketched in [`source_wizard.md`](source_wizard.md)) — the backend
enumerating its own filesystem, which is the only party that can. It
does not exist today. Until it does, a browser user types the path, and
`pickPath` returns `unavailable` rather than pretending.

## Checklist for a new path field

When you add a `kind: "path"` entry to
[`catalog.ts`](../../datalib/ui/src/config/catalog.ts), the button
comes for free. What you owe it:

- **`picks: "file" | "dir"`**, correct. It decides which dialog opens,
  and a folder-vs-file mismatch is the one error a picker can still let
  through.
- **`pickTitle`** that names the thing ("Choose your WhatsApp backup
  folder"), not the widget ("Select folder"). Falls back to the field
  label, which is usually too terse for a window title.
- **`extensions`** for a file picker, when the type is canonical
  (`["lrcat"]`). Keep them broad enough not to hide a legitimate file —
  the typed input is the escape hatch, but only if the user thinks to
  use it.
- **Keep the `placeholder`** example path. Paste is a legitimate way in
  — over ssh, from a note, from a colleague — and the browser-served
  case has nothing else.

Two behaviors the shared code already handles, worth not breaking:
cancel is a no-op on the field, and the dialog opens at the field's
current value when that value is an absolute path (`~/…` is dropped —
Tauri passes `defaultPath` to the platform dialog verbatim, no shell is
involved, so a literal `~` is a *relative* path resolved against the
process's cwd).

What is still missing is validation on selection: the descriptor knows
what the folder should contain (`Databases/msgstore.db.crypt15` for
WhatsApp) and nothing checks it. That is where the design's `inspect`
probe goes — and where the macOS permission check below wants to live
too, since both are "look at the path now, in this process, rather than
during a sync days later".

## Open: what the picker buys us in macOS permissions

**Unresolved, worth following up.** On macOS, choosing a path in the
standard open panel normally grants the app access to it — which would
make the picker a permissions fix as well as a typo fix. Whether we
actually get that benefit here is not established, and nothing in this
repo has ever mentioned macOS file permissions.

What *is* established, by reading the tree:

- **The app is not sandboxed.** No `.entitlements` file exists;
  `build-signed-app.sh` signs with Developer ID and `--options runtime`
  (hardened runtime) and nothing more; `tauri.conf.json` sets no macOS
  entitlements. So the mechanism usually meant by this question —
  Powerbox handing a *sandboxed* app a grant for the user-selected
  file, persisted with a security-scoped bookmark — is not in play at
  all. There is no `com.apple.security.files.user-selected.read-only`
  for the panel to satisfy. **Do not reach for security-scoped
  bookmarks here**; they are the answer to a question this app does not
  ask.
- **TCC still applies.** Even unsandboxed, macOS gates `~/Desktop`,
  `~/Documents`, `~/Downloads`, iCloud Drive, and removable/network
  volumes. Phone backups land in exactly those places, so this is a
  live case: typing `~/Documents/WhatsApp` into the field can earn an
  "Operation not permitted" that choosing the same folder would not.
- **The picking process is not the reading process.** The panel opens
  in the shell; the file is opened four processes down and much later:
  `Datalib.app` → `datalib-http` (`tauri/src/main.rs`, `start_backend`)
  → `datalib-dag` (`http/src/worker.rs`) → `datalib-step`
  (`dag/src/subprocess.rs`), when a sync is queued rather than when the
  folder is chosen. TCC attributes a child to its responsible process,
  normally the app, so an in-app sync plausibly inherits the grant —
  but the same `config.toml` is explicitly meant to run outside the app
  too (`datalib-dag <config>` from a terminal, `datalib-http`
  standalone), where the responsible process is the terminal and the
  app's consent is irrelevant.

Weak evidence that the inheritance does work: `~/Documents/Datalib` is
the default new data root, and `datalib-http` reads and writes it as a
spawned child today.

**What has not been tested is the part that matters** — whether a grant
from the panel survives four levels of spawn and a deferred sync. The
tree cannot answer it; it needs a run against a folder in a protected
location, watching whether the step fails and whether the prompt names
Datalib.

The likely fix if it doesn't hold is not exotic: **`readdir` the chosen
path in the shell process, right after picking**, and say so
immediately when it fails. That turns a deferred, cryptic sync failure
into a sentence at the moment of choosing — and it is the same hook the
design already wants for the `inspect` probe ("4,182 PDFs, 3.1 GB, 96
need OCR"), so the two should land together. A Full Disk Access
instruction is the fallback for anyone whose backups sit somewhere the
panel cannot cover.

## Corrections this doc made

Two claims in the tree argued the other way, and were removed when this
landed:

- `catalog.ts` said the UI "has no Tauri IPC". It does —
  `desktop.ts` and `externalLinks.ts` both invoke through it, each with
  its own capability file. What was missing was a capability and an npm
  package, not a bridge.
- [`source_wizard.md`](source_wizard.md)'s section on local-path
  sources concluded "**a backend-served browse endpoint**, not a native
  dialog", with the dialog as a later enhancement layered on top. That
  ordering was inverted: in the app — which is how this ships, and how
  nearly every user meets it — the native dialog is the primary
  control. The browse endpoint remains the right answer for the
  browser-served case, and still does not exist.
