# Design: the "Add a source" wizard

**Status: mostly built; the rest is listed under [Still open](#still-open).**
This is the wizard's design record: what shipped, and what was proposed
and is not built. The original design was written against a config
shape that no longer exists (ungrouped steps with a `command` and
`outputs`); everything below describes the tree as it is, and the open
items are the only proposals kept. The design of the entity it writes —
a source is a `[[groups]]` entry plus its `ingest` and `render_markdown`
steps — is [`config_model.md`](../config_model.md), and
the wizard's file and folder fields are
[`wizard_file_pickers.md`](../wizard_file_pickers.md).

## What shipped

**The job.** Take a person from "I want my Slack in here" to a saved,
credential-verified source without a text editor, and check everything
that *can* be checked before the first run — credentials, channel
names, paths — in the form rather than in a failed sync. The rule the
whole thing rests on: **`config.toml` stays the single source of truth.**
The wizard edits that text; it keeps no state of its own.

**Where it lives.** One dialog,
[`SourceWizard.vue`](../../../datalib/ui/src/components/SourceWizard.vue),
opened from the Manage screen's sources table
(`ui/src/cards/SourcesCard.ce.vue`). It writes and edits a source as
one unit: the group (its `name` and `description` edited in place) and
both its steps, with the render step's fields under a "Rendering"
heading of the same form. The picker is a grid of tiles grouped by
`kind` (`api` / `export` / `local`) with a filter box over label, type
and keywords. Every source type has a tile; one whose descriptor says
`wizard: false` is shown disabled, and Edit on such a source points at
the config editor under **Advanced** instead.

**Descriptors.** The catalog is a TypeScript table,
[`ui/src/config/catalog.ts`](../../../datalib/ui/src/config/catalog.ts),
one `CatalogEntry` per source (or per variant of one). Beyond the
picker fields (`label`, `blurb`, `keywords`, `kind`, `icon`,
`defaultName`, `nameHint`) an entry carries:

- `fields`, each with a `target` — a dotted path into the step's
  params — and a `phase` (`download` or `render`) saying which step it
  lands on. Field kinds are a closed set: `text`, `path`, `select`,
  `date`, `bool`, `int`, `bytes`, `string_list`. A `text` field marked
  `latchkey: true` renders as the latchkey-account control; a
  `string_list` marked `probe: <noun>` grows a checklist from the
  probe, the nouns being `labels`, `mailboxes`, `conversations` and
  `channels` (`ProbeItemPicker.vue` picks its columns from the kind
  of item the list holds).
- `preset` values the entry writes without asking, and `method`, the
  params table that selects the ingest method when no field names it.
- `credentialService` (the latchkey service name),
  `credentialRegister` (how to register that service with latchkey if
  it has never heard of it — Claude's is a cookie capture of
  `sessionKey`, ChatGPT's a token capture) and
  `credentialConnectWarning`.
- `canProbe`, which offers "Test connection".
- `variantKey`: Gmail and Fastmail are two entries over the one
  `email` type, told apart by which params table is present, so `type`
  is not a unique key. `entryKey` is, and `catalogForStep` picks the
  variant from an existing step's params.

**Writing the config.** All client-side, in
[`ui/src/config/sourceSteps.ts`](../../../datalib/ui/src/config/sourceSteps.ts):
`buildSource` produces the TOML for a group and its two steps,
`appendSource` / `replaceSteps` / `removeSteps` splice it into the
text, `wireIntoFanIns` / `unwireFromFanIns` keep the index steps'
`inputs` right, and the result is saved through the existing
`PUT /api/config`, which runs the real loader. There is no
`toml_edit` and no backend draft endpoint. Two rules make editing
trustworthy: `paramsAreRepresentable` gates the Edit button, so a
source carrying something the descriptor cannot show is sent to the
config editor rather than to a form that would drop it; and the
wizard writes back only the fields it models.

**Names and ids.** The id is derived from the name once (`slugify`,
then `suggestId`, which suffixes `-2`, `-3` past anything taken or
reserved); typing into the Id box stops the derivation; on edit the id
is read-only. Uniqueness is the loader's job, not the UI's:
`dag/src/config.rs` rejects a duplicate group or step id and anything
under the reserved `system` directory.

**Credentials and the probe.** Three routes in
`datalib/backend/http/src/connect.rs`, all shelling out — latchkey to
`latchkey` directly (it is not provider code), the probe to
`datalib-step`:

```
GET  /api/latchkey/{service}               stored accounts, authOptions, gateway
POST /api/latchkey/{service}/connect       start `latchkey auth browser` → {id}
GET  /api/latchkey/connect/{id}/status     poll it → running | ok | failed
POST /api/probe   {type, params}           → the provider's probe report
```

`connect` registers the service from `credentialRegister` first if
latchkey lacks it, runs `ensure-browser` restricted to a browser
already on the machine (the Chromium download stays something a person
chooses by running the command), and seeds a placeholder with
`auth set` when the person named an account latchkey has not seen,
since `auth browser` only refreshes. The login is polled, not
streamed. Under a latchkey gateway the button is not offered — the
note says where to sign in — and "Test connection" still works, since
the probe goes through `latchkey curl`, which the gateway serves. A
service latchkey holds without a browser login is not converted behind
the person's back: the dialog shows the `auth clear` / `deregister` /
`register` commands and leaves running them to the owner.

`datalib-step probe <type> --params <json>` takes the ingest params
and answers one question — what account is this and what can it reach.
There is no `--op`; one report serves as the auth check and the list.
Probes exist for **email, Claude, ChatGPT and Slack**
(`datalib_step/src/probe.rs`); any other type is refused with a
message saying so. Which account a sync runs as is
`latchkey_settings.account`, passed to latchkey as `--account`
(`etl/src/latchkey.rs`), so two workspaces on one service work.

**Delete** removes the entries from the config and nothing else: the
confirm names the ingest step and the render step that has to go with
it, the data stays on disk, and re-adding resumes from it.

**The Manage screen itself** — the table, its status column, the log
behind it — is designed in
[`completed/data_centric_ui.md`](completed/data_centric_ui.md) and
[`completed/logs_and_metrics.md`](completed/logs_and_metrics.md), not
here.

## Still open

Proposed, checked against the tree on 2026-09-17, and not built:

- **A served catalog.** The design wanted each descriptor beside its
  provider's config struct, aggregated and served at
  `GET /api/sources/catalog`. The table is a TS file, and nothing
  asserts it names every `SourceType` — `ingest_methods.test.ts`
  checks the other direction, that every catalog type has a declared
  ingest method.
- **Forms for the rest.** These entries are `wizard: false`: `github`,
  `gitlab`, `notion`, email over mbox or a non-Fastmail JMAP server,
  `contacts`, `yolink`, `google_takeout`, `linkedin`,
  `sms_backup_restore`, `beeper`, `perseus`. Probes for every provider
  beyond the four above.
- **`GET /api/fs/browse`.** In the desktop app a path field opens a
  native dialog; in a plain browser it is a text box, and a
  server-side browse endpoint is the only fix
  ([`wizard_file_pickers.md`](../wizard_file_pickers.md)).
- **A token field.** For a `set`-only service the wizard shows
  commands. The design had a secret field whose value the backend
  pipes to `latchkey auth set` on stdin — never argv, which `ps` can
  read.
- **A tree picker.** `ProbeItemPicker` is a flat checklist. Email
  labels and Notion pages are trees, and label matching is exact, so
  three hundred labels in a flat list is a poor picker.
- **A guard on `credentialService`.** latchkey resolves a request by
  URL, so the name in a descriptor is used only for the credential UI
  and nothing at request time checks it. A wrong name would show the
  wrong status and connect an account the sync never uses. A test that
  the named service's `baseApiUrls` match the provider's base URL would
  catch it.
- **Document counts on the Manage screen.** If a per-source count is
  ever wanted there, it must come through the `unified_index` applet:
  `runtime/src/layout.rs` says `datalib-http` reads nothing under
  `unified_index/`.
- **Renaming an id.** The id is fixed on edit because a rename is a
  migration — move the directory, remap `system/dag_state.json`, and
  rewrite `markdowns.md_path`, `markdowns.source_id` and
  `grid_rows.qmd_path`, which `grid_index` would otherwise never touch
  since the render store's diff names no row. It wants to be one
  operation and is cheaper than it looks: rendered markdown is
  position-independent, `markdown_uuid` is upstream-derived so filed
  feedback survives, and qmd keys vectors by content hash.
- **Removing a source's data.** Delete edits the config only. Removing
  the tree is a separate feature nobody has designed.
