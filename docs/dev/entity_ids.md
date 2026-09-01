# Entity ids: what `grid_rows.uuid` is, and what it is not

Every searchable thing in datalib carries an id. That one string is
doing more jobs than it looks like:

- the **primary key** of `grid_rows`;
- for chat/thread/page rows, the **`markdown_uuid`** of the rendered
  document, and its **`conversation_uuid`**;
- the **`data-section-uuid` anchor** the renderer bakes into the
  markdown body, which the UI scrolls to and highlights;
- the value **`feedback.target_uuids`** stores, unqualified, forever;
- half of a **`/chat/{...}` URL** that has been handed out.

An id that turns out to mean two things breaks all five at once, and the
last two are not recoverable after the fact — a filed feedback row is a
bare string with no provider column beside it.

This document is the rule for minting them. The implementation is
[`datalib/backend/id/src/lib.rs`](../../datalib/backend/id/src/lib.rs).

## The rule

```rust
use datalib_id::{entity_id_str, Scope};

let uuid = entity_id_str(
    "slack",                       // provider
    Scope::Upstream(team_id),      // what it is unique within
    "message",                     // entity kind, in the upstream's vocabulary
    &format!("{channel_id}\u{1f}{ts}"),  // the upstream's own key
);
```

One root namespace, one function, four components joined with `\x1f`.
Nothing else mints an id.

### Picking a scope

`Scope` is the decision that used to be implicit in each provider's
recipe string, and it is the one that determines whether two configured
sources can collide.

| Variant | When | Examples |
|---|---|---|
| `Upstream(id)` | Default. The natural key is unique within one upstream account / workspace / org, and the scope value is **always present**. | Slack `team_id`, JMAP `account_id`, Signal account identifier |
| `ProviderGlobal` | The upstream genuinely guarantees the natural key is unique provider-wide. | GitHub `{repo}:pr:{n}`, Notion `page_id`, WhatsApp `chat_jid`, Anthropic and ChatGPT conversation/message uuids |
| `Content` | Identity **is** the bytes, and two sources finding the same file *should* collapse to one row. | pdf `blake3`, perseus canonical work id |

"Probably unique" is `Upstream` with the account id. It costs nothing
extra — *provided the account id is always there*.

**A scope component has to be present-or-never.** This is the one way
`Upstream` can be wrong, and it is not obvious: if the value is
`Option` and merely *usually* set, then the first ingest that finds it
populated re-keys every row minted while it was empty. That is precisely
the silent re-keying this crate exists to prevent, arrived at from the
inside.

Anthropic is the worked example, and the reason it appears under
`ProviderGlobal` above rather than here. Scoping its ids on `org_uuid`
looks like the textbook `Upstream` case — right until you notice the
column is empty whenever orgs aren't mirrored (`sync.projects = false`,
or an older ingest) and populated afterwards. It issues service-wide
unique uuids anyway, so `ProviderGlobal` is both correct and safe. See
`anthropic/src/render/ids.rs` §Scope for the full argument.

So the test is not "is this key unique within the account?" but "will
this scope value be identical on every future ingest, including the ones
configured differently?" If it can appear later, it is not a scope.

`Content` deliberately makes two overlapping sources contend for one id.
That is the intended behaviour; `IdClaims` turns the contention into an
error naming both sources rather than letting one silently erase the
other (see [Guardrails](#guardrails)).

### `SourceInstance` is the last resort, not the default

Four providers — signal, whatsapp, yolink, contacts — key on our
config's source name.

An earlier draft of this file said that was flatly unsafe, because one
editable string served as both the display name and the identity, so a
rename would silently re-key every row a source ever produced. **That
was wrong, and it was wrong before #201 too.** `SourceWizard.vue` has
carried `:disabled="isEdit"` on the name field since it shipped, with a
comment saying renaming is a migration rather than a form field. There
was never a rename button to press.

What #201 changed is that the split is now *explicit and enforced in
the config format* rather than a property of one Vue component:

| | |
|---|---|
| `id` | identity. Path-safe, unique, forms the directory structure. Changing it is a migration; the wizard makes it read-only on edit. |
| `name` | what a person types and every screen shows. Free text, meaningless to every program, freely changed. |

A renderer receives `source_name(tree)` — the first segment of the
step's `id` — so only the stable half ever reaches an id.

So the real argument against scoping on it was never the rename hazard.
It is that such an id is a function of *configuration* rather than of
data: two roots ingesting the same upstream content under different step
ids get different uuids, which costs the reproducibility the fixture
suite and every insta golden rest on.

Prefer `Upstream` wherever the provider gives you anything to key on;
reach for `SourceInstance` when it genuinely gives you nothing, as with
yolink's per-source timeseries page — a document datalib composes, with
no YoLink-side object behind it.

**The source *type* was never the missing piece.** It is already the
first recipe component: `provider` is a hardcoded `&'static str` per
provider (`"slack"`, `"openai"`, `"jmap"`), never a config string. What
the type cannot supply is instance-level discrimination:

- signal's `chat_id` is an autoincrement local to one backup file, so
  two accounts both have chat `1`;
- yolink's `device` is a user-typed label like `"fridge"`;
- contacts' vCard `UID` is unique per addressbook, not globally.

Two configured accounts of any of those types would collide on every
row. They need either a stable **upstream** identity (`Scope::Upstream`)
or, where no upstream object exists, `Scope::SourceInstance` — which at
least keys on the stable `id` rather than a display name. Corroborating
evidence that upstream was always the intent: contacts'
`contact_uuid(account_id, …)` is *called* with the source name — the
parameter has been named for the right thing all along.

### Why not opaque random ids

A v4 per row makes collisions impossible and is the obvious answer. It
costs the property this codebase is actually built on: ids as a pure
function of upstream data.

- Re-ingest stops being idempotent — every render needs a backpointer
  lookup to find the id it minted last time.
- A fresh data root re-ingesting the same upstream data produces
  *different* ids, so `//tests/fixtures:ingested_tng_test` (which
  asserts byte-identical convergence across three runs) and every insta
  golden would have to stop asserting on ids.

Determinism is the property to keep. Uniqueness is the property to fix.

## The backpointer

`uuid` is a one-way hash, so the row also carries what it was minted
from:

| Column | Holds |
|---|---|
| `upstream_id` | The upstream's own id, within the scope |
| `upstream_entity_kind` | The `entity_kind` component — the upstream's vocabulary |
| `upstream_scope` | The `Scope::Upstream` / `SourceInstance` value; NULL for `ProviderGlobal` / `Content` |

Together with `provider` (its own column) that is the entire recipe, so
once a provider is ported `entity_id(provider, scope,
upstream_entity_kind, upstream_id) == uuid` holds by construction.

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

Three checks stand between a bad recipe and silent data loss.

1. **`IdClaims`** ([`datalib_etl::grid_index`](../../datalib/backend/etl/src/grid_index.rs))
   fails an index run when two sources claim one `markdown_uuid` or one
   `grid_rows.uuid`, naming both. Scoped to a single run on purpose:
   the same ids arriving under a new `source_name` is a *rename*, which
   is legitimate, whereas two sidecars claiming one id inside one walk
   is always a misconfiguration or a recipe missing a discriminator.
2. **`//tests/fixtures:ingested_tng_test`** asserts `grid_rows.uuid` is
   unique, that no `markdown_uuid` is claimed by two `source_name`s, and
   that the set of providers minting non-UUID primary keys equals
   `NON_UUID_PK_PROVIDERS` exactly — in both directions, so the
   allowlist cannot rot into a permanent exemption.
3. **`//datalib/ui:e2e_test`** (`grid-copy-ids.spec.ts`) pins that the
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
- **anthropic `thinking` blocks** (and the fallback for a tool block
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
Runs 2 and 3 cannot do this: both skip on `source_fingerprint`, so no
id in them is recomputed. Verified by sabotage — a wall-clock salt in
an id recipe passes runs 1–3 and fails run 4.

It catches a recipe that reads the clock, an RNG, or an unfixed
iteration order, and a renderer that reads ids back off the index
rather than deriving them.

It does not catch an id derived from `config.toml`, because the driver
regenerates the same step ids every run. The real test is a run over
the same fixture under *different* step ids, which the driver cannot do
today — its source names are a hardcoded dict and three raw stores are
seeded at paths built from them. Standing in for it:
`SCOPE_TAG_BY_PROVIDER` forces a ported provider whose ids depend on
configuration to declare the `src` scope and store the value in
`upstream_scope`, or the round-trip check fails it. That makes a
config-scoped id **declared** rather than merely detected, which is
weaker but not nothing.

## Porting status

Three of sixteen row-emitting providers mint through `entity_id`. The
`NON_UUID_PK_PROVIDERS` allowlist is empty and every row in the TNG
fixture is UUID-shaped.

| Provider | Status | Scope |
|---|---|---|
| anthropic | ported | `ProviderGlobal` |
| openai (chatgpt) | ported | `ProviderGlobal` |
| slack | ported | `Upstream(team_id)` |
| github, gitlab | pending | `Upstream(repo)` — recipe already carries it |
| email (jmap) | pending | `Upstream(account_id)` — already carries it |
| beeper | pending | `Upstream(store)` — already carries it |
| notion | pending | `ProviderGlobal` — page ids are Notion UUIDs |
| pdf, perseus | pending | `Content` |
| linkedin, google_takeout, sms_backup_restore | pending | `ProviderGlobal` |
| whatsapp | pending | `Upstream(account_jid)` — needs parse plumbing |
| signal | pending | `ProviderGlobal` on recipient identifiers |
| yolink | pending | `Upstream(device_udid)` for devices, `SourceInstance` for the page |
| contacts | pending | `SourceInstance`, until a CardDAV principal is extracted |

The pending ones are mechanical: their recipes already carry the right
discriminator, so the port is swapping the namespace and separator and
populating the backpointer.

### The three that were blocked

All four source-name-keyed providers wanted an upstream identity.
Three now have a route, and none is blocked on a decision any more:

- **whatsapp** — `wa_chat.account_jid` is already in the raw store,
  just not surfaced by `parse`. `Upstream(account_jid)`.
- **signal** — the blocker was that `chat_id` and `author_id` are
  autoincrements local to one backup file. `ParsedRecipient.identifier`
  is the e164 or ACI, and both chats and messages resolve to a
  recipient, so keying on identifiers instead of row ids makes the ids
  content-derived and backup-independent. (`AccountData` carries
  `profileKey`, `username` and names but no ACI, so the *account*
  cannot be identified — which is why keying on the peer rather than
  the owner is the move.)
- **yolink** — `device_udid` is "the per-device UUID returned by the
  YoLink open API", so device rows are `Upstream`. The per-source
  timeseries page has no upstream object at all and is the honest case
  for `SourceInstance`.
- **contacts** — the weakest. A vCard `UID` is unique per addressbook
  rather than globally, and the config's `server_url` is ours.
  `SourceInstance` until someone extracts the CardDAV principal.

One consequence worth settling before signal lands: content-derived ids
mean two backups of one account deliberately dedupe, and `IdClaims`
currently treats two sources claiming an id as a hard error. That is a
contradiction this file introduced — `Scope::Content` is documented as
"two sources finding the same thing collapse" while the check fails the
run. Dedup-intending scopes need an exemption, or the check needs to
compare row content rather than just the id.

### When porting a provider

1. Add an `ids` module returning an `Identity { uuid, natural_key,
   entity_kind }`. Returning the pair is what keeps `upstream_id`
   and `uuid` from drifting — build the key once and use it twice.
   Use `datalib_id::composite_key` for tuple keys.
2. Populate all three backpointer columns. For chat-common providers
   that means `NormalizedChat::upstream_scope`,
   `RenderProfile::chat_entity_kind`, and `source_ref` on every item
   **and every reaction** (reactions get their own grid_rows and are
   easy to miss — that was a real bug).
3. Add the provider to `SCOPE_TAG_BY_PROVIDER` and `PORTED_PROVIDERS`
   in `ingested_tng_test`.
4. Bump the provider's `RENDER_VERSION`. A re-key moves `chat_uuid`,
   which *names the output directory*, so the new documents land beside
   the old ones rather than over them and the index loads both. The
   render step handles that — a tree whose sidecars carry a version this
   build doesn't produce is deleted and re-rendered from the raw store.
   Skip the bump and the port silently does nothing to any data root
   that already exists: the fingerprints still match, so nothing
   re-renders and the old ids stay.

   Every render processor already returns its constant from
   `DataProcessor::render_version`, and the render step fails a source
   that writes sidecars without declaring one — so a *new* provider
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
  three from the sidecars when they disagree. Before that, a root
  predating the `external_id` → `upstream_id` rename answered every
  read *and* every write with `no such column: upstream_id`.
- **The rendered tree.** The render step discards a tree stamped with a
  foreign `render_version`, cursor included, and re-renders from the
  raw store.

Both are derived data, so the cost is a re-render plus a re-index. No
re-download.

The round-trip check is not a formality. It caught three real bugs
across the first three ports, each invisible to every other test: a
composite key spelled `#` in the column and `\x1f` in the recipe; a
Claude Project stamped `"conversation"` while minted as `"project"`;
and slack's reaction rows carrying no backpointer at all.
