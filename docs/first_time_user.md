# Porject Data Liberation ✊ - First-time user guide

> Codenames in this project (`frankweiler`, etc.) are inspired by
> [_From the Mixed-Up Files of Mrs. Basil E. Frankweiler_](https://en.wikipedia.org/wiki/From_the_Mixed-Up_Files_of_Mrs._Basil_E._Frankweiler).

Liberate your data from silos. Run SOTA AI and data tools on it, on your own terms.

## 0. Setup pre-reqs

If you don't already have them, you'll need a few host tools on `PATH`:

```sh
brew install node gh
```

- `gh` — used below to pull the release tarball (since the repo is not public)
- `node` — the qmd indexer shells out to latchkey, and `npx -y @tobilu/qmd@<version>` 
  during the index phase.

To access the Imbue-private repo, also make sure you're authenticated with GitHub for the `gh` download:

```sh
gh auth login
```

## 1. Make a data_root playground and download the CLI

Make a "data root", for example here: `~/mixed_up_files`.  This is where the tools will download your data.

You don't have to, but you can also download the tools directly into this directory as well.
Doing so is convenient for this demo:

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

## 2. Get access to some data

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

You don't need to install `latchkey` — the commands below invoke it via
`npx`, which fetches it on demand (the `node` install from step 0 ships
with `npx`).

### Option: Download some Google Takeout

FIXME: Add instructions about how to go to Google Takeout and request a download of email, Gchat, Maps History, YouTube History, etc. 


### Option: Register Slack with latchkey (easy, supported flow)

  Register Slack via latchkey's browser flow (the sample config in the
  next step includes a Slack source, so this is needed for the sync to
  succeed):

  ```sh
  npx -y latchkey auth browser slack
  ```

### Option: Register Claude web with latchkey (tricky, needs browser)


a. Register the `claude-ai` service with latchkey (one-time):

   ```sh
   npx -y latchkey services register claude-ai --base-api-url="https://claude.ai/"
   ```

b. Paste the registration command into your terminal **but don't run it
   yet** — the next step puts the cookie on your clipboard, so you want
   this command staged first. `pbpaste` is used (instead of pasting the
   cookie value literally) because zsh/bash record the pre-expansion
   command in history, so history ends up storing the harmless
   `$(pbpaste)` text instead of your live session token:

   ```sh
   npx -y latchkey auth set claude-ai -H "Cookie: sessionKey=$(pbpaste)"
   ```

c. Open [claude.ai](https://claude.ai) in a logged-in browser tab and
   copy your `sessionKey` cookie. It's `HttpOnly`, so it's not visible
   to `document.cookie` — you have to read it from DevTools directly:

   - Open DevTools → **Application** tab → **Storage** → **Cookies** →
     `https://claude.ai`.
   - Find the row named `sessionKey` and copy its **Value**.

   Now switch back to your terminal and press Enter to run the staged
   command — `$(pbpaste)` will expand to the cookie you just copied.


## 3. Sample configuration

Download [**sample_config.yaml**](https://github.com/imbue-ai/mixed_up_files/blob/main/docs/sample_config.yaml)
into your working dir.

This config only enables the Slack and the Claude API source, so it's the minimum
needed to mirror those your conversations.

Credentials are not in the config — downloaders that need them use `latchkey` at runtime.

You'll want to at least eyeball the config to make sure it is writing to the directory you created.
It's the `data_root` configuration parameter at the top. 

You can also feel free to comment out some of the YAML stanzas that identify different synchronization sources.

## 4. Run the sync

```sh
./frankweiler-sync --config ./sample_config.yaml
```

The first time you run this, it is slow and takes a long time to download everything.
All of the data will be going into the directory that sample config points at. 

This process is meant to be stoppable and resumable, so you can control-C it,
Then run the same command again to resume downloading.
It does do some database commits when you control-C, so that part is not instant. 

Subsequent runs of the same command are meant to be incremental delta downloads,
and should be faster.

**During the run** you'll see, roughly in order:

- An `extract` phase: per-org conversation enumeration, then a progress
  bar as each new / updated / overlap conversation is fetched from
  `claude.ai/api`. New conversations are fetched first.
- A `translate` phase: each conversation rendered into intelligible Markdown (including image attachments).
- A SQL `index` phase: rows written into the doltlite SQL store at `<data_root>/backend_index.doltlite_db`.
- A `qmd index` phase: builds the search index. **First run is slow** —
  embedding ~5–10 minutes per thousand chunks on CPU. It's resumable, so
  Ctrl-C and re-run is safe. Re-runs after the backlog drains take
  seconds.

**On disk afterwards** (with `data_root: ~/mixed_up_files`):

```
FIXME: Look at current correct shape under: ~/mixed_up_files.thad_dev_7
~/mixed_up_files/
├── raw/anthropic-api/
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
re-runs against an unchanged corpus are relatively fast no-ops.

## 8. Querying the index directly with qmd

To find relevant markdown content, you can also query the search index directly from the command line by
pointing `qmd` at the sqlite file under your data root via the
`INDEX_PATH` env var:

```sh
INDEX_PATH=~/mixed_up_files/qmd/index.sqlite \
    npx -y @tobilu/qmd query "hello"
```

Use `qmd status` against the same `INDEX_PATH` to confirm collections
and document counts.
