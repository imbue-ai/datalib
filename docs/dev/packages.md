# Packages

A **package** is a set of programs from outside datalib that a library
runs as steps and applets: a source type datalib doesn't ship, with the
cards that show it. A package is published as one manifest file at a URL, usually an
asset of a GitHub release. There is no server of ours in between: anyone
who knows the link can add the package, and the gallery of packages datalib
suggests is one more file listing links (§"The gallery").

Which packages a library uses is part of the library, not of the app. The
library's `config.toml` lists them, pinned by hash, and their programs are
unpacked under the library's own `system/packages/`. A copied library
brings its packages along, and a library opened on another machine
fetches the build for that machine.

The code: `datalib/backend/packages/` (the manifest, the lookup, the
fetch), `http/src/packages.rs` (the routes), `[[packages]]` and the
`package` key in `datalib/backend/dag/src/config.rs`, and in the UI
`config/packages.ts`, `components/PackageReview.vue` and
`components/PackagesDialog.vue`. The example package is
`datalib/backend/packages/example/hello/`.

## The manifest: `datalib-package.toml`

```toml
schema = 1
name = "Weather"
version = "1.2.0"
description = "Hourly observations from a NOAA station."
homepage = "https://github.com/alice/datalib-weather"
step_protocol = 1                 # the step protocol it was written against

[artifacts]                       # one .tar.gz per platform, or `any`
aarch64-apple-darwin     = { url = "weather-1.2.0-aarch64-apple-darwin.tar.gz", sha256 = "…", size = 3141592 }
x86_64-unknown-linux-gnu = { url = "weather-1.2.0-x86_64-unknown-linux-gnu.tar.gz", sha256 = "…" }

[[sources]]                       # a source type it adds to "Add source"
type = "weather"                  # the group `type` such a source gets
commands = { ingest = "bin/fetch-weather", render_markdown = "bin/render-weather" }
[sources.wizard]                  # its add-source form
label = "Weather station"
blurb = "Hourly observations from a NOAA station."
sections = [{ heading = "Which station?", fields = ["station"] }]
fields = [{ kind = "text", target = "station", label = "Station", required = true }]

[[sources.applets]]               # an applet each source of this type gets
id = "chart"
command = "bin/weather-chart"
```

- **`artifacts`**: a key is a target triple datalib is released for, or
  `any` for a build that runs everywhere (a script). A URL is relative to
  the manifest's own, or absolute. The platform datalib runs on takes
  its own build first, then `any`.
- **`sources`**: `commands` maps a function to the command its step
  runs. `ingest` is required. `render_markdown` and `render_time_series`
  are optional; each is a step that reads the ingest's tree and writes
  the store the indexes read, exactly as a built-in source's does
  (§"Rendering"). The first word of a command is a path inside the
  unpacked archive. What each program must do is the step protocol
  (`step_protocol.md`).
- **`sources.wizard`**: the add-source form, written as a `CatalogEntry`
  (`datalib/ui/src/config/catalog.ts`) without `type`: the same
  `label`, `blurb`, `sections`, `fields`, `before` and the rest that a
  built-in source declares. The "Add source" dialog draws it with the
  same code, so `wizard_design.md` applies to it unchanged. A form can
  only ask for what a field kind can ask for; a package cannot bring its
  own form code.
- **`sources.applets`**: the applets a source of this type brings. An
  applet belongs to a source: it is written into the config when a
  source of the type is added, filed under the source's group, and
  removed with it. A package has no applets of its own. `id` is the
  applet's name within the source (letters, digits, `_`); the config's
  id is `<group>_<id>`. The applet gets `source = "<group>"` in its
  params, so it knows which source's trees to read. `applets.md` is the
  contract; the command's first word is a path inside the package, as
  for a step.

The platforms datalib is released for (`.github/workflows/release.yml`),
and so the keys `artifacts` may use:

| key | platform |
|---|---|
| `aarch64-apple-darwin` | macOS on Apple silicon |
| `x86_64-unknown-linux-gnu` | Linux, x86-64, glibc |
| `aarch64-unknown-linux-gnu` | Linux, ARM64, glibc |
| `x86_64-unknown-linux-musl` | Linux, x86-64, musl (what Minds installs) |
| `aarch64-unknown-linux-musl` | Linux, ARM64, musl |
| `any` | every platform |

`datalib_packages::platform::TARGETS` is the same list; a manifest naming
anything else is refused.

## Publishing one

```sh
scripts/pack_datalib_package.py <package dir> <out dir>
```

The script needs Python 3.11 or newer. The package dir holds
`datalib-package.toml` without `[artifacts]`, and
`build/<key>/` for each build. The script packs each build, then writes
the manifest with `[artifacts]` naming each archive by its sha256. Upload
everything in the out dir as the assets of one GitHub release. People add
the package with:

| they type | the manifest fetched |
|---|---|
| `github:alice/datalib-weather` | the latest release's `datalib-package.toml` |
| `github:alice/datalib-weather@v1.2.0` | that release's |
| `https://…/datalib-package.toml` | that file, from any host |
| `file:///…/datalib-package.toml` | a package being developed, on this machine |

## What a library records

```toml
[[packages]]
id = "weather"
source = "github:alice/datalib-weather"
url = "https://github.com/alice/datalib-weather/releases/download/v1.2.0/datalib-package.toml"
sha256 = "…"   # the manifest's

[[steps]]
group = "sfo"
function = "ingest"
package = "weather"
command = "bin/fetch-weather"
```

- `source` is what the person typed, and what "Check for update" looks
  up again. `url` is where the manifest was found after the redirects
  that stay on the host asked, so it names a release, not `latest`, and
  not the expiring download link GitHub then redirects to. `sha256` is the manifest's hash. The
  manifest names each archive's hash, so the manifest's hash pins every
  byte the package unpacks.
- A step or applet with `package` runs `<data root>/system/packages/<the
  first 16 hex digits of sha256>/<first word of command>`. The loader
  refuses a `package` that names no `[[packages]]` entry, and a program
  path that is absolute or contains `..`
  (`datalib_dag::config::package_program`).
- The pin is part of the step's argv, so it is part of the step's
  fingerprint: updating a package makes its steps run again on the next
  sync.

## Rendering

A package source's `render_markdown` and `render_time_series` programs
write what datalib's own render steps write, in the same place:

- **`<group>/render_markdown/indexed_markdown.doltlite_db`** and the
  `.md` files beside it. `grid_index` reads the store into the grid,
  and the source's `keyword_index` and `embed` read the files for
  search. The step reports the store's tree hash as its version.
- **`<group>/render_time_series/time_series.doltlite_db`**, which
  `time_series_index` lists and the Time series card charts. The step
  reports the store's commit hash as its version.

The wizard adds these steps to the inputs of `grid_index`, the search
aggregator and `time_series_index` (`setTimeSeriesStep` in
`config/sourceSteps.ts`), as it does for a built-in source. Nothing in
the indexes asks a store which type or provider wrote it.

These stores are datalib's own formats, not a published interface. A
writer has to match them exactly:

1. **The tables, byte for byte.** `grid_index` reads a render store only
   when its `_datalib_meta.schema_hash` (blake3 over the `CREATE`
   statements) equals the one this build computes
   (`datalib/backend/etl/render/src/indexed_markdown.rs`, `schema_hash`). A release
   that changes a render table therefore hides every package's
   documents until the package ships a build with the new tables.
2. **The doltlite writer rules.** Write on the `datalib_writer` branch
   and move `main` to it when a run ends; readers only look at `main`
   (`datalib/backend/etl/src/doltlite_raw.rs`, `commit_run`).
3. **`_datalib_meta.datalib_version`.** A store naming a newer datalib
   than the one running makes the app refuse the whole data root
   (`datalib/backend/store_meta/src/guard.rs`). A package should write a value that is
   not a datalib version.
4. **The time-series chunk encoding** (`time_series.md` §"The chunk
   encoding"). A time-series store the index cannot read fails
   `time_series_index` for every source, not only that one.

A built-in render step also gets something a package's does not: the
launch's migrate and compact passes skip a step with a `command`
(`datalib/backend/dag/src/config.rs`, `spec_of`), so a package's stores
are never migrated in place.

The Hello package (`github:qi-imbue/datalib-hello`) writes both stores
from a separate repo, with a copy of these formats kept in its own
`datalib-sdk` crate.

## Adding, updating and removing one

The UI writes the config; the server only fetches.

1. **Look it up.** `POST /api/packages/lookup {source}` fetches the
   manifest and answers what it says, with the `url` and `sha256` that
   adding it would pin. It writes nothing.
2. **Review.** The person sees the name, version, where it comes from,
   what it adds, which programs it runs, the download for this
   platform, and that a package runs as them (§"Trust").
3. **Fetch.** `POST /api/packages/fetch {url, sha256}` fetches the
   manifest again and refuses it unless it still hashes to the pin. It
   then downloads this platform's archive, checks its sha256, and
   unpacks it into `system/packages/<pin>/`. A `.datalib-installed`
   stamp there names the platform; a stamp for another platform
   (a library copied from a Mac to Linux) means not installed.
4. **Write.** Only then does the UI add the `[[packages]]` entry to the
   config. Adding a source of one of its types then writes the source's
   steps and applets, which run at once.

`GET /api/packages` lists the config's packages and their state:
`installed`, `fetching` or `failed`. Listing starts a fetch for any
package not installed here. That covers a library copied from another
machine, or a config edited by hand. A failed fetch is shown above the
Sources table with the sources that wait on it; nothing else stops.

An update is the same review, then the same fetch, then a new `url` and
`sha256` on the entry. Each source's applets follow the new version: one
it no longer has is removed, a new one is added, and one it still has
(matched by `command`) is left as the config has it. A source's steps
are not changed by an update: a function the new version adds reaches a
source when it is added again. Removing a package removes its entry.
Removal is refused while a source's steps still run the package; remove
the source first, which takes its applets with it. The unpacked directory stays until the
library is cleaned up by hand.

## Where it is in the UI

- **"Add source"** lists the sources of every installed package first,
  under "From packages in this library". Below the list, "Not listed?
  Add a package from a link" looks a package up, reviews it, adds it, and
  opens its form when it adds exactly one source.
- **Packages** on the Sources card lists the library's packages, offers
  "Check for update" and "Remove", and has the same box for adding one.
- Both show the gallery (§"The gallery") above the link box.

## Trust

A package's programs run as the person, with everything their account
can read: there is no sandbox. The review says so, in the warning colour,
before anything is fetched. The pin is what makes that confirmation
mean something: the bytes that run are the bytes the person agreed to,
until they review and accept an update.

## The gallery

The gallery is the list of packages datalib suggests:
`datalib/ui/src/config/packageGallery.ts`, one entry per package with its
`source`, a name and a one-line description. It ships in the app, so a
change to it reaches people with the next release. Both places that add
a package show it above the link box. Choosing one looks its `source` up
and shows the same review as a pasted link: the gallery vouches for
nothing, and the person still decides. A package already in the library
is marked as such.

To list a package, publish it as above and add an entry.
