# Packages

A **package** is a set of programs from outside datalib that a library
runs as steps and applets: a source type datalib doesn't ship, or a
card. A package is published as one manifest file at a URL, usually an
asset of a GitHub release. There is no server of ours in between: anyone
who knows the link can add the package, and a curated store would be one
more file listing links (§"A curated store").

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

[[applets]]                       # an applet it adds, written into the config with it
id = "weather_chart"
command = "bin/weather-chart"
```

- **`artifacts`**: a key is a target triple datalib is released for, or
  `any` for a build that runs everywhere (a script). A URL is relative to
  the manifest's own, or absolute. The platform datalib runs on takes
  its own build first, then `any`.
- **`sources`**: `commands` maps a function to the command its step
  runs. `ingest` is required. A source with `render_markdown` also gets
  a render step that reads the ingest's tree; one without it renders
  nothing. The first word of a command is a path inside the unpacked
  archive. What each program must do is the step protocol
  (`step_protocol.md`).
- **`sources.wizard`**: the add-source form, written as a `CatalogEntry`
  (`datalib/ui/src/config/catalog.ts`) without `type`: the same
  `label`, `blurb`, `sections`, `fields`, `before` and the rest that a
  built-in source declares. The "Add source" dialog draws it with the
  same code, so `wizard_design.md` applies to it unchanged. A form can
  only ask for what a field kind can ask for; a package cannot bring its
  own form code.
- **`applets`**: `applets.md` is the contract. The command's first word is
  a path inside the package, as for a step.

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

The package dir holds `datalib-package.toml` without `[artifacts]`, and
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
4. **Write.** Only then does the UI add the `[[packages]]` entry and the
   package's applets to the config. Steps that name the package can run
   at once.

`GET /api/packages` lists the config's packages and their state:
`installed`, `fetching` or `failed`. Listing starts a fetch for any
package not installed here. That covers a library copied from another
machine, or a config edited by hand. A failed fetch is shown above the
Sources table with the sources that wait on it; nothing else stops.

An update is the same review, then the same fetch, then a new `url` and
`sha256` on the entry. Removing a package removes its entry and its
applets. Removal is refused while a source's steps still run the
package; remove the source first. The unpacked directory stays until the
library is cleaned up by hand.

## Where it is in the UI

- **"Add source"** lists the sources of every installed package first,
  under "From packages in this library". Below the list, "Not listed?
  Add a package from a link" looks a package up, reviews it, adds it, and
  opens its form when it adds exactly one source.
- **Packages** on the Sources card lists the library's packages, offers
  "Check for update" and "Remove", and has the same box for adding one.
  This is the way in for a package that adds only applets.

## Trust

A package's programs run as the person, with everything their account
can read: there is no sandbox. The review says so, in the warning colour,
before anything is fetched. The pin is what makes that confirmation
mean something: the bytes that run are the bytes the person agreed to,
until they review and accept an update.

## A curated store

A curated store needs no server either. It would be a file listing
`source` strings with a short description each, kept in a repo. The app
would read a configured list of such files and show their entries above
the link box. Nothing in the format above has to change for it.
