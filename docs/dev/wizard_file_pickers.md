# Wizard path fields must offer a native picker

**The rule.** Any wizard field that asks the user for a file or a
folder must offer a native OS picker dialog — not a bare text box the
user is expected to type a path into. The typed box stays, as a
fallback and a paste target; it is not the primary way in.

In the desktop app every `kind: "path"` field in
[`catalog.ts`](../../datalib/ui/src/config/catalog.ts) renders a
**Choose folder… / Choose file…** button wired to a real OS dialog,
with the path it picked beside it and the typed input behind "Type the
path instead". In a plain browser the button is absent and the typed
input is all there is; see
[below](#the-browser-served-case-is-still-typed-only) for why, and what
would fix it. Most source types read from local disk, so most new
wizard entries will carry a path field. This doc is the rule for those,
and the machinery to reuse.

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
   grants `dialog:allow-open` (and nothing else from the dialog plugin)
   to the `main` and `card-*` windows, plus
   `core:path:allow-resolve-directory` so the page can expand a `~`. `tauri-plugin-dialog` is
   registered in [`main.rs`](../../datalib/tauri/src/main.rs), which
   also uses it for the launcher's own folder picker (`launcher_pick`).
2. **`pickPath()`** in
   [`ui/src/desktop.ts`](../../datalib/ui/src/desktop.ts) — calls the
   dialog through `@tauri-apps/plugin-dialog` and returns one of three
   outcomes: `picked`, `canceled`, `unavailable`.
3. **The control** in
   [`SourceWizard.vue`](../../datalib/ui/src/components/SourceWizard.vue)
   — the button, shown only when `isDesktopApp()`, and an input that
   stays editable either way.

Two details in there are load-bearing and easy to get wrong:

**The capability needs a `remote` block, with the trailing `/**`.**
This app does not bundle its frontend: it serves the UI from
`datalib-http` and loads it as an external URL, and Tauri withholds IPC
from a remote origin unless a capability lists it. `http://127.0.0.1:*`
without the trailing `/**` constrains the pathname to empty and matches
no route. **An unmatched pattern denies the call silently** — a button
that does nothing — so verify in the app, not in a browser tab. Every
capability in
[`datalib/tauri/capabilities/`](../../datalib/tauri/capabilities/)
that the UI reaches needs the same block.

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
sketched in [`source_wizard.md`](plans/source_wizard.md)) — the backend
enumerating its own filesystem, which is the only party that can. It
is not built. Until it is, a browser user types the path, and
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
- **An example path in the `help`**. Paste is a legitimate way in —
  over ssh, from a note, from a colleague — and the browser-served case
  has nothing else. Not a `placeholder`: text inside the box reads as a
  value someone already typed, so no path field has one.
- **`startIn`**, where the location is fixed by the app that owns it
  (`~/Library/Messages`, `~/Pictures/Lightroom`), not a download the
  user put somewhere. The picker opens there while the field is empty,
  and the help shows the path with a copy button, so the help must
  name it exactly (`catalogStartIn.test.ts` checks).
- **`guarded`**, where macOS keeps the path private and choosing it in
  the picker is what grants access (`~/Library/Messages`, a Photos
  library). The field is then drawn as a card under that title, with
  the Full Disk Access advice beneath it.

Three behaviors the shared code already handles, worth not breaking:
cancel is a no-op on the field; the dialog opens at the field's current
value, else at its `startIn`, with a leading `~` expanded against the
home directory Tauri reports (Tauri passes `defaultPath` to the
platform dialog verbatim, no shell is involved, so a literal `~` would
be a *relative* path resolved against the process's cwd); and help text
selects and copies in WebKit, where the `<label>` around each field
would otherwise take the click that ends a drag and focus its input
(`wizard-help-select.spec.ts`).

What is still missing is validation on selection: the descriptor knows
what the folder should contain (`Databases/msgstore.db.crypt15` for
WhatsApp) and nothing checks it. That is where the design's `inspect`
probe goes — and where the macOS permission check below wants to live
too, since both are "look at the path now, in this process, rather than
during a sync days later".

## What the picker buys us in macOS permissions

On macOS, choosing a path in the standard open panel grants the app
access to it, and (measured) that grant reaches the processes that do
the reading. So the picker is a permissions fix as
well as a typo fix, and for `apple_messages` and `apple_photos` it is
the *only* way in short of Full Disk Access.

What is established by reading the tree:

- **The app is not sandboxed.** No `.entitlements` file exists for it;
  `build-signed-app.sh` has Tauri sign with the Developer ID identity,
  `tauri.conf.json`'s `beforeBuildCommand` signs the bundled binaries
  with `--options runtime` (hardened runtime), and neither sets macOS
  entitlements. So the mechanism usually meant by this question —
  Powerbox handing a *sandboxed* app a grant for the user-selected
  file, persisted with a security-scoped bookmark — is not in play at
  all. There is no `com.apple.security.files.user-selected.read-only`
  for the panel to satisfy. **Do not reach for security-scoped
  bookmarks here**; they are the answer to a question this app does not
  ask.
- **TCC still applies.** Even unsandboxed, macOS gates `~/Desktop`,
  `~/Documents`, `~/Downloads`, iCloud Drive, removable/network
  volumes, and a short list of application data stores — the ones that
  matter here are `~/Pictures/Photos Library.photoslibrary`, which the
  `apple_photos` source reads, and `~/Library/Messages`, which
  `apple_messages` reads. Phone backups land in the first group,
  so typing `~/Documents/WhatsApp` into the field can earn an
  "Operation not permitted" that choosing the same folder would not.
- **The picking process is not the reading process.** The panel opens
  in the shell; the file is opened further down and much later:
  `Datalib.app` → `datalib-http` (`tauri/src/main.rs`, `start_backend`),
  whose loop spawns `datalib-step` (`dag/src/subprocess.rs`) when a sync
  is queued rather than when the folder is chosen. TCC attributes a
  child to its responsible process, normally the app.

What was measured, against the Photos library — the most locked-down
path any source here reads. The experiment used `osascript` under an
app with no Full Disk Access (a shell spawned by it got `Operation not
permitted` on a plain `ls` of the bundle beforehand), so the app under
test was in Datalib.app's position:

| step | result |
|---|---|
| a *folder* chooser | cannot select the bundle: the panel shows a `.photoslibrary` package as a file, so the nearest pick is `~/Pictures/` |
| a *file* chooser, picking the bundle, then `ls database/` from a **child** of the panel process | reads it |
| `ls database/` from an unrelated process of the same app, one that never showed a panel | reads it — the grant is recorded against the app, not the process that showed the panel |

Two consequences for a descriptor whose path is a macOS package: it
must say `picks: "file"`, and the grant it earns does carry down the
spawn chain above.

**A picked file grants that file; a picked folder grants what is in
it.** Measured in the app with `apple_messages`, without Full Disk
Access:

| picked | what the step could then do |
|---|---|
| `~/Library/Messages/chat.db` | copy `chat.db`; copying `chat.db-wal` beside it: `Operation not permitted` |
| the folder `~/Library/Messages` | mirror `LiteSegmentStore.db` in it, a file never picked on its own (`VACUUM INTO` opened it in place, `-wal` and `-shm` included) |

A SQLite database in WAL mode is three files, so a source reading one
picks the folder that holds them. A refusal inside a protected folder
can look like absence (`stat` answers `No such file or directory`), so
check a missing file in Finder before blaming the grant.

What is **not** measured is whether the grant
survives quitting and relaunching the app; the TCC database that would
say so is itself protected. Until it is, the wizard's help text for
such a source names Full Disk Access as the durable fallback, and the
right place to notice a lapse is the `inspect` probe: **`readdir` the
chosen path in the shell process, right after picking**, and say so
immediately when it fails, rather than during a sync days later.

None of this reaches the CLI. `datalib-dag <config>` from a terminal is
attributed to the terminal, which has its own grants or prompts for
them; the app's consent is irrelevant there.
