# Project Data Liberation ✊ - First-time user guide

Liberate your data from silos. Run SOTA AI and data tools on it, on your own terms.

> 🛑 **<span style="color:red">WITH GREAT POWER COMES GREAT RESPONSIBILITY</span>** 🛑
>
> <span style="color:red">**These tools allow you to accumulate a lot of high-value
> data into a single place. Hopefully, the computer where you run these tools is a
> safe place to store this data.**</span>
>
> <span style="color:red">**Please think at least 3x before running an agent on this
> data, then think again. Make sure you understand the full implications of the
> [lethal trifecta](https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/).
> Most of the data accumulated by these tools should be considered both <span style="color:red">**Private
> Data**</span> and <span style="color:red">**Untrusted Content**</span>.**</span>
>
> <span style="color:red">**Also remember that most agentic harnesses are effectively
> (!!!) EXFILTRATION SCRIPTS (!!!), and running them on your private data will
> upload it to a third party where you have very little control over what happens
> with it next. Ask yourself: "would the people who sent me these messages be
> OK with me sending them to Anthropic, OpenAI, or Google?"  Because that's exactly what
> you're doing when you run an agentic harness on this data.**</span>

 <span style="color:red">**Deletes might not actually delete from your local copy.**
 We use Doltlite (a version of SQLite) to keep versions of your data as it changes over time. 
 This can help you recover from unintended data loss, but is a double-edged sword.
 Deletions in your data sources, even if they propagate into the current
 version of your data, as stored and presented by our tools, are still in theory recoverable from
 the version history.  If you truly need to delete, you'll have to remove the whole doltlite_db file,
 not just delete from the data source.
 **</span>

Codenames in this project (`frankweiler`, etc.) are inspired by
[_From the Mixed-Up Files of Mrs. Basil E. Frankweiler_](https://en.wikipedia.org/wiki/From_the_Mixed-Up_Files_of_Mrs._Basil_E._Frankweiler).

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

## 1. Make a data_root playground and download the CLI (here it's `~/datalib`)

This is where the tools will download your data.

The tools themselves can run from anywhere, but for now, you can just download them directly into the data_root directory and run them from there.

```sh
# Make the data_root as our playground.
mkdir -p ~/datalib && cd ~/datalib

gh release download --repo imbue-ai/datalib --clobber --pattern '*.tar.gz' -D /tmp \
    && tar -xzf /tmp/frankweiler-aarch64-apple-darwin.tar.gz --strip-components=1
```

Verify:

```sh
./frankweiler-sync --version
```

## 2. Get access to some data

The options below cover the sources wired into the sample config. For a
fuller per-source cheat sheet on getting your data onto disk — including
Signal and WhatsApp backups off an Android phone — see
[**getting your data**](/docs/user/getting_your_data.md).

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
> local agent inherits this authority for as long as the cookies remain valid.

You don't necessarily need to install `latchkey` — the commands below invoke it via
`npx`, which fetches it on demand (the `node` install from step 0 ships
with `npx`).

### Option 1: Download some Google Takeout data (no Latchkey necessary)

Google Takeout (<https://takeout.google.com>) lets you export your own
data out of Google's silos. Useful targets for this project:

- **Mail** — exports as a single `.mbox` (one file for "All mail
  Including Spam and Trash"). The email source below ingests this
  directly; no credentials needed.
- **Chat**, **Maps (Your Timeline)**, **YouTube history** — also
  exportable; not wired into the sample config yet but live on disk
  the same way once you've unpacked them.

Steps:

1. Go to <https://takeout.google.com>, **Deselect all**, then tick
   just the products you want. For Mail, expand the row and confirm
   **"Include all messages in Mail"** (or pick specific labels).
2. Choose **Export once**, **.zip**, and the largest split size you're
   comfortable with. Submit the request.
3. Google emails you a download link when it's ready (minutes to
   hours, depending on mailbox size). Download the archive(s) and
   unpack them somewhere stable — these instructions assume
   `~/backups/Takeout/`:

   ```sh
   mkdir -p ~/backups
   unzip ~/Downloads/takeout-*.zip -d ~/backups/
   ```

   After unpacking, your Gmail mbox should live at:

   ```
   ~/backups/Takeout/Mail/All mail Including Spam and Trash.mbox
   ```

   The sample config in the next step has an `email` source stanza
   that points at exactly that path.


### Option 2: Register Slack with latchkey (easy, supported flow)

  Register Slack via latchkey's browser flow (the sample config in the
  next step includes a Slack source, so this is needed for the sync to
  succeed):

  ```sh
  npx -y latchkey auth browser slack
  ```

### Option 3: Register Claude web with latchkey (tricky)

This is tricky, requires you to do sketchy things in your browser.

It also might not work inside Minds because of the Chrome handshake issues.
When Minds runs latchkey, it doesn't use our curl shim with the Chrome 131 handshake
because latchkey reaches out to its gateway.

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

Download [**sample_config.yaml**](https://github.com/imbue-ai/datalib/blob/main/docs/user/config_examples/sample_config.yaml)
into your working dir.

This config enables the Slack source, the Claude API source, and an
email source that reads a Google Takeout `.mbox` from disk — enough to
mirror your conversations and your Gmail archive.

Credentials are not in the config — downloaders that need them use `latchkey` at runtime.

You'll want to at least eyeball the config to make sure it is writing to the directory you created.
It's the `data_root` configuration parameter at the top. 

You can also feel free to comment out some of the YAML stanzas that identify different synchronization sources.

If you only want to mirror your Claude conversations, download
[**claude_only.yaml**](https://github.com/imbue-ai/datalib/blob/main/docs/user/config_examples/claude_only.yaml)
instead — it has just the `claude_api` stanza.

For a reference listing every supported source type with realistic
defaults (including both input modes for email and contacts), see
[**all_sources.yaml**](https://github.com/imbue-ai/datalib/blob/main/docs/user/config_examples/all_sources.yaml).

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

**On disk afterwards** (with `data_root: ~/datalib`):

```
~/datalib/
├── raw/                            # one directory per source
│   ├── claude_web/
│   │   ├── entities.doltlite_db
│   │   └── blobs.doltlite_db
│   ├── slack/
│   │   ├── entities.doltlite_db
│   │   └── blobs.doltlite_db
│   ├── fastmail/                   # (mbox source lands here too)
│   │   └── …
│   └── …
├── rendered_md/                    # one .md per conversation, bucketed by provider
│   ├── anthropic/
│   ├── slack/
│   ├── jmap/                       # email (JMAP + mbox both render here)
│   └── …
├── backend_index.doltlite_db       # doltlite SQL store (grid rows + audit log)
├── qmd/
│   ├── index.sqlite                # search index hit by hybrid / vector queries
│   └── models -> ~/.cache/qmd/models
└── sync_summary_<timestamp>.json   # one per run
```

A final `Summary` line reports per-source counts (new / updated /
skipped / errors). Exit code is non-zero if any source errored.

## 5. Browse the result

`frankweiler-http` is the single-binary search backend with the web UI
embedded — point it at your data root and it serves everything:

```sh
./frankweiler-http ./
```

It binds to `http://127.0.0.1:8731` by default and opens that URL in
your default browser. Pass `--no-open` if you'd rather click in
yourself, and set `FRANKWEILER_BIND=127.0.0.1:<port>` to override the
listen address.

## 6. Re-syncing

Re-run `frankweiler-sync` whenever you want to pull new conversations.
The downloader is incremental and the qmd index is content-hashed, so
re-runs against an unchanged corpus are relatively fast no-ops.

## 7. Querying the index directly with qmd

To find relevant markdown content, you can also query the search index directly from the command line by
pointing `qmd` at the sqlite file under your data root via the
`INDEX_PATH` env var:

```sh
INDEX_PATH=~/datalib/qmd/index.sqlite \
    npx -y @tobilu/qmd query "hello"
```

Use `qmd status` against the same `INDEX_PATH` to confirm collections
and document counts.
