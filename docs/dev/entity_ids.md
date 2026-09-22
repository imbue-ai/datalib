# Entity ids: what `grid_rows.uuid` is, and what it is not

Every searchable thing in datalib carries an id. That one string is
doing more jobs than it looks like:

- the **primary key** of `grid_rows`;
- for chat/thread/page rows, the **`markdown_uuid`** of the rendered
  document, and its **`conversation_uuid`**;
- the **`data-section-uuid` anchor** the renderer bakes into the
  markdown body, which the UI scrolls to and highlights;
- the value **`feedback.target_uuids`** stores, unqualified, forever;
- half of a **`/chat/{...}` URL** that has been handed out;
- the **primary key the render store and the index sort by**, which
  decides how many leaves a sync rewrites.

An id that turns out to mean two things breaks the first five at once,
and the feedback and the URL are not recoverable after the fact — a
filed feedback row is a bare string with no provider column beside it.

This document is the rule for minting them. The implementation is
[`datalib/backend/id/src/lib.rs`](../../datalib/backend/id/src/lib.rs).

## The rule

```rust
use datalib_id::{composite_key, Identity, IdNamespace};

let id = Identity::mint(
    IdNamespace::Slack,                  // provider
    source_id,                           // the configured source's group id
    Some(team_id),                       // the upstream account, when the record names one
    "message",                           // entity kind, in the upstream's vocabulary
    composite_key(&[channel_id, ts]),    // the upstream's own key
    Some(date_ms),                       // the row's `created_at`, or None
);
// id.uuid          → grid_rows.uuid / markdown_uuid / the anchor
// id.natural_key   → grid_rows.upstream_id
// id.entity_kind   → grid_rows.upstream_entity_kind
// the account      → grid_rows.upstream_scope
```

One root namespace, one function, five recipe components joined with
`\x1f`, and one stamp. Nothing else mints an id. `Identity` carries
what the id was minted from so the row's backpointer columns cannot
drift from it — build the key once, use it twice.

**The configured source is a component of every id.** Two sources
therefore cannot share an id whatever they hold: each has its own name,
and that is the whole collision story. The trade is deliberate. An id
is a function of the configuration as well as of the data, so a fresh
data root that names its sources differently mints different ids, and
renaming a source's group id (already a migration) re-keys everything
it rendered. What is bought is that two sources over overlapping data
— two label filters on one mailbox, an mbox import beside the live
account it came from — just work, as two sets of rows, with nothing to
refuse and nothing to merge. Finding the same upstream thing across two
sources is a query over the backpointer columns, not something the id
does. Which source a row came from is `markdowns.source_id`.

### The layout: the stamp first, then the hash

An id is RFC 9562's version 8: the leading 48 bits are the record's
`created_at` in unix milliseconds, then the version nibble, then the
bits of a v5 hash over the four-part recipe. Every store here is a
doltlite prolly tree sorted by primary key, and a write rewrites every
leaf its keys fall in, so keys that scatter (a plain hash) cost one leaf
per row and keys that sort by time cost one leaf per batch —
[`etl/README.md` § "What a write costs"](../../datalib/backend/etl/README.md#what-a-write-costs-the-transaction-is-the-unit-and-the-key-decides-the-size)
has the measurement (210 MB of history against 25 MB). A sync's new
messages are the newest things in the store, so they land together at
its right edge. An id with no stamp starts `00000000-0000-8…` and sorts
to the left edge beside every other unstamped row.

**The stamp is the row's `created_at` or nothing.** That is the whole
rule, and the fixture's round-trip check reads the stamp back out of
every uuid and compares it to the row's `created_at_utc`, so a stamp
that is anything else fails there. Two consequences:

- It goes in at the precision the row stores. A provider that stores
  seconds mints from `RecordStampPrecision::stored_ms(date_ms)`; one
  that stores an ISO string mints from `datalib_time::record_stamp_ms`
  of that string. Both are what `created_at_utc` reads back as.
- Present-or-never applies to the stamp as it does to a scope. A stamp
  that is null on one fetch and set on the next re-keys the row, and a
  source that edits a record's stamp re-keys it — a real identity
  change, reported the way any re-key is.

**A document row carries no stamp.** chat-common sets a document's
`created_at` to its earliest item, so the value is derived and moves
when an older message arrives; a re-keyed document orphans its `/chat/`
URL and every feedback row filed against it. Documents are also
rewritten every sync their chat is touched, wherever their first message
fell, so a time prefix buys them no adjacency — clustering at the left
edge does. The same goes for a page datalib composes (a source's
timeseries, a storage report) and for a device row whose stamp is its
latest reading. What carries a stamp is the record with a stamp of its
own: a message, a comment, a PR, a page Notion dated, a PDF whose Info
dictionary dated it.

An edge takes the stamp of its source end (the anchor's, else the
document's), so the edges a render writes beside a message land in the
leaf its row does; a diff row keeps the stamp of the row it is about.

### The account, wherever the record names one

The third component is the upstream account the record belongs to: a
Slack `team_id`, a JMAP `account_id`. It goes in as data — the account
is part of what the record *is* — and out to `grid_rows.upstream_scope`
in the clear, so it is never a secret. Two rules decide whether a
provider has one:

- **It is on every row the provider writes.** "The source could look
  it up" does not count: a GitHub login is not on the PR, so github
  passes `None`. A repository is not an account either; it leads the
  natural key (`{repo}#{number}`) instead.
- **It is present-or-never.** If the value is `Option` and merely
  *usually* set, the first ingest that finds it populated re-keys every
  row minted while it was empty — precisely the silent re-keying this
  crate exists to prevent. Claude's `org_uuid` is the worked example:
  it looks like the textbook account, and it is empty whenever orgs
  aren't mirrored (`sync.projects = false`, or an older ingest) and
  populated afterwards, so claude passes `None`. So do whatsapp
  (`chat.account_jid_row_id` is nullable) and signal (a backup names no
  account at all); see [Where an account was wanted and not
  taken](#where-an-account-was-wanted-and-not-taken).

`None` therefore means one thing: the upstream names no account on the
record. Nothing about uniqueness rides on it — the source component
already keeps two sources apart, and within a source a provider's keys
are unique by construction. The test for a candidate is "will this
value be identical on every future ingest, including the ones
configured differently?" If it can appear later, it is not the account.

### A raw store's keys are the upstream's own

The source id belongs in the *rendered* id and nowhere else. A raw
store under `<name>/ingest/` keys every row by what the download gave —
the upstream's id, a `{team}#{channel}#{ts}`, a Matrix event id, a
profile URL, the bytes' hash — and nothing in a raw store mints an
entity id; the render mints one from the raw key. Two roots that
download the same account under different group ids therefore produce
byte-identical raw stores, and the raw store stays a backup rather than
a function of how it was asked for.
[`data_architecture_ingestion.md`](data_architecture_ingestion.md#object-identity-ship-of-theseus-on-uuids)
has the rule; a render that needs its raw key back (a bucket the
driver named) keeps a map from id to key, as beeper's parse does.

### Rows datalib itself mints

Two kinds of row are about a source's data without being it:

- a source's **storage report** (`datalib_step::introspect`) takes
  `IdNamespace::Datalib` under the source's own group id, so it can
  never collide with the rows it measures;
- a **diff group's** rows are the source's own render, run under the
  diff group's name (`datalib_step::render_diff`). The source
  component of every id is then the diff group's, so the diff's rows
  are distinct from the source's by construction and nothing has to be
  re-keyed. `upstream_id` is the source's: it points at the real thing.

### Why not opaque random ids

A v4 per row makes collisions impossible and is the obvious answer. It
costs the property this codebase is actually built on: ids as a pure
function of upstream data.

- Re-ingest stops being idempotent — every render needs a backpointer
  lookup to find the id it minted last time.
- A fresh data root re-ingesting the same upstream data under the same
  configuration produces *different* ids, so
  `//tests/fixtures:ingested_tng_test` (which asserts byte-identical
  convergence across runs) and every insta golden would have to stop
  asserting on ids.

Determinism is the property to keep. Uniqueness comes from the source
component.

## The backpointer

`uuid` is a one-way hash, so the row also carries what it was minted
from:

| Column | Holds |
|---|---|
| `upstream_id` | The upstream's own id, within the scope |
| `upstream_entity_kind` | The `entity_kind` component — the upstream's vocabulary |
| `upstream_scope` | The upstream account the record names; NULL when it names none |

Together with `provider` (its own column), `markdowns.source_id` (the
source) and `created_at_utc` (the stamp) that is the entire recipe, so
`entity_id(provider, source_id, upstream_scope, upstream_entity_kind,
upstream_id, stamp_of(uuid)) == uuid` holds by construction, with
`stamp_of(uuid)` either zero or the row's `created_at_utc` — and the
fixture recomputes it for every row from those columns alone, with no
per-provider table.

`upstream_entity_kind` is **not** `grid_rows.kind`. `kind` is a display
label for the grid's Kind column ("LLM Thinking", "GitHub PR") and may
be reworded freely; this one may not, because the id depends on it. It
is also what makes the backpointer usable: GitHub numbers issue
comments, reviews and review comments in three independent sequences
that overlap, and each is fetched from a different API path, so a bare
`12345` is ambiguous without it.

**Set `upstream_id` even when it currently equals `uuid`.** A
provider that passes an upstream id through as its primary key loses
that route the moment it moves onto `datalib_id`, and this column is
what the grid's "Copy upstream ID(s)" action reads.

## Guardrails

Two checks stand between a bad recipe and silent data loss. (A third,
`IdClaims`, refused an index run in which two sources claimed one id;
with the source in every id that cannot happen, and it is gone. Two
documents of *one* source minting one id still fails the load —
`grid_index::insert_grid_row` names the document already holding it.)

1. **`//tests/fixtures:ingested_tng_test`** recomputes every row's uuid
   from its backpointer, its source and its stamp and compares
   (`_roundtrip_failures`), asserts `grid_rows.uuid` is unique, and
   that the set of providers minting non-UUID primary keys equals
   `NON_UUID_PK_PROVIDERS` exactly — in both directions, so the
   allowlist cannot rot into a permanent exemption.
2. **`//datalib/ui:e2e_test`** (`grid-copy-ids.spec.ts`) pins that the
   two copy actions land in two different id spaces.

## Known instabilities

An id is supposed to be a pure function of upstream data. Two ported
recipes are keyed on something weaker, and both are position- or
response-shaped rather than issued by the provider:

- **slack reactions** include the reacting user, and whether Slack
  returns `reactions[].users` varies by response rather than by
  reaction. The same reaction is either N per-user rows or one
  aggregate row keyed with an empty user, and a re-fetch in the other
  shape re-keys it. Options and their costs are written out on
  `slack::ids::reaction`.
- **claude `thinking` blocks** (and the fallback for a tool block
  missing its id) are keyed on `(message_uuid, block_index)`, where the
  index is the block's position in the message's `content` array.
  Claude's content order is meaningful, so this is stable in practice —
  but it is derived from position, not from anything Anthropic issued,
  and a re-fetch yielding a different block set moves every id after
  the change point.

Neither is caught by the reproducibility check below: that run replays
one fixed payload, so a value that varies *between* upstream responses
never varies within it.

### What the reproducibility check does and does not cover

`ingested_tng_test`'s run 4 wipes the data root and ingests the same
TNG fixture again, then asserts every id is byte-identical to run 1.
Runs 2 and 3 cannot do this: both find their raw stores unmoved, so
no id in them is recomputed. Verified by sabotage — a wall-clock salt in
an id recipe passes runs 1–3 and fails run 4.

It catches a recipe that reads the clock, an RNG, or an unfixed
iteration order, and a renderer that reads ids back off the index
rather than deriving them.

Every id depends on the source's group id by design, and the driver
regenerates the same group ids every run, so the check says nothing
about a recipe that reads *more* of the config than that. The
round-trip check stands in: a row's uuid has to come back from its
provider, its source, its scope, its kind, its key and its stamp, so
anything else a recipe folded in fails there.

## Porting status

Every provider that renders mints through `entity_id`: the
`IdNamespace` variants are exactly the `grid_rows.provider` tags, the
`NON_UUID_PK_PROVIDERS` allowlist is empty, and
`//tests/fixtures:ingested_tng_test` round-trips every row of every
provider in the fixture from the row's own columns. What is left
outside `datalib_id` is a raw store's own
row key where an export carries no id — facebook, linkedin,
google_takeout, sms_backup_restore hash the record; contacts
synthesizes a `UID` — and those are the raw store's business, read
back by render as the natural key.

| Provider | Account | Stamped rows |
|---|---|---|
| airvisual | none — the page keyed on the source id, a device on its serial | none — a device's stamp is its latest reading |
| apple_messages | none — `message.guid` is a UUID Messages mints | messages, tapbacks |
| beeper | none — `rooms.account_id` is nullable; keys are Matrix ids | events, at millisecond precision |
| chatgpt | none — keys are OpenAI's conversation and message ids | messages |
| claude | none — `org_uuid` is nullable (see above); keys are Anthropic's uuids | messages, blocks, project documents |
| claude_code | none — session ids, record uuids and tool-use ids are all Claude Code's own | records, blocks |
| contacts | none — keyed on `addressbook#uid` | none — a card has no creation event |
| email | `account_id` — the JMAP account, the Gmail address, or the mbox's configured id | emails |
| facebook | none — keyed on the raw row id (`fbid` where the record has one, else a hash of it) | posts, comments, reactions, photos |
| garmin | none — the page keyed on the source id, a device on Garmin's id | none |
| github, gitlab | none — the repository / project leads the key: `{repo}#{number}` | PRs, MRs, comments, reviews, notes — the record's own `created_at` |
| google_takeout | none — a Chat message id names its space, a Voice row id is the ingest's | messages |
| linkedin | none — keyed on the profile URL, the post link, the raw row id | messages, shares, comments |
| notion | none — page, discussion and comment ids are Notion UUIDs, now the backpointer rather than the key; `notion_page_uuid` holds the page's datalib id | pages, comments — Notion's `created_time` |
| pdf | none — keyed on the blake3, so two copies within a source are one row | documents and pages, when the Info dictionary dates the file |
| perseus | none — keyed on the CTS locator and edition | none — a classical text has no stamp of its own |
| signal | none — keyed on the backup's local ids | messages, on `date_sent` |
| slack | `team_id` | messages, reactions — the `ts` in the key |
| sms_backup_restore | none — keyed on the raw row id | messages, calls |
| whatsapp | none — keyed on the chat JID | messages, reactions |
| yolink | none — keyed on the device's config name; the ids YoLink issues a device are read secrets, and a natural key is stored in the clear | none |

### Where an account was wanted and not taken

Three providers have a candidate account on the record, and each fails
present-or-never. Take one only after verifying the column on a real
backup, and expect a re-key when you do:

- **whatsapp** — `chat.account_jid_row_id` names the account, but the
  column is nullable and the fixture leaves it so.
- **signal** — `ParsedRecipient.identifier` is the e164 or ACI of the
  *peer*; the backup carries no identifier for the account itself.
- **beeper** — `rooms.account_id` is nullable.

### Adding a provider, or changing a recipe

1. Add an `ids` module whose functions return `datalib_id::Identity`
   through `Identity::mint`. Returning the pair is what keeps
   `upstream_id` and `uuid` from drifting — build the key once and use
   it twice. Use `datalib_id::composite_key` for tuple keys, and pass
   the item's stamp (`None` for a document).
2. Populate the backpointer columns. For chat-common providers that
   means `NormalizedChat::upstream_scope` (the account, when there is one),
   `RenderProfile::chat_entity_kind`, `source_ref` on every item
   **and every reaction** (reactions get their own grid_rows and are
   easy to miss — that was a real bug), and `NormalizedDoc::source_ref`
   on every bucket whose id is not the chat's own — a period of a chat,
   a subagent's transcript. For contact-common providers,
   `ContactRenderProfile::contact_entity_kind` and
   `NormalizedContact::{external_id, upstream_scope}`.
3. Thread `source_id` — the render's `ctx.name` — to wherever the ids
   are minted; a bucket key the driver hands back is the raw key, so
   a parse that narrows by it maps the id back (beeper, chatgpt,
   claude, email, slack all do).
4. Bump the provider's `RENDER_VERSION`. A re-key moves `chat_uuid`,
   which *names the output directory*, so the new documents land beside
   the old ones rather than over them and the index loads both. The
   render step handles that — a store whose documents carry a version
   this build doesn't produce is deleted and re-rendered from the raw
   store.
   Skip the bump and the port silently does nothing to any data root
   that already exists: the raw store has not moved, so nothing
   re-renders and the old ids stay.

   Every render processor already returns its constant from
   `DataProcessor::render_version`, and the render step fails a source
   that writes documents without declaring one — so a *new* provider
   can't inherit the old behaviour by omission, and a wrong constant is
   caught by `//tests/fixtures:ingested_tng_test` rather than by a user
   noticing every conversation twice.

### What a re-key costs an existing data root

Ids change, so anything holding one breaks. Filed feedback pointing at
old `grid_rows.uuid`s does not survive, and that is not recoverable.

What *is* handled, as of the fix for #216's fallout — neither needs a
human to delete anything:

- **The index.** `grid_index::init_schema` compares the on-disk
  `grid_rows` / `markdowns` / `edges` against their DDL and rebuilds all
  three from the per-source render stores when they disagree. Before
  that, a root
  predating the `external_id` → `upstream_id` rename answered every
  read *and* every write with `no such column: upstream_id`.
- **The rendered tree.** The render step discards a tree stamped with a
  foreign `render_version`, cursor included, and re-renders from the
  raw store.

Both are derived data, so the cost is a re-render plus a re-index. No
re-download: a raw store's keys are the upstream's own and never move
with a recipe.

The round-trip check is not a formality. It caught three real bugs
across the first three ports, each invisible to every other test: a
composite key spelled `#` in the column and `\x1f` in the recipe; a
Claude Project stamped `"conversation"` while minted as `"project"`;
and slack's reaction rows carrying no backpointer at all.
