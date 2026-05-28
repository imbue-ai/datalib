# First-time user guide

Getting a personal mirror of your Claude conversations onto your laptop,
end-to-end. macOS arm64 only for now.

## 0. Pre-reqs

You'll need a few host tools on `PATH`:

```sh
brew install node gh
```

- `gh` — used below to pull the release tarball.
- `node` — the qmd indexer shells out to `npx -y @tobilu/qmd@<version>`
  during the index phase.

(doltlite is embedded directly in the backend binary; no separate
install is required. The SQL store lives at
`<data_root>/backend_index.doltlite_db`.)

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

3. Paste the registration command into your terminal **but don't run it
   yet** — the next step puts the cookie on your clipboard, so you want
   this command staged first. `pbpaste` is used (instead of pasting the
   cookie value literally) because zsh/bash record the pre-expansion
   command in history, so history ends up storing the harmless
   `$(pbpaste)` text instead of your live session token:

   ```sh
   latchkey auth set claude-ai -H "Cookie: sessionKey=$(pbpaste)"
   ```

4. Open [claude.ai](https://claude.ai) in a logged-in browser tab and
   copy your `sessionKey` cookie. It's `HttpOnly`, so it's not visible
   to `document.cookie` — you have to read it from DevTools directly:

   - Open DevTools → **Application** tab → **Storage** → **Cookies** →
     `https://claude.ai`.
   - Find the row named `sessionKey` and copy its **Value**.

   Now switch back to your terminal and press Enter to run the staged
   command — `$(pbpaste)` will expand to the cookie you just copied.


## 3. Sample configuration

Download [**sample_config.yaml**](https://raw.githubusercontent.com/imbue-ai/mixed_up_files/main/docs/sample_config.yaml)
and drop it at `~/.config/frankweiler/config.yaml` (or anywhere and
point `FRANKWEILER_CONFIG` at it). One-liner:

```sh
mkdir -p ~/.config/frankweiler && \
    curl -fsSL https://raw.githubusercontent.com/imbue-ai/mixed_up_files/main/docs/sample_config.yaml \
    -o ~/.config/frankweiler/config.yaml
```

This config only enables the Claude API source, so it's the minimum
needed to mirror all your conversations.

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
- A `load` phase: rows written into the doltlite SQL store at
  `<data_root>/backend_index.doltlite_db`.
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
├── backend_index.doltlite_db  # doltlite SQL store (grid rows + audit log)
└── .frankweiler/qmd/
    ├── index.sqlite           # search index hit by hybrid / vector queries
    └── models -> ~/.cache/qmd-models
```

A final `Summary` line reports per-source counts (new / updated /
skipped / errors). Exit code is non-zero if any source errored.

## 6. Browse the result

`frankweiler-http` is the single-binary search backend with the web UI
embedded — point it at your data root and it serves everything:

```sh
frankweiler-http ~/mixed_up_files
```

It binds to `http://127.0.0.1:8731` by default and opens that URL in
your default browser. Pass `--no-open` if you'd rather click in
yourself, and set `FRANKWEILER_BIND=127.0.0.1:<port>` to override the
listen address.

## 7. Re-syncing

Re-run `frankweiler-sync` whenever you want to pull new conversations.
The downloader is incremental and the qmd index is content-hashed, so
re-runs against an unchanged corpus are no-ops.
