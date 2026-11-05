# Threat models

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

Not written up yet, and not yet decided whether we defend against them:
other programs on the same machine, a stolen laptop, and code a person
chose to install as a package or applet.

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

The third way in skips the rest. The write key is a repository secret, so
a workflow edited on any branch can name it, with no PR and no merge.

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
| A release reads no cache: no BuildBuddy key, no restored repository cache (check 21) | An entry written on `main`, or with a stolen key, reaching what a release ships. On the mac legs this was the repository cache: v0.49.0's restored an entry saved on `main` by `latchkey-auth.yml`'s daily run, while BuildBuddy gave them no hits | Bad code a release compiles itself (item 1). It also does not cover `docker-publish`'s layer cache, below |
| Two BuildBuddy orgs | A laptop or a PR run writing what `main`'s CI replays | Anything that runs on `main` |
| The write key only on a push to `main` | A PR's code getting the key before it merges | A workflow edited on a branch: only `test.yml`'s own expression holds the key to `main` |
| Publish and sign jobs build nothing (`release_steps.md`; check 21) | A build taking the signing key or a write token, or rewriting a published release | Bad code in what it builds |
| The `v*` tag ruleset | Anyone outside the team starting or moving a release | What a release does once started |

### What is still open

- **`docker-publish` still uses a shared layer cache.** Its `docker
  buildx` builds read and write `type=gha` entries, and a planted layer
  would ship in the image. `main` runs nothing that writes that scope, so
  this needs something on `main` to get GitHub's cache token out of a
  job, which `run:` steps are not handed (reasoned; not tried, and not
  measured how often a tag's build hits).
- **Bad code a release compiles itself** (item 1 above). The burn-in and
  `cargo deny` are all that stand in front of it.
- **The write key sits where crates compile.** `test.yml`'s three jobs
  that run on a push to `main` hold it, and it cannot be hidden from
  build scripts on those runners. Its reach is `main`'s CI.
- **Any write-access account can reach the write key** through a branch
  workflow. A GitHub environment limited to `main` would close this one
  route without a review gate.
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
| A downloaded runtime or model is swapped | whoever controls the host | Every Bazel download, the runtime fetch and the qmd models are pinned by sha256 | [`runtime_fetch.md`](runtime_fetch.md) |
| A person's data lands in this public repo | us, by accident | A rule, not a check | AGENTS.md §"Real data stays out of the repo" |
