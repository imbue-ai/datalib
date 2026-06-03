# First-time user guide

> Codenames in this project (`frankweiler`, etc.) are inspired by
> [_From the Mixed-Up Files of Mrs. Basil E. Frankweiler_](https://en.wikipedia.org/wiki/From_the_Mixed-Up_Files_of_Mrs._Basil_E._Frankweiler).

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

## 1. Make a playground and install the CLI

Make a working directory, pull the latest release tarball from GitHub,
and extract it in place. All subsequent commands run from this
directory:

```sh
# Our playground
mkdir -p ~/mixed_up_files && cd ~/mixed_up_files

gh release download --repo imbue-ai/mixed_up_files --clobber --pattern '*.tar.gz' -D /tmp \
    && tar -xzf /tmp/frankweiler-aarch64-apple-darwin.tar.gz --strip-components=1
```

Verify:

```sh
./frankweiler-sync --version
```

## 2. Set up `latchkey`

> 🛑 **RED WARNING — READ BEFORE PROCEEDING** 🛑
>
> The commands in this section store live session cookies for `claude.ai`
> and Slack on your machine via `latchkey`. **Any process, script, or AI
> agent that can run CLI programs as your user account can invoke
> `latchkey` (or read its store) and thereby ACT AS YOU on these
> services** — read every conversation, send messages, change settings,
> impersonate you to coworkers, etc. There is no additional password
> prompt, MFA challenge, or confirmation gate between a shell command
> and your identity on these services.
>
> Only run these steps on a machine you trust, and be aware that *every*
> local agent (including this one) inherits this authority for as long
> as the cookies remain valid.

`frankweiler-sync` does not handle `claude.ai` cookies itself. It shells
out to [`latchkey curl`](https://www.npmjs.com/package/latchkey), which
injects the cookies registered under the `claude-ai` service. `claude.ai`
is fronted by Cloudflare's managed-challenge system, so the underlying
`curl` has to impersonate Chrome's TLS fingerprint — `frankweiler-sync`
takes care of that internally by pointing latchkey at the bundled
`latchkey-curl-shim` (`wreq`-backed, Chrome 131 handshake). You don't
need to set `LATCHKEY_CURL` yourself.

You don't need to install `latchkey` — the commands below invoke it via
`npx`, which fetches it on demand (the `node` install from step 0 ships
with `npx`).

a. Register Slack via latchkey's browser flow (the sample config in the
   next step includes a Slack source, so this is needed for the sync to
   succeed):

   ```sh
   npx -y latchkey auth browser slack
   ```


b. Register the `claude-ai` service with latchkey (one-time):

   ```sh
   npx -y latchkey services register claude-ai --base-api-url="https://claude.ai/"
   ```

c. Paste the registration command into your terminal **but don't run it
   yet** — the next step puts the cookie on your clipboard, so you want
   this command staged first. `pbpaste` is used (instead of pasting the
   cookie value literally) because zsh/bash record the pre-expansion
   command in history, so history ends up storing the harmless
   `$(pbpaste)` text instead of your live session token:

   ```sh
   npx -y latchkey auth set claude-ai -H "Cookie: sessionKey=$(pbpaste)"
   ```

d. Open [claude.ai](https://claude.ai) in a logged-in browser tab and
   copy your `sessionKey` cookie. It's `HttpOnly`, so it's not visible
   to `document.cookie` — you have to read it from DevTools directly:

   - Open DevTools → **Application** tab → **Storage** → **Cookies** →
     `https://claude.ai`.
   - Find the row named `sessionKey` and copy its **Value**.

   Now switch back to your terminal and press Enter to run the staged
   command — `$(pbpaste)` will expand to the cookie you just copied.


## 3. Sample configuration

Download [**sample_config.yaml**](https://github.com/imbue-ai/mixed_up_files/blob/main/docs/sample_config.yaml)
into your working dir (the `?token=...` query param is a short-lived
GitHub raw access token — the repo is private):

```sh
curl "https://raw.githubusercontent.com/imbue-ai/mixed_up_files/refs/heads/main/docs/sample_config.yaml?token=GHSAT0AAAAAADGT6DOKTRDYTP635AE7B3WA2QZ23WA" \
    -o sample_config.yaml
```

This config only enables the Slack and the Claude API source, so it's the minimum
needed to mirror all your conversations.

Credentials are not in the config — every downloader uses `latchkey` at runtime.

## 4. Run the sync

```sh
./frankweiler-sync --config ./sample_config.yaml
```

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
    └── models -> ~/.cache/qmd/models
```

A final `Summary` line reports per-source counts (new / updated /
skipped / errors). Exit code is non-zero if any source errored.

## 6. Browse the result

`frankweiler-http` is the single-binary search backend with the web UI
embedded — point it at your data root and it serves everything:

```sh
./frankweiler-http ./
```

It binds to `http://127.0.0.1:8731` by default and opens that URL in
your default browser. Pass `--no-open` if you'd rather click in
yourself, and set `FRANKWEILER_BIND=127.0.0.1:<port>` to override the
listen address.

## 7. Re-syncing

Re-run `frankweiler-sync` whenever you want to pull new conversations.
The downloader is incremental and the qmd index is content-hashed, so
re-runs against an unchanged corpus are no-ops.

## 8. Querying the index directly with qmd

You can also query the search index directly from the command line by
pointing `qmd` at the sqlite file under your data root via the
`INDEX_PATH` env var:

```sh
INDEX_PATH=~/mixed_up_files/qmd/index.sqlite \
    npx -y @tobilu/qmd query "hello"
```

Use `qmd status` against the same `INDEX_PATH` to confirm collections
and document counts.
