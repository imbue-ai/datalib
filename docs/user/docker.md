# Running datalib in Docker

The container is the cautious way to try datalib. It sees only the
folders you mount into it, so the binaries never touch your host, and
neither do the credentials you give them. The image is
`ghcr.io/imbue-ai/datalib`, built for Linux on x86_64 and arm64, and it
runs on macOS and Linux under Docker Desktop, Docker Engine, or Colima.

It ships with a small demo data library already ingested — seven
sources of Star Trek: TNG-themed fixture data — so you can see the app
before you hand it anything of yours. The shell blocks below are run
against every published image by `datalib/docker/doc_test.sh`, so they
are known to work as written.

Set a few variables once, in the shell you'll use for the rest of this
page:

<!-- doc-test: setup -->
```sh
IMG=ghcr.io/imbue-ai/datalib:latest
PORT=8731                        # host port the web UI will be on
TOKEN="$(openssl rand -hex 16)"  # the API token; the URL below carries it
DATA_ROOT="$HOME/datalib-docker" # where your own data will go (step 2)
MBOX="$HOME/backups/Takeout/Mail/All mail Including Spam and Trash.mbox"
```

<!-- doc-test: run pull -->
```sh
docker pull "$IMG"
```

## 1. Try the demo

The demo library lives inside the image at `/opt/datalib/demo`. Serve
it:

<!-- doc-test: run demo-serve -->
```sh
docker run -d --name datalib-demo \
  -p "127.0.0.1:$PORT:8731" \
  -e DATALIB_BIND=0.0.0.0:8731 \
  -e DATALIB_TOKEN="$TOKEN" \
  "$IMG" datalib-http --no-open /opt/datalib/demo
echo "http://127.0.0.1:$PORT/?token=$TOKEN"
```

Open the URL it prints. The token in it is the app's API key, the same
way a Jupyter server's URL carries one; your browser trades it for a
cookie on the first load. The port is published on loopback only, so
nothing else on your network can reach it.

You are looking at Captain Picard's Claude conversations, a Gmail
Takeout, Google Chat and Voice, texts and calls, a LinkedIn export,
contacts and a folder of PDFs, all in one grid. Keyword search works
right away. Semantic search needs an index the image doesn't carry
(building one during the image build would slow every release down), so
build it now — a minute or so on a laptop, and the models are already in
the image:

<!-- doc-test: run demo-index -->
```sh
docker exec datalib-demo datalib-dag /opt/datalib/demo/config.toml
```

The same thing happens if you press **Sync all** on the Manage tab.
Every store in the library is a database you can query, from inside
the container or from any other one with the folder mounted:

<!-- doc-test: run demo-sql -->
```sh
docker exec datalib-demo datalib-doltlite -readonly \
  /opt/datalib/demo/unified_index/grid_index/db.doltlite_db \
  "SELECT provider, count(*) FROM grid_rows GROUP BY 1;"
```

When you're done:

<!-- doc-test: run demo-stop -->
```sh
docker rm -f datalib-demo
```

## 2. Your own data, from a file on disk

The first real source to try is one that needs no credentials: an
export you already have. This walkthrough uses a Gmail `.mbox` from
[Google Takeout](getting_your_data.md#google-takeout), at the `MBOX`
path you set above; any of the file-backed sources in
[`all_sources.toml`](config_examples/all_sources.toml) works the same
way.

The container gets exactly two folders: your data root, read-write,
and the folder holding the export, read-only at `/import`. Write the
config first:

<!-- doc-test: run own-config -->
```sh
mkdir -p "$DATA_ROOT"
cat > "$DATA_ROOT/config.toml" <<EOF
[[groups]]
id = "gmail"
name = "Gmail (Takeout)"
type = "email"

[[steps]]
group = "gmail"
function = "ingest"
[steps.params.common]
input_path = "/import/$(basename "$MBOX")"
always_clear_before_ingest = true
[steps.params.mbox]
account_id = "you@gmail.com"
display_name = "You"
email_address = "you@gmail.com"
is_personal = true

[[steps]]
group = "gmail"
function = "render_markdown"
inputs = ["gmail/ingest"]
[steps.params]
outlink_format = "gmail"

[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "grid_index"
inputs = ["gmail/render_markdown"]

[[steps]]
group = "unified_index"
function = "qmd_index"
inputs = ["gmail/render_markdown"]

[[applets]]
group = "unified_index"
id = "unified_index"
command = "datalib-applet unified_index"
EOF
```

There is no `data_root` line: it defaults to the folder the config is
in, which inside the container is `/data`. Ingest, render and index:

<!-- doc-test: run own-ingest -->
```sh
docker run --rm \
  -v "$DATA_ROOT:/data" \
  -v "$(dirname "$MBOX"):/import:ro" \
  "$IMG" datalib-dag /data/config.toml
```

The last step embeds every message for semantic search, which on a
laptop CPU takes roughly five to ten minutes per thousand messages. It
is resumable: Ctrl-C, run the same command again, and it picks up where
it stopped. Then serve it, mounting the export again so **Sync all**
in the app can re-read it:

<!-- doc-test: run own-serve -->
```sh
docker run -d --name datalib \
  -p "127.0.0.1:$PORT:8731" \
  -e DATALIB_BIND=0.0.0.0:8731 \
  -e DATALIB_TOKEN="$TOKEN" \
  -v "$DATA_ROOT:/data" \
  -v "$(dirname "$MBOX"):/import:ro" \
  "$IMG" datalib-http --no-open /data
echo "http://127.0.0.1:$PORT/?token=$TOKEN"
```

Everything the pipeline wrote is in `$DATA_ROOT` on your machine, in
the layout the [first-time guide](first_time_user.md#4-run-the-sync)
describes. Query it without the server running at all:

<!-- doc-test: run own-sql -->
```sh
docker run --rm -v "$DATA_ROOT:/data" "$IMG" datalib-doltlite -readonly \
  /data/unified_index/grid_index/db.doltlite_db \
  "SELECT provider, count(*) FROM grid_rows GROUP BY 1;"
```

<!-- doc-test: run own-stop -->
```sh
docker rm -f datalib
```

On a Linux host the files the container writes are owned by root, so
edit `config.toml` with `sudo` or `chown` the folder afterwards. Docker
Desktop on macOS maps them to your user.

## 3. Live sources: credentials inside the container

A web source such as Slack, Claude.ai or Gmail authenticates through
[latchkey](https://github.com/imbue-ai/latchkey), which the image
includes. Its credential store is one more folder you mount, and the
container generates the key that encrypts it on first use and keeps
that key in the same folder. So the folder is worth exactly what the
credentials in it are worth: keep it mode 700, and treat it like a
file of passwords, because that is what it is.

```sh
LATCHKEY_DIR="$HOME/.datalib-docker/latchkey"
mkdir -p "$LATCHKEY_DIR" && chmod 700 "$LATCHKEY_DIR"
```

Credentials you paste go in directly. For Claude.ai, copy the
`sessionKey` cookie as described in
[getting your data](getting_your_data.md#claudeai), then:

```sh
docker run --rm -v "$LATCHKEY_DIR:/root/.latchkey" "$IMG" \
  latchkey services register claude-ai --base-api-url=https://claude.ai/
pbpaste | docker run --rm -i -v "$LATCHKEY_DIR:/root/.latchkey" "$IMG" \
  sh -c 'latchkey auth set claude-ai -H "Cookie: sessionKey=$(cat)"'
```

The browser login flows (`latchkey auth browser slack`, `google-gmail`,
`github`, `fastmail`) cannot run inside the container, which has no
browser. Run them on your host as usual, then re-encrypt the result
into the container's store. The first container run against the folder
creates its key; the re-encrypt reads that key from standard input:

```sh
docker run --rm -v "$LATCHKEY_DIR:/root/.latchkey" "$IMG" latchkey auth list
npx -y latchkey auth browser slack
npx -y latchkey auth re-encrypt "$LATCHKEY_DIR" --services slack \
  < "$LATCHKEY_DIR/encryption_key"
docker run --rm -v "$LATCHKEY_DIR:/root/.latchkey" "$IMG" latchkey auth list
```

The last line should show `slack` as valid. Add the source to
`$DATA_ROOT/config.toml` (the
[first-time guide](first_time_user.md#3-configuration) and
[`all_sources.toml`](config_examples/all_sources.toml) have every
source's entry), then sync with the credential store mounted read-only:

```sh
docker run --rm \
  -v "$LATCHKEY_DIR:/root/.latchkey:ro" \
  -v "$DATA_ROOT:/data" \
  "$IMG" datalib-dag /data/config.toml
```

Add the same `-v "$LATCHKEY_DIR:/root/.latchkey:ro"` to the
`datalib-http` command in step 2 and **Sync all** in the app works
too.

## What the container can see

| Host path | In the container | Mode | Holds |
|---|---|---|---|
| `$DATA_ROOT` | `/data` | read-write | `config.toml`, one folder per source, the indexes, the server's own state |
| an export's folder | `/import` | read-only | whatever a file-backed source reads; the name is a convention, not a requirement |
| `$LATCHKEY_DIR` | `/root/.latchkey` | read-write to add a credential, read-only to sync | latchkey's encrypted store and the key that opens it |

Nothing else. In particular, don't mount your home directory, and don't
run the container with `--privileged`; the point of the container is
that the blast radius of a bad day is the folders in this table.

The image bundles the three qmd models, so semantic search never
downloads anything. Building the image yourself, and what is in it, is
in [`docs/dev/docker.md`](../dev/docker.md).
