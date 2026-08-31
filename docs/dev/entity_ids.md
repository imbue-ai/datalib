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
| `Upstream(id)` | Default. The natural key is unique within one upstream account / workspace / org. | Slack `team_id`, JMAP `account_id`, Anthropic `org_uuid`, Signal account identifier |
| `ProviderGlobal` | The upstream genuinely guarantees the natural key is unique provider-wide. | GitHub `{repo}:pr:{n}`, Notion `page_id`, WhatsApp `chat_jid` |
| `Content` | Identity **is** the bytes, and two sources finding the same file *should* collapse to one row. | pdf `blake3`, perseus canonical work id |

"Probably unique" is `Upstream` with the account id. It costs nothing
extra and cannot be wrong.

`Content` deliberately makes two overlapping sources contend for one id.
That is the intended behaviour; `IdClaims` turns the contention into an
error naming both sources rather than letting one silently erase the
other (see [Guardrails](#guardrails)).

### There is no `SourceName` scope

Four providers — signal, whatsapp, yolink, contacts — key on our
config's `source_name` today. That means renaming a source from the
Manage tab silently re-keys every row it ever produced and orphans every
`feedback.target_uuids` entry pointing at them. One click, unrecoverable.

**`source_type` is not the fix.** It survives renames but stops
discriminating exactly where the discrimination is load-bearing:

- signal's `chat_id` is an autoincrement local to one backup file, so
  two accounts both have chat `1`;
- yolink's `device` is a user-typed label like `"fridge"`;
- contacts' vCard `UID` is unique per addressbook, not globally.

Two configured accounts of any of those types would collide on every
row. What they need is a stable **upstream** identity, which is what
`Scope::Upstream` asks for. Corroborating evidence that this was always
the intent: contacts' `contact_uuid(account_id, …)` is *called* with
`source_name` — the parameter has been named for the right thing all
along.

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
| `source_native_id` | The upstream's own id, within the scope |
| `source_entity_kind` | The `entity_kind` component — the upstream's vocabulary |
| `source_scope` | The `Scope::Upstream` value; NULL for `ProviderGlobal` / `Content` |

Together with `provider` (its own column) that is the entire recipe, so
once a provider is ported `entity_id(provider, scope,
source_entity_kind, source_native_id) == uuid` holds by construction.

`source_entity_kind` is **not** `grid_rows.kind`. `kind` is a display
label for the grid's Kind column ("LLM Thinking", "GitHub PR") and may
be reworded freely; this one may not, because the id depends on it. It
is also what makes the backpointer usable: GitHub numbers issue
comments, reviews and review comments in three independent sequences
that overlap, and each is fetched from a different API path, so a bare
`12345` is ambiguous without it.

**Set `source_native_id` even when it currently equals `uuid`.** A
provider that passes an upstream id through as its primary key loses
that route the moment it moves onto `datalib_id`, and this column is
what the grid's "Copy source ID(s)" action reads.

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
| **signal, yolink, contacts** | **blocked** | see below |

The pending ones are mechanical: their recipes already carry the right
discriminator, so the port is swapping the namespace and separator and
populating the backpointer.

### The blocked three

All four `source_name`-keyed providers need an upstream identity to
replace it with. Only whatsapp has one available today
(`wa_chat.account_jid`, already in the raw store, just not surfaced by
`parse`). The other three do not:

- **signal** — the backup's `AccountData` frame carries `profileKey`,
  `username`, `givenName`/`familyName`, but no ACI or e164. The chat
  ids it keys on are autoincrements local to one backup file, so
  something must discriminate two accounts. Candidates: a hash of
  `profileKey` (stable, but rotates on re-registration, and it is a
  secret we would be hashing into public ids), or extracting the self
  recipient's ACI (more parse work, may not be present).
- **yolink** — device rows can key on the config's `family_device_id`,
  which is YoLink-issued. The per-source *timeseries page* has no
  upstream entity at all: it is a document datalib invents for a
  configured source, so its identity genuinely is the source.
- **contacts** — `contact_uuid(account_id, …)` is called with
  `source_name`. The CardDAV config has `server_url`, which is stable
  against renames but is still our config rather than the server's own
  principal id.

These need a decision, not more code. The options are on the table:
mint and persist a per-source instance id (breaks id reproducibility
from data alone, which the fixture and insta goldens rely on), extract
a real upstream id per provider (most work, best result), or add an
explicit scope variant for "a document datalib invents per configured
source" that names the rename hazard instead of hiding it.

### When porting a provider

1. Add an `ids` module returning an `Identity { uuid, natural_key,
   entity_kind }`. Returning the pair is what keeps `source_native_id`
   and `uuid` from drifting — build the key once and use it twice.
   Use `datalib_id::composite_key` for tuple keys.
2. Populate all three backpointer columns. For chat-common providers
   that means `NormalizedChat::source_scope`,
   `RenderProfile::chat_entity_kind`, and `source_ref` on every item
   **and every reaction** (reactions get their own grid_rows and are
   easy to miss — that was a real bug).
3. Add the provider to `SCOPE_TAG_BY_PROVIDER` and `PORTED_PROVIDERS`
   in `ingested_tng_test`.
4. Re-key: existing data roots must be wiped and re-ingested. Filed
   feedback pointing at old ids does not survive.

The round-trip check is not a formality. It caught three real bugs
across the first three ports, each invisible to every other test: a
composite key spelled `#` in the column and `\x1f` in the recipe; a
Claude Project stamped `"conversation"` while minted as `"project"`;
and slack's reaction rows carrying no backpointer at all.
