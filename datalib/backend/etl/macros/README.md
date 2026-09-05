# `datalib-etl-macros` — the derive reference

Four derives, one per table shape. Each turns a row struct into the DDL, the
column metadata and the write plumbing, so a provider's `schema_raw.rs` is
the struct and its attribute and nothing else.

| derive | for |
|---|---|
| `WirePayloadRow` | a wire-payload entity table (id + payload + promoted columns) |
| `RawTable` | the general form: payload-shaped *and* payload-less raw tables |
| `CasEdgeRow` | a per-provider attachment / blob-link edge table |
| `PortableTable` | the consumer side — flat typed tables in portable SQL |

## Type mapping (`WirePayloadRow`, `RawTable`, `CasEdgeRow`)

| Rust | SQL |
|---|---|
| `String` | `TEXT NOT NULL` |
| `Option<String>` | `TEXT NULL` |
| `i64` | `INTEGER NOT NULL` |
| `Option<i64>` | `INTEGER NULL` |
| `f64` | `REAL NOT NULL` |
| `Option<f64>` | `REAL NULL` |

Any other field type is a compile error pointing at the field. Add support
here when a new shape comes up; keeping the universe narrow keeps the bind
code straightforward.

## `WirePayloadRow`

`#[wire_payload_row(table = "name")]` is required.

The struct must have **exactly one field of type `WirePayload`** —
path-tolerant, so `WirePayload`, `dr::WirePayload` and
`datalib_etl::doltlite_raw::WirePayload` all match. That field carries the
`id` and `payload` columns; every other field is a promoted column, emitted
into the `CREATE TABLE` in declaration order and bound in the same order.

```rust,ignore
#[derive(WirePayloadRow)]
#[wire_payload_row(table = "chat_items")]
pub struct ChatItemRow {
    pub id_and_payload: WirePayload,
    pub chat_id: String,
    pub author_id: String,
    pub date_sent: i64,
}
```

`ChatItemRow::ddl()` then returns the same SQL a hand-written
`wire_payload_table_ddl("chat_items", &[…])` call would have produced.

## `RawTable`

The general form of `WirePayloadRow`: it also covers payload-less tables
(N:M join tables, cursor tables) that `WirePayloadRow` cannot express. The
mode is chosen by whether the struct has a `WirePayload` field.

- **Payload mode** — one `WirePayload` field contributing `id TEXT PRIMARY
  KEY` and the JSONB `payload`; every other field is promoted.
  `PAYLOAD_COLUMN = Some("payload")`.
- **Plain mode** — no payload column. The primary key is the single column
  named by `primary_key` (default `"id"`), which must be `String` or `i64`.
  A join table fits this mode by carrying a synthesized `id` such as
  `"{email_id}#{mailbox_id}"`, so the conflict target stays one column.

Attributes on `#[raw_table(...)]`:

- `table = "name"` — required.
- `primary_key = "col"` — plain mode only; rejected in payload mode, where
  the key is always `id`.
- `index = "name:col1,col2"` — emits `CREATE INDEX IF NOT EXISTS`.
  Repeatable.

Emits `ddl()`, `index_ddls()` and `all_ddl()` (table plus indexes, ready to
splice into a provider's `full_ddl()`), plus the `BulkUpsertable` impl.

```rust,ignore
#[derive(RawTable)]
#[raw_table(table = "email_mailboxes",
            index = "email_mailboxes_by_mailbox:mailbox_id")]
pub struct EmailMailboxRow {
    pub id: String,          // synthesized "{email_id}#{mailbox_id}"
    pub email_id: String,
    pub mailbox_id: String,
}
```

## `CasEdgeRow`

`#[cas_edge_row(table = "name")]` is required. The struct must have
**exactly four named fields, in this order**:

1. `id: String` — synthesized PK, `"{owning_id}#{ref_id}"`
2. `<owning>: String` — owning-entity FK; the column name is read from this
   field's identifier
3. `<ref>: String` — upstream ref id; likewise
4. `blake3: Option<String>` — the CAS hash, NULL until the bytes are stored

The fixed shape is the universal pattern of every per-provider
attachment-edge table. Validation enforces that `id` is first and `blake3`
is last; the two middle identifiers become the emitted column names.

```rust,ignore
#[derive(CasEdgeRow)]
#[cas_edge_row(table = "slack_attachments")]
pub struct SlackAttachmentRow {
    pub id: String,
    pub message_uuid: String,
    pub file_id: String,
    pub blake3: Option<String>,
}
```

emits the table DDL, two index DDLs, the `BulkUpsertable` impl, and the
`blob_cas::CasEdgeRow` impl with `OWNING_COLUMN = "message_uuid"` and
`REF_COLUMN = "file_id"`.

## `PortableTable`

The consumer-side sibling. Where the three above derive the raw-store wire
shape, this covers flat typed tables whose columns use portable
MySQL/Dolt/SQLite types (`VARCHAR(n)`, `LONGTEXT`, `INT`). It replaced the
old `schemas/codegen.py` JSON-Schema path, making the struct the single
source of truth the same way `schema_raw.rs` already was.

- `#[portable_table(table = "grid_rows", primary_key = "uuid")]` — both keys
  required; `primary_key` accepts a comma-separated list for composite keys.
- `#[col(sql = "VARCHAR(96)")]` — required on every field. Nullability is
  inferred from the Rust type: `Option<T>` is nullable, anything else gets
  `NOT NULL`.
- `#[derived(name = "when_ts_utc", sql = "VARCHAR(40)")]` — repeatable, on
  the column it follows. Declares a column that lives in the DB but is
  computed at load time and so is absent from the struct.

Emits module-level `TABLES`, `DDL` and `COLUMNS`.

The `BulkUpsertable` impl is **skipped for a composite primary key**:
`BulkUpsertable` keys on one column by contract (`ID_COLUMN` is the
`ON CONFLICT` target and `id()` returns one `&str`), and `disk_usage` is
legitimately keyed on `(path, measured_at)`. Such a table still gets its DDL
and column metadata; it just keeps writing itself.
