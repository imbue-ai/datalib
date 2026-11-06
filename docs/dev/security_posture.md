# Security posture

Who we are guarding against, what they would want, how they could get
it, what stops them and what does not. A guard makes sense only next to
the attack it answers, and the code shows the guard but not the attack.

Each threat says what is still open. An open item with a reason is
useful here; a threat written as if it were solved is not. Say whether a
claim was measured, read from source, or only reasoned about. A dated
review of the tree goes in [`audits/`](audits/), not here.

## What is worth taking

datalib holds a person's email, messages, documents and health data, and
the sign-ins that fetch more of it. So there are two prizes:

- **One person's library**, reached through something that person
  received or a page they visited.
- **Everyone's library**, reached through what we ship. A release runs on
  each person's machine with access to all of the above, and a signed Mac
  app is trusted by macOS as coming from us. This is why the build gets
  more attention below than its size suggests.

## Who we guard against

| who | what they can already do |
|---|---|
| A stranger | Send email or a message with any content and any attachment, which a sync then mirrors. |
| A website | Run script in a browser that is also open on the app's local port. |
| Whoever controls a third-party package | Publish a new version. Compromised maintainer accounts are the usual route. |
| Someone holding a write-access account | Push branches and run workflows here, and merge a PR whose CI is green. Reviews are deliberately not required, and about 30 accounts have write access. |
| Another program running as you | Read any file you can, see your other programs' environment and arguments, and connect to any local port. |

Not written up yet, and not yet decided whether we defend against them:
a stolen laptop, and code a person chose to install as a package or applet.

## Bad code reaches a release through the build

This is the threat behind the job split in `release.yml`
([`release_steps.md`](release_steps.md) §"Who holds what") and behind a
release reading no cache ([`ci.md`](ci.md) §"Two caches").

Two facts make it possible:

- **Compiling runs other people's code.** A crate's `build.rs` is a
  program that runs while the crate compiles, as the same user as the
  build, and so do proc macros and any tool a Bazel rule runs. Nothing
  there is a security boundary. On `test.yml`'s and the release's Linux
  jobs, actions run under `processwrapper-sandbox`, which in Bazel's
  source is a symlink tree with no mounts and no network isolation. On a
  Mac, a Bazel action under `darwin-sandbox` read a stand-in
  `.bazelrc.user` and got an answer from BuildBuddy's host (measured);
  `cargo`, which builds the desktop app, has no sandbox at all.
- **Bazel trusts a cache.** The action cache maps a hash of an action's
  command and inputs to its recorded outputs, and on a hit Bazel uses
  those outputs without running anything and without checking them. The
  repository cache behaves the same way for source trees: with a file
  under its `contents/` directory edited and the download's server shut
  down, a Bazel 9.3.1 build succeeded with the edited bytes in its output
  (measured, on a one-file repository). So whoever can write a cache
  chooses what a later build that reads it gets.

### How the code gets there

```
crates.io                a new version of a crate someone took over
    │  7-day wait        only for the backend's weekly lockfile PR
    ▼
PR run                   build script runs, holding the contributor key
    │  CI green          no review required
    ▼
push to main             build script runs, holding main's BuildBuddy write key
    │
    ▼
tag → release build      compiles the crate itself, reads no cache:
                         signed Mac app, tarballs, Docker image
```

The second way in skips the first two boxes. Someone holding a
write-access account needs no crate: a test in their own PR runs with the
write key once it reaches `main`.

### What a build script can do with that position

From smallest reach to largest:

1. **Change what this build produces.** It needs neither a cache nor a
   key. The crate's own output is the obvious target, and because nothing
   isolates one action from another, anything else the same job builds is
   in reach too (reasoned from the sandbox's design, not tried). It ends
   when the crate is gone and the job is rebuilt. A release that compiles
   from scratch is exposed to this, and `macos-app-sign` would sign the
   result.
2. **Write a cache entry for some other action.** A dependency only tests
   use can replace the output of a crate `main` builds, and the entry
   outlives the crate: revert the bump and it stays, because the victim
   action's hash has not changed. This reaches `main`'s CI, not a
   release: it can make a failing test look green on `main`, or run code
   in those jobs by replacing a tool the build runs. Those jobs hold the
   write key and a read-only token. No workflow takes what they produce,
   though one artifact is a built tarball a person could download by hand.
3. **Take the key.** Send it elsewhere, then write entries from anywhere
   at any time until someone mints a new one. The repository shows no
   trace, and removing the bad crate changes nothing. The reach is the
   same as item 2. The gate on a PR uses the contributor org, so this key
   cannot fake a required check.

The first comes with using a dependency at all, and a release reading no
cache does nothing about it. What it removes is the reach of the other
two: a crate no release builds, or one already reverted, has no way left
into a release's output.

**The `v*` tag ruleset limits who starts a release, not what runs in
one.** Only the `datalib-release` team, two people, can create a tag.
That does nothing about the build scripts that run once one starts.

### What guards it

| guard | what it stops | what it does not stop |
|---|---|---|
| Lockfiles; `cargo deny` allows only the crates.io registry and fails on a known advisory; required to merge | An unpinned version, an unknown source, a bad version once it has an advisory | A bad version nobody has reported yet |
| 7-day burn-in in `cargo-update.yml` | A bad backend crate noticed within the week | One nobody notices. Dependabot's PRs have no `cooldown`, so the desktop app's crates and the npm trees do not wait |
| A release reads no cache: no BuildBuddy key, no restored repository cache, no Docker layer cache (check 21) | An entry written on `main`, or with a stolen key, reaching what a release ships. On the mac legs this was the repository cache: v0.49.0's restored an entry saved on `main` by `latchkey-auth.yml`'s daily run, while BuildBuddy gave them no hits | Bad code a release compiles itself (item 1) |
| Two BuildBuddy orgs | A laptop or a PR run writing what `main`'s CI replays | Anything that runs on `main` |
| The write key is a secret of the `main-ci` environment, which only `main` may use (check 21, and a setting below) | A PR's code, or a workflow edited on a branch, getting the key before it merges | Anything that runs on `main` |
| BuildKit pinned by digest, and each image built on its own architecture, so no emulator image runs (check 21) | Whoever controls a tag on Docker Hub running code in a job that can push the image, or changing what assembles it | A bad version someone pins deliberately, and the runner image itself |
| Publish and sign jobs build nothing (`release_steps.md`; check 21) | A build taking the signing key or a write token, or rewriting a published release | Bad code in what it builds |
| What a build made reaches those jobs outside the checkout, and the sign job's archive passes `unpack_compiled_app.py` (check 21) | A build replacing a script or the Tauri CLI that the sign job then runs with the key in reach, by an archive entry or a planted artifact | A bad binary inside the allowed paths, which gets signed. Nothing in the sign job runs it (`sign-staged.sh`), and `macos-app-verify` runs it with no key |
| The `v*` tag ruleset | Anyone outside the team starting or moving a release | What a release does once started |

### What is still open

- **Bad code a release compiles itself** (item 1 above). The burn-in and
  `cargo deny` are all that stand in front of it.
- **The write key sits where crates compile.** `test.yml`'s three jobs
  that run on a push to `main` hold it, and it cannot be hidden from
  build scripts on those runners. Its reach is `main`'s CI.
- **Nothing can tell a bad entry from a good one.** If a compromise is
  found, mint a new write key; that leaves any planted entries in place
  for `main`'s CI, and no release reads them.

### What has not been shown

Nobody has run these attacks. These are the links that rest on reading
rather than on a run:

- Reading the real key from an action in `test.yml`'s job, suspected in
  the 2026-11-03 audit. The evidence is Bazel's source, the job's log
  naming `processwrapper-sandbox`, and the Mac measurement above.
- A build script editing the repository cache in `latchkey-auth.yml`'s
  job before `setup-bazel` saves it, and a tag then building from that.
  Measured separately: a tag's mac legs restore an entry saved on `main`,
  and Bazel uses an edited tree unchecked. Nobody has opened a saved entry
  to see `contents/` in it; that it is saved follows from `setup-bazel`
  saving the whole directory.
- Working out the hash of a real action and planting an entry that a
  later `main` build replays.

## A release is swapped after it is built

Someone holding a write-access account could replace a published tarball and
the `.sha256` beside it, or publish a release of their own and mark it latest.
Anyone following `releases/latest` would then install what they chose. The `v*`
tag ruleset says nothing about this: no tag moves.

| guard | what it stops | what it does not stop |
|---|---|---|
| `install.sh` pins a version and each tarball's hash, written by `pin_release.py` after a release publishes | All of the below for `curl \| sh`: it follows no `latest`, and a hash cannot be swapped from outside the repository | A merge to `main`, which is where the script is read from. A tag someone names with `DATALIB_VERSION`, checked only against the checksum beside it |
| `pin_release.py` pins only a release whose tag's commit has a successful `release.yml` run on a tag push | A writer's own release, on a new tag or an old one, being written into `install.sh` and the Docker page. A branch named like the tag does not pass: its run is on another commit. None of the 28 `v*` tags with no release has a successful run (measured) | Anyone reading the release page or naming the tag themselves |
| `docs/user/docker.md` names the image by digest, and a release carries that digest as an asset | A moved image tag reaching someone following the page | Someone who names a tag themselves |
| Immutable releases (a setting below), from v0.51.0 on | Replacing or removing an asset of a published release, or moving its tag | Any release published before the setting went on, which is all 49 through v0.50.0 (`gh api` reports `immutable: false` on each) |
| One job creates, uploads, reads back and publishes (`release_steps.md` §"Draft, then publish") | An asset swapped or added while a draft stands open, and a build's artifact carrying another job's file | A swap in the seconds between the read and the publish: the read after it reports that, but the release is locked by then |
| The tag ruleset covers every tag, not only `v*` (a setting below) | Publishing under a new tag name | A release on a tag that already exists, and marking an older release latest, which GitHub allows even on an immutable one |
| The Mac app's signature and notarization | A swapped dmg passing Gatekeeper, for someone who clicks through `releases/latest` | Bad code we signed ourselves |

### What is still open

- **`releases/latest` can still be moved by anyone with write access.** This
  no longer reaches `curl | sh` or the Docker page. It reaches a person following
  the README's link to the dmg, where the signature is the guard. Two ways remain
  to move it: mark an older release latest, or publish a new release on one of the
  28 old `v*` tags that have none. The second comes from GitHub's docs, which do
  not rule it out, and has not been tried.
- **A writer can take a new tag while its release builds.** No release exists
  during the run, about 50 minutes, so a writer could publish an immutable one of
  their own on the tag. `publish_release.py` finds it and fails the run, and
  `pin_release.py` then refuses that tag, but the version number is lost: cut the
  next one. Reasoned from the code and GitHub's docs, not tried.
- **Old releases stay replaceable.** GitHub's docs say immutability "will only
  apply to future releases". They do not say whether publishing a release again
  locks it, and nobody has tried. Doing so would lock whatever bytes are there
  now, not show that they are ours.
- **Deleting old releases would not help, and would break things.** Their tags
  remain, so a writer could publish a new release on one, and an installed build
  fetches its Node runtime from its own release on first use. Keep them.
- **A moved image tag.** A workflow on any branch can ask for `packages: write`,
  so it can probably move `ghcr.io/imbue-ai/datalib:latest` and
  `datalib_devcontainer:latest`, which `main`'s CI runs in holding the write key.
  The "refuse to move latest" step in `devcontainer.yml` is in a file that branch
  can edit. Neither has been tried, and the packages' Actions access settings
  cannot be read through the API. The real fix is a push credential in an
  environment only `v*` tags may use, as `main-ci` is for `main`, with the
  repository's own access to the package removed.
- **`install.sh` is read from `main`, so it is as trusted as a merge,** and
  merging takes a green CI and no review. A PR's run uses that PR's own workflow
  files, so a writer can edit a required check to pass. Binding the checks to
  GitHub Actions stops a status posted by hand and nothing more. An unreviewed
  merge is the easiest way into a signed release, and immutability then locks that
  release in.
- **Nothing outside GitHub vouches for a tarball.** The Mac app is signed, and
  the tarballs are not; a person trusts GitHub's account of what the tag built.
  GitHub attests each immutable release (`gh release verify v0.51.0` passes;
  v0.50.0 has no attestation). As we read it, that says what a release held when
  it locked, not who built it, so a writer's own release on an old tag would
  likely get one too (not tried). A build attestation from `release.yml` would
  say who built it, but `curl | sh` has no `gh` to check one with.

### What has not been shown

`publish_release.py` has run only against a stand-in for GitHub's API, so its
real calls are first tried by a tag's run. Whether the settings below hold is
something only those commands can say.

## The settings this rests on

Each of these lives in GitHub, not in the tree, so no test or lint can see it.
Each line is a command that shows it.

| setting | check |
|---|---|
| Immutable releases on | `gh api repos/imbue-ai/datalib/immutable-releases` says `"enabled":true` |
| `main` takes changes only through a PR | `gh api repos/imbue-ai/datalib/rulesets/22904358 --jq '.rules[].type'` includes `pull_request` |
| `main`'s required checks can come only from GitHub Actions | `gh api repos/imbue-ai/datalib/rulesets/22904358 --jq '.rules[] \| select(.type=="required_status_checks") \| .parameters.required_status_checks[].integration_id'` says `15368` for each |
| A workflow that names no permissions gets a read-only token | `gh api repos/imbue-ai/datalib/actions/permissions/workflow --jq .default_workflow_permissions` says `read`. `can_approve_pull_request_reviews` stays `true` because `cargo-update.yml` opens its PR with the workflow's token, and the one setting covers both |
| A workflow can run an action only by its full commit hash, so whoever controls an action's tag cannot change what runs. This covers actions nested in other actions; local ones are exempt | `gh api repos/imbue-ai/datalib/actions/permissions --jq .sha_pinning_required` says `true` |
| Private vulnerability reporting on, which `SECURITY.md` sends people to | `gh api repos/imbue-ai/datalib/private-vulnerability-reporting` says `"enabled":true` |
| Only `datalib-release` may create, move or delete any tag | `gh api repos/imbue-ai/datalib/rulesets/26439103 --jq .conditions` includes `~ALL` |
| `main-ci` may deploy only from `main` | `gh api repos/imbue-ai/datalib/environments/main-ci/deployment-branch-policies --jq '.branch_policies[] \| [.name, .type]'` says `main`, `branch`, and nothing else |
| The release org's key is only in `main-ci` | `gh secret list --repo imbue-ai/datalib` does not name `BUILD_BUDDY_RELEASE_API_KEY`; `gh secret list --env main-ci --repo imbue-ai/datalib` does |

## Another program running as you

An AI agent's shell, a tool that agent installed, a script you ran. It has
your user id, so it can do whatever you can unless macOS itself says no:

- **Read any file you can.** A file's mode does not separate it from you:
  0600 means "only this user", and it is this user.
- **See your other programs' environment and arguments** with `ps eww`.
  Measured on macOS 26 with a program of our own, both plain and with the
  hardened runtime under an ad-hoc signature. An Apple binary (`/bin/sleep`)
  did not show its environment, so test with your own.
- **Connect to any port on 127.0.0.1**, the app's and every applet's.

### What we defend

**An ordinary library: nothing.** It is files in your home folder, and any
such program can read them directly. A guard on the server would not change
that.

**A library in the protected folder**, on macOS 15 or later
([`tauri/README.md`](../../datalib/tauri/README.md) §"A protected library").
macOS keeps other programs out of the folder until you say yes, and that is
the only guard there is. It is meant to stop an agent running shell commands
as you from reading the library or using its sign-ins, not a program set on
stealing from you.

### The rule for a secret

**A secret that opens a library may be in a file only inside that library,
and never in an environment variable, an argument or a log.**

A file inside is acceptable because of where it is. A program that can read
`system/api-token` or `system/applet-secret` is already in the library, and
can read the data and the sign-ins without them, so the secret gives it
nothing new. The same file anywhere else is a way around the folder: a program
that macOS keeps out of the library could read it and ask the server instead.
That is what the backend's log in `$TMPDIR` was, and what `DATALIB_APPLET_SECRET`
was in `ps`.

We looked at handing the applet secret over a pipe, so that it was in no file.
That would add little. A copy of a running library would hold no working
secret, but the copy already holds the data. The API token stays in a file
either way, because scripts and Imbue Studio read it, and every published
applet would have to read a pipe instead of a file. Studio sets `DATALIB_TOKEN`
in the environment, where `ps` shows it; that is fine in its workspace, which
is not what this section defends, and the desktop app does not use it.

### What guards each way in

| way in | what guards it | what it does not stop | how we know |
|---|---|---|---|
| The library's files | macOS's guard on a group container, from macOS 15. Before that the app does not offer the folder | A program you said Allow to, and one with Full Disk Access | Full Disk Access measured on macOS 26.7.2. The version comes from Apple's documentation |
| The sign-ins | Only the folder. The keychain item lets in the app's Node, which any program can run | The same | Read from source; `tauri/README.md` has the detail |
| The server, with the API token | The token is in `system/api-token`. The launch URL reaches the desktop app over a pipe and is printed only to a terminal | A path a script gives `--url-file`, which is that script's choice | Measured on a scratch library: the token was in no other file and not in `ps`. `launch_url_test` |
| Temp files, which hold web responses, decrypted databases, unpacked archives and a sign-in browser's state | `TMPDIR` names `<library>/system/tmp` for the server and everything it runs; the server empties it at start | A program that ignores `TMPDIR`, which macOS's own tools do (below). Anything written there by earlier builds, until macOS clears it | `protected_temp_test`, on the real server with a real step. One launch of the real desktop app at the protected path, with a scratch home: the server and its applets had this `TMPDIR`, and what a crash left was gone |
| An applet's own port | Each applet requires the secret in `system/applet-secret` (`datalib_env::secret_file`) | A published applet that skips the check or hands the secret on | Measured on two real applets: `401` with what `ps` shows, `200` through the gateway. `applet_tests` |

**`TMPDIR` only moves what follows it.** Rust's `temp_dir()` and Node's
`os.tmpdir()` do, and they are what datalib's code and latchkey use. macOS's own
`mktemp` does not, with or without `-t`, even when it is run outside the app
(measured on macOS 26), so a custom step's shell script that calls it writes
outside the folder. Name the path instead: `mktemp "$TMPDIR/x.XXXXXX"`. A
visible Chromium keeps its `SingletonSocket` in macOS's temp dir at any `TMPDIR`
length (measured up to 94 bytes), which holds no library content.

### What is still open

These all put something from a protected library outside its folder:

- **The web view's disk cache.** The app sets no separate store for its
  WKWebView, the server marks `/data` blobs cacheable for a year, and the
  installed app had about 300 MB under `~/Library/Caches/com.imbue.datalib`.
  Only the files were counted, so whether a protected library's content is in
  there is not known.
- **A credential pasted into the wizard** is on `latchkey`'s command line for
  the second that runs. It needs a change upstream
  ([`latchkey.md`](latchkey.md)).
- **The fingerprint cache** is in `~/Library/Caches/datalib` on a Mac, shared by
  every library on the machine. It says which local files a source read. Read
  from source.

### What has not been shown

- All of this on a signed release. The measurements used dev builds, and the
  `ps` one an ad-hoc signature rather than a Developer ID.
- macOS's guard on `system/applet-secret` and `system/api-token` themselves.
  The scratch library was not in the group container, so the measurements above
  show where the secrets are and not that macOS keeps a program from them. The
  checks to run are in `tauri/README.md`, and they do not name these files yet.
- A real sign-in with `TMPDIR` in the folder. A visible Chromium launched by
  hand ran at that path length, but latchkey's Playwright did not run there.
- The Mac before version 15. The launcher page was drawn with stubbed state
  and only the version comparison has a test.

## Other threats, briefly

These have guards in the tree and deserve the same treatment as the one
above. What was open when last checked is in
[`audits/2026-11-03_security.md`](audits/2026-11-03_security.md); read the
code before repeating it as current.

| threat | who | what guards it | start reading at |
|---|---|---|---|
| Something a person received runs script in the app. From there it could rewrite the config, whose step `command` runs any program | a stranger | Every served file gets the sandbox policy and keeps its type only if that type is on a short list; message bodies go through a sanitizer | `set_data_headers` in `http/src/embed.rs`, `ui/tests/e2e/document-sandbox.spec.ts`, [`chat-common/README.md`](../../datalib/backend/etl/chat-common/README.md) |
| A website drives the local server | a website | A per-process API token; a cross-site request is refused unless it is a navigation, and named routes refuse those too | `fetch_policy` and `only_from_this_site` in `http/src/auth.rs` |
| A message's remote image tells its sender you read it | a stranger | Remote media is fetched only for an allowed sender or document, through the server | `http/src/remote_media.rs` |
| A crafted file or export breaks a sync or reaches SQL | a stranger | Values are bound and names quoted; where a parser's work is wrapped in `catching_panics`, a panic costs that one record, not the step | AGENTS.md §"Dynamic SQL needs `AssertSqlSafe` and a reason", `etl/src/run_problems.rs` |
| A secret reaches a log, an error or a child process | any of the above, or an accident | A child process gets a named environment, and an error carries only a safe form of a URL | `datalib/backend/env/src/lib.rs` |
| A link makes a card run code | a website, or a stranger who sent the link | A card's source is data, opening a card never writes, and nothing a query says runs as code: the DACTAL frame has no `eval` and no `'unsafe-eval'` | [`cards.md`](cards.md) § "Card source", [`dactal.md`](dactal.md) caveat 5 |
| datalib reports on you without saying so | us, or a dependency | Nothing we ship sends telemetry: every `latchkey` we start, or that a person runs from our launcher, has its usage count off. A check of the real package, not a scan for others | `latchkey_counting_test`, `README.md` §"Read this before you point an agent at it" |
| A downloaded runtime or model is swapped | whoever controls the host | Every Bazel download, the runtime fetch and the qmd models are pinned by sha256 | [`runtime_fetch.md`](runtime_fetch.md) |
| A person's data lands in this public repo | us, by accident | A rule, not a check | AGENTS.md §"Real data stays out of the repo" |
