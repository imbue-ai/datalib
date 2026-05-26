# First-time user guide

Getting a personal mirror of your Claude conversations onto your laptop,
end-to-end. macOS arm64 only for now.

## 0. Pre-reqs

You'll need a few host tools on `PATH`:

```sh
brew install node gh dolt
```

- `gh` — used below to pull the release tarball.
- `dolt` — `frankweiler-sync` manages a `dolt sql-server` subprocess
  against `<data_root>/dolt_db/`.
- `node` — the qmd indexer shells out to `npx -y @tobilu/qmd@<version>`
  during the index phase.

Also make sure you're authenticated with GitHub for the `gh` download:

```sh
gh auth login
```

## 1. Install the CLI

Pull the latest release tarball from GitHub and drop the binaries into
`~/.local/bin`:

```sh
mkdir -p ~/.local/bin
gh release download --repo imbue-ai/mixed_up_files --clobber --pattern '*.tar.gz' -D /tmp \
    && tar -xzf /tmp/frankweiler-aarch64-apple-darwin.tar.gz -C ~/.local/bin --strip-components=1
```

Make sure `~/.local/bin` is on your `PATH`. Verify:

```sh
frankweiler-sync --version
```

## 2. Set up `latchkey` for Claude

`frankweiler-sync` does not handle `claude.ai` cookies itself. It shells
out to [`latchkey curl`](https://github.com/imbue-ai/latchkey), which
injects the cookies registered under the `claude-ai` service. `claude.ai`
is fronted by Cloudflare's managed-challenge system, so the underlying
`curl` has to impersonate Chrome's TLS fingerprint — `frankweiler-sync`
takes care of that internally by pointing latchkey at the bundled
`latchkey-curl-shim` (`wreq`-backed, Chrome 131 handshake). You don't
need to set `LATCHKEY_CURL` yourself.

1. Install `latchkey` (see its repo for instructions) and make sure it's
   on your `PATH`.
2. Register the `claude-ai` service with latchkey (one-time):

   ```sh
   latchkey services register claude-ai --base-api-url="https://claude.ai/"
   ```

3. Open [claude.ai](https://claude.ai) in a logged-in browser tab, open
   DevTools → Console, and paste this snippet. It reads your
   `sessionKey` cookie and prints a ready-to-run `latchkey` command:

   ```js
   (() => {
     const sk = document.cookie.split('; ').find(c => c.startsWith('sessionKey='));
     if (!sk) { console.error('sessionKey cookie not found — are you logged into claude.ai?'); return; }
     const value = decodeURIComponent(sk.slice('sessionKey='.length));
     console.log(`latchkey auth set claude-ai -H 'Cookie: sessionKey=${value}'`);
   })();
   ```

4. Copy the printed command into your terminal and run it.


## 3. Sample configuration

Drop the following at `~/.config/frankweiler/config.yaml` (or anywhere
and point `FRANKWEILER_CONFIG` at it). This config only enables the
Claude API source, so it's the minimum needed to mirror all your
conversations:

```yaml
# ~/.config/frankweiler/config.yaml -- or anywhere you look.
data_root: ~/mixed_up_files  # Where the downloaded data gets written.

dolt:
  port: 3306

sources:
  - name: anthropic-api
    type: claude_api
    sync: {}
      # overlap: 50              # re-fetch the N most-recently-updated
      # refresh_window_days: 7   # treat anything older than this as cold
```

Credentials are not in the config — every downloader uses `latchkey` at runtime.

## 4. Run the sync

```sh
frankweiler-sync \
    --config ~/.config/frankweiler/config.yaml \
    --now "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
```

`--now` is the timestamp threaded through renderers and the Dolt load so
re-runs are deterministic.

## 5. What to expect

First run does the slow work; subsequent runs are mostly cache hits.

**During the run** you'll see, roughly in order:

- An `extract` phase: per-org conversation enumeration, then a progress
  bar as each new / updated / overlap conversation is fetched from
  `claude.ai/api`. New conversations are fetched first.
- A `translate` phase: each conversation rendered into the export-shaped
  JSON cache and then into Markdown.
- A `load` phase: rows written into a managed `dolt sql-server` at
  `<data_root>/dolt_db/`.
- A `qmd index` phase: builds the search index. **First run is slow** —
  embedding ~5–10 minutes per thousand chunks on CPU. It's resumable, so
  Ctrl-C and re-run is safe. Re-runs after the backlog drains take
  seconds.

**On disk afterwards** (with `data_root: ~/mixed_up_files`):

```
~/mixed_up_files/
├── raw/anthropic-api/
│   ├── conversations.json     # export-shaped cache, the source of truth for incremental
│   └── users.json
├── rendered_md/               # one .qmd per conversation
├── dolt_db/                   # managed Dolt repo (rows for the grid)
└── .frankweiler/qmd/
    ├── index.sqlite           # search index hit by hybrid / vector queries
    └── models -> ~/.cache/qmd-models
```

A final `Summary` line reports per-source counts (new / updated /
skipped / errors). Exit code is non-zero if any source errored.

To browse the result, launch the UI from a source checkout:

```sh
bazelisk run //frankweiler:dev -- ~/mixed_up_files
```

Re-run `frankweiler-sync` whenever you want to pull new conversations.
The downloader is incremental and the qmd index is content-hashed, so
re-runs against an unchanged corpus are no-ops.
