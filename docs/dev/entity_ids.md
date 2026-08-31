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

`datalib_id` exists and the backpointer columns are populated where the
information was already at hand. **No provider mints its ids through
`entity_id` yet** — that is a breaking re-key and is deliberately
separate.

| | Providers |
|---|---|
| Ported onto `datalib_id` | *(none yet)* |
| Non-UUID primary keys | anthropic, openai — tracked by `NON_UUID_PK_PROVIDERS` |
| Keyed on our `source_name` | signal, whatsapp, yolink, contacts |
| `source_native_id` populated | anthropic, chatgpt, slack (chat-level), pdf, github, gitlab, yolink, plus everything through chat-common / contact-common |
| `source_native_id` still NULL | every message-level row — per-item ids need a field on `NormalizedChatItem` and a pass through each provider's item builder |

When porting a provider:

1. Move its recipes onto `entity_id`, deleting its `*_UUID_NS` constant.
2. Populate all three backpointer columns.
3. Shrink `NON_UUID_PK_PROVIDERS` if it was listed.
4. Add the round-trip assertion (`entity_id(...) == uuid`) to
   `ingested_tng_test`, scoped to the providers that have moved —
   nothing asserts it today because it would be vacuous.
5. Re-key: existing data roots must be wiped and re-ingested. Filed
   feedback pointing at old ids does not survive.
