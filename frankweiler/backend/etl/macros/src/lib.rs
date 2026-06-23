//! Proc-macros for the frankweiler ETL crates.
//!
//! Derives today:
//!
//!   - [`WirePayloadRow`] — DDL + bulk-upsert plumbing for any row
//!     struct that maps to a wire-payload entity table (id + payload
//!     + promoted columns).
//!   - [`RawTable`] — the general form of `WirePayloadRow`, covering
//!     both payload-shaped and payload-less raw-store tables.
//!   - [`CasEdgeRow`] — every per-provider CAS edge table (each
//!     attachment / blob-link table) follows the same four-column
//!     shape; this derive emits the
//!     [`frankweiler_etl::blob_cas::CasEdgeRow`] +
//!     [`frankweiler_etl::bulk::BulkUpsertable`] impls so the
//!     provider's `schema_raw.rs` is just the struct.
//!   - [`PortableTable`] — the consumer-side counterpart: emits the
//!     portable `CREATE TABLE` DDL + `TABLES`/`COLUMNS` consts for the
//!     denormalized presentation tables (`grid_rows`, `edges`,
//!     `markdowns`, `feedback`, `sync_jobs`) that back the grid / UI.
//!     Replaces the old `schemas/codegen.py` JSON-Schema path.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    parse_macro_input, Attribute, Data, DataStruct, DeriveInput, Field, Fields, GenericArgument,
    Ident, LitStr, PathArguments, Type, TypePath,
};

/// Derive `frankweiler_etl::doltlite_raw::WirePayloadRow` and
/// `frankweiler_etl::bulk::BulkUpsertable` for a row struct that maps
/// to a wire-payload entity table.
///
/// **Required shape.** The struct must have **exactly one field of
/// type `WirePayload`** (path-tolerant — `WirePayload`,
/// `dr::WirePayload`, or `frankweiler_etl::doltlite_raw::WirePayload`
/// all match). That field carries the `id` and `payload` columns.
/// Every *other* field is a promoted column, emitted into the CREATE
/// TABLE in declaration order and bound in the same order.
///
/// **Attribute.** `#[wire_payload_row(table = "name")]` names the
/// SQL table. Required.
///
/// **Type mapping** (Rust → SQL):
/// - `String` → `TEXT NOT NULL`
/// - `Option<String>` → `TEXT NULL`
/// - `i64` → `INTEGER NOT NULL`
/// - `Option<i64>` → `INTEGER NULL`
/// - `f64` → `REAL NOT NULL`
/// - `Option<f64>` → `REAL NULL`
///
/// Any other field type is a compile error pointing at the field.
/// Add support here when a new shape comes up — keeping the universe
/// narrow keeps the bind code straightforward.
///
/// **Example.**
/// ```ignore
/// use frankweiler_etl::doltlite_raw::WirePayload;
/// use frankweiler_etl_macros::WirePayloadRow;
///
/// #[derive(WirePayloadRow)]
/// #[wire_payload_row(table = "chat_items")]
/// pub struct ChatItemRow {
///     pub id_and_payload: WirePayload,
///     pub chat_id: String,
///     pub author_id: String,
///     pub date_sent: i64,
/// }
/// ```
///
/// The derive emits — among other things — `ChatItemRow::ddl()`
/// returning the same SQL the hand-written
/// `dr::wire_payload_table_ddl("chat_items", &[…])` call would have
/// produced.
#[proc_macro_derive(WirePayloadRow, attributes(wire_payload_row))]
pub fn derive_wire_payload_row(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand(input: DeriveInput) -> syn::Result<TokenStream2> {
    let struct_name = input.ident.clone();
    let table = parse_table_attr(&input.attrs, &struct_name)?;
    let fields = collect_named_fields(&input)?;
    let (id_and_payload_ident, promoted) = split_id_and_payload(&fields, &struct_name)?;

    // DDL: build the promoted-columns slice the existing
    // `wire_payload_table_ddl` helper expects. Aligning column names
    // to a uniform width matches the visual style of the hand-written
    // DDLs the macro is replacing. Shared with `RawTable` payload mode.
    let ddl_literals = promoted_decl_literals(&promoted);
    let table_lit = LitStr::new(&table, proc_macro2::Span::call_site());

    // BulkUpsertable typed columns: promoted columns only.
    // (`payload` goes in PAYLOAD_COLUMN; `id` is the PK and bound
    // separately.)
    let typed_col_names: Vec<LitStr> = promoted
        .iter()
        .map(|p| LitStr::new(&p.name.to_string(), proc_macro2::Span::call_site()))
        .collect();

    // bind_into binds in INSERT column order: id_and_payload.id, then
    // each promoted column, then id_and_payload.payload.
    let promoted_binds: Vec<TokenStream2> = promoted.iter().map(|p| p.bind_expr()).collect();

    Ok(quote! {
        impl ::frankweiler_etl::doltlite_raw::WirePayloadRow for #struct_name {
            fn ddl() -> ::std::string::String {
                ::frankweiler_etl::doltlite_raw::wire_payload_table_ddl(
                    #table_lit,
                    &[#(#ddl_literals),*],
                )
            }
        }

        impl ::frankweiler_etl::bulk::BulkUpsertable for #struct_name {
            const TABLE: &'static str = #table_lit;
            const TYPED_COLUMNS: &'static [&'static str] = &[#(#typed_col_names),*];

            fn id(&self) -> &str {
                &self.#id_and_payload_ident.id
            }

            fn bind_into<'q>(
                &'q self,
                q: ::sqlx::query::Query<
                    'q,
                    ::sqlx::Sqlite,
                    ::sqlx::sqlite::SqliteArguments<'q>,
                >,
            ) -> ::sqlx::query::Query<
                'q,
                ::sqlx::Sqlite,
                ::sqlx::sqlite::SqliteArguments<'q>,
            > {
                q.bind(&self.#id_and_payload_ident.id)
                    #(.bind(#promoted_binds))*
                    .bind(&self.#id_and_payload_ident.payload)
            }
        }
    })
}

// ─────────────────────────────────────────────────────────────────────
// Attribute parsing
// ─────────────────────────────────────────────────────────────────────

fn parse_table_attr(attrs: &[Attribute], struct_name: &Ident) -> syn::Result<String> {
    for attr in attrs {
        if !attr.path().is_ident("wire_payload_row") {
            continue;
        }
        let mut table: Option<String> = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("table") {
                let lit: LitStr = meta.value()?.parse()?;
                table = Some(lit.value());
                Ok(())
            } else {
                Err(meta.error("unknown #[wire_payload_row(...)] key; supported keys: `table`"))
            }
        })?;
        return table.ok_or_else(|| {
            syn::Error::new_spanned(attr, "#[wire_payload_row(table = \"...\")] is required")
        });
    }
    Err(syn::Error::new_spanned(
        struct_name,
        "#[derive(WirePayloadRow)] requires #[wire_payload_row(table = \"…\")]",
    ))
}

// ─────────────────────────────────────────────────────────────────────
// Field walking
// ─────────────────────────────────────────────────────────────────────

fn collect_named_fields(input: &DeriveInput) -> syn::Result<Vec<&Field>> {
    let Data::Struct(DataStruct { fields, .. }) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "WirePayloadRow can only be derived on structs",
        ));
    };
    let Fields::Named(named) = fields else {
        return Err(syn::Error::new_spanned(
            fields,
            "WirePayloadRow requires a struct with named fields",
        ));
    };
    Ok(named.named.iter().collect())
}

/// Identify the single `WirePayload` field (by trailing
/// path segment, so it doesn't matter whether the user wrote
/// `WirePayload`, `dr::WirePayload`, or the full path).
/// Every other field becomes a promoted column.
fn split_id_and_payload<'a>(
    fields: &[&'a Field],
    struct_name: &Ident,
) -> syn::Result<(&'a Ident, Vec<PromotedField<'a>>)> {
    let mut id_and_payload: Option<&Ident> = None;
    let mut promoted: Vec<PromotedField<'a>> = Vec::new();
    for f in fields {
        let ident = f.ident.as_ref().expect("named field");
        if is_wire_payload(&f.ty) {
            if id_and_payload.is_some() {
                return Err(syn::Error::new_spanned(
                    f,
                    "duplicate WirePayload field; exactly one is allowed",
                ));
            }
            id_and_payload = Some(ident);
        } else {
            promoted.push(PromotedField::from_field(f)?);
        }
    }
    id_and_payload.map(|t| (t, promoted)).ok_or_else(|| {
        syn::Error::new_spanned(
            struct_name,
            "no WirePayload field; add one named field of type WirePayload",
        )
    })
}

fn is_wire_payload(ty: &Type) -> bool {
    let Type::Path(TypePath { path, .. }) = ty else {
        return false;
    };
    path.segments
        .last()
        .map(|seg| seg.ident == "WirePayload")
        .unwrap_or(false)
}

// ─────────────────────────────────────────────────────────────────────
// Promoted-column type mapping
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum PromotedKind {
    TextNotNull,
    TextNullable,
    IntegerNotNull,
    IntegerNullable,
    RealNotNull,
    RealNullable,
}

impl PromotedKind {
    fn sql_type(self) -> &'static str {
        match self {
            PromotedKind::TextNotNull => "TEXT NOT NULL",
            PromotedKind::TextNullable => "TEXT NULL",
            PromotedKind::IntegerNotNull => "INTEGER NOT NULL",
            PromotedKind::IntegerNullable => "INTEGER NULL",
            PromotedKind::RealNotNull => "REAL NOT NULL",
            PromotedKind::RealNullable => "REAL NULL",
        }
    }
}

struct PromotedField<'a> {
    name: &'a Ident,
    kind: PromotedKind,
    sql_type: &'static str,
}

impl<'a> PromotedField<'a> {
    fn from_field(f: &'a Field) -> syn::Result<Self> {
        let kind = classify(&f.ty).ok_or_else(|| {
            syn::Error::new_spanned(
                &f.ty,
                "unsupported field type for #[derive(WirePayloadRow)]; \
                 supported types: String, Option<String>, i64, Option<i64>, \
                 f64, Option<f64>",
            )
        })?;
        Ok(Self {
            name: f.ident.as_ref().expect("named field"),
            kind,
            sql_type: kind.sql_type(),
        })
    }

    /// Emit the right `.bind(...)` argument expression for this field.
    /// sqlx wants `&String` for non-null TEXT, `Option<&str>` for
    /// nullable TEXT (use `.as_deref()`), and bare `i64` /
    /// `Option<i64>` by value for INTEGER columns, and the same
    /// by-value treatment for `f64` / `Option<f64>` REAL columns.
    fn bind_expr(&self) -> TokenStream2 {
        let name = self.name;
        match self.kind {
            PromotedKind::TextNotNull => quote! { &self.#name },
            PromotedKind::TextNullable => quote! { self.#name.as_deref() },
            PromotedKind::IntegerNotNull
            | PromotedKind::IntegerNullable
            | PromotedKind::RealNotNull
            | PromotedKind::RealNullable => quote! { self.#name },
        }
    }
}

fn classify(ty: &Type) -> Option<PromotedKind> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return None;
    };
    let seg = path.segments.last()?;
    match seg.ident.to_string().as_str() {
        "String" => Some(PromotedKind::TextNotNull),
        "i64" => Some(PromotedKind::IntegerNotNull),
        "f64" => Some(PromotedKind::RealNotNull),
        "Option" => {
            let PathArguments::AngleBracketed(args) = &seg.arguments else {
                return None;
            };
            let GenericArgument::Type(inner) = args.args.first()? else {
                return None;
            };
            let Type::Path(TypePath { path, .. }) = inner else {
                return None;
            };
            let inner_seg = path.segments.last()?;
            match inner_seg.ident.to_string().as_str() {
                "String" => Some(PromotedKind::TextNullable),
                "i64" => Some(PromotedKind::IntegerNullable),
                "f64" => Some(PromotedKind::RealNullable),
                _ => None,
            }
        }
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────────────
// RawTable
// ─────────────────────────────────────────────────────────────────────

/// Derive a raw-store table's **entire** SQL surface from a Rust
/// struct: the `CREATE TABLE` DDL, any `CREATE INDEX` DDLs, and the
/// [`frankweiler_etl::bulk::BulkUpsertable`] impl that the generic
/// bulk-upsert helper writes through. The goal is that a provider's
/// `schema_raw.rs` contains *no* hand-written `CREATE TABLE` strings
/// and *no* hand-written `BulkUpsertable` impls — just structs.
///
/// This is the general form of [`WirePayloadRow`]: it covers both
/// payload-shaped entity tables *and* payload-less tables (N:M join
/// tables, cursor tables) that `WirePayloadRow` can't express.
///
/// **Two modes**, chosen by whether the struct has a `WirePayload`
/// field:
///
/// 1. **Payload mode** — exactly one field of type `WirePayload`
///    (path-tolerant, same as [`WirePayloadRow`]). It contributes the
///    `id TEXT PRIMARY KEY` and `payload` (JSONB) columns; every other
///    field is a promoted column. `PAYLOAD_COLUMN = Some("payload")`.
///
/// 2. **Plain mode** — no `WirePayload` field, no payload column. The
///    primary key is a single column named by
///    `#[raw_table(primary_key = "col")]` (default `"id"`); that
///    field must be `String` or `i64`. Every other field is a typed
///    column. N:M join tables fit this mode by carrying a synthesized
///    single `id` (e.g. `"{email_id}#{mailbox_id}"`) so the conflict
///    target stays one column.
///
/// **Attributes** on `#[raw_table(...)]`:
/// - `table = "name"` — SQL table name. Required.
/// - `primary_key = "col"` — plain-mode PK column. Optional, default
///   `"id"`. Rejected in payload mode (the PK is always `id` there).
/// - `index = "name:col1,col2"` — emit
///   `CREATE INDEX IF NOT EXISTS name ON table(col1, col2)`. Repeatable.
///
/// **Type mapping** is identical to [`WirePayloadRow`] (`String` →
/// `TEXT NOT NULL`, `Option<String>` → `TEXT NULL`, `i64`/`Option<i64>`
/// → INTEGER, `f64`/`Option<f64>` → REAL).
///
/// The derive emits inherent `Self::ddl()`, `Self::index_ddls()`, and
/// `Self::all_ddl()` (table DDL + index DDLs, ready to splice into a
/// provider's `full_ddl()`), plus the `BulkUpsertable` impl.
///
/// **Example.**
/// ```ignore
/// #[derive(RawTable)]
/// #[raw_table(table = "email_mailboxes",
///             index = "email_mailboxes_by_mailbox:mailbox_id")]
/// pub struct EmailMailboxRow {
///     pub id: String,         // synth "{email_id}#{mailbox_id}"
///     pub email_id: String,
///     pub mailbox_id: String,
/// }
/// ```
#[proc_macro_derive(RawTable, attributes(raw_table))]
pub fn derive_raw_table(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_raw_table(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

struct IndexSpec {
    name: String,
    columns: Vec<String>,
}

fn parse_raw_table_attrs(
    attrs: &[Attribute],
    struct_name: &Ident,
) -> syn::Result<(String, Option<String>, Vec<IndexSpec>)> {
    let mut table: Option<String> = None;
    let mut primary_key: Option<String> = None;
    let mut indexes: Vec<IndexSpec> = Vec::new();
    let mut saw_attr = false;
    for attr in attrs {
        if !attr.path().is_ident("raw_table") {
            continue;
        }
        saw_attr = true;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("table") {
                table = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else if meta.path.is_ident("primary_key") {
                primary_key = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else if meta.path.is_ident("index") {
                let lit: LitStr = meta.value()?.parse()?;
                indexes.push(parse_index_spec(&lit)?);
                Ok(())
            } else {
                Err(meta.error(
                    "unknown #[raw_table(...)] key; supported keys: `table`, `primary_key`, `index`",
                ))
            }
        })?;
    }
    if !saw_attr {
        return Err(syn::Error::new_spanned(
            struct_name,
            "#[derive(RawTable)] requires #[raw_table(table = \"…\")]",
        ));
    }
    let table = table.ok_or_else(|| {
        syn::Error::new_spanned(struct_name, "#[raw_table(table = \"...\")] is required")
    })?;
    Ok((table, primary_key, indexes))
}

/// Parse `"name:col1,col2"` into an [`IndexSpec`]. The bare-column
/// form `"name:col"` is the common single-column case.
fn parse_index_spec(lit: &LitStr) -> syn::Result<IndexSpec> {
    let raw = lit.value();
    let (name, cols) = raw.split_once(':').ok_or_else(|| {
        syn::Error::new_spanned(
            lit,
            "index spec must be \"index_name:col1,col2\" (missing ':')",
        )
    })?;
    let name = name.trim().to_string();
    let columns: Vec<String> = cols
        .split(',')
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    if name.is_empty() || columns.is_empty() {
        return Err(syn::Error::new_spanned(
            lit,
            "index spec must name an index and at least one column",
        ));
    }
    Ok(IndexSpec { name, columns })
}

fn expand_raw_table(input: DeriveInput) -> syn::Result<TokenStream2> {
    let struct_name = input.ident.clone();
    let (table, pk_attr, indexes) = parse_raw_table_attrs(&input.attrs, &struct_name)?;
    let fields = collect_named_fields(&input)?;

    // Index DDLs are mode-independent — they only reference column
    // names, which exist in both modes.
    let index_literals: Vec<LitStr> = indexes
        .iter()
        .map(|ix| {
            let cols = ix.columns.join(", ");
            LitStr::new(
                &format!(
                    "CREATE INDEX IF NOT EXISTS {name} ON {table}({cols})",
                    name = ix.name,
                ),
                proc_macro2::Span::call_site(),
            )
        })
        .collect();

    // Detect the optional WirePayload field.
    let mut wire_ident: Option<&Ident> = None;
    let mut other_fields: Vec<&Field> = Vec::new();
    for f in &fields {
        if is_wire_payload(&f.ty) {
            if wire_ident.is_some() {
                return Err(syn::Error::new_spanned(
                    f,
                    "duplicate WirePayload field; exactly one is allowed",
                ));
            }
            wire_ident = Some(f.ident.as_ref().expect("named field"));
        } else {
            other_fields.push(f);
        }
    }

    let table_lit = LitStr::new(&table, proc_macro2::Span::call_site());

    let (ddl_tokens, bulk_impl) = if let Some(wp) = wire_ident {
        // ── Payload mode ────────────────────────────────────────────
        if let Some(pk) = &pk_attr {
            return Err(syn::Error::new_spanned(
                &struct_name,
                format!(
                    "primary_key = \"{pk}\" is invalid in payload mode; \
                     the WirePayload field already supplies the `id` PK"
                ),
            ));
        }
        let promoted: Vec<PromotedField> = other_fields
            .iter()
            .map(|f| PromotedField::from_field(f))
            .collect::<syn::Result<_>>()?;
        let ddl_literals = promoted_decl_literals(&promoted);
        let typed_col_names: Vec<LitStr> = promoted
            .iter()
            .map(|p| LitStr::new(&p.name.to_string(), proc_macro2::Span::call_site()))
            .collect();
        let promoted_binds: Vec<TokenStream2> = promoted.iter().map(|p| p.bind_expr()).collect();

        let ddl = quote! {
            ::frankweiler_etl::doltlite_raw::wire_payload_table_ddl(
                #table_lit,
                &[#(#ddl_literals),*],
            )
        };
        let bulk = quote! {
            impl ::frankweiler_etl::bulk::BulkUpsertable for #struct_name {
                const TABLE: &'static str = #table_lit;
                const TYPED_COLUMNS: &'static [&'static str] = &[#(#typed_col_names),*];

                fn id(&self) -> &str {
                    &self.#wp.id
                }

                fn bind_into<'q>(
                    &'q self,
                    q: ::sqlx::query::Query<'q, ::sqlx::Sqlite, ::sqlx::sqlite::SqliteArguments<'q>>,
                ) -> ::sqlx::query::Query<'q, ::sqlx::Sqlite, ::sqlx::sqlite::SqliteArguments<'q>> {
                    q.bind(&self.#wp.id)
                        #(.bind(#promoted_binds))*
                        .bind(&self.#wp.payload)
                }
            }
        };
        (ddl, bulk)
    } else {
        // ── Plain mode (no payload column) ──────────────────────────
        let pk_name = pk_attr.unwrap_or_else(|| "id".to_string());
        let pk_field = other_fields
            .iter()
            .find(|f| f.ident.as_ref().expect("named field") == &pk_name)
            .ok_or_else(|| {
                syn::Error::new_spanned(
                    &struct_name,
                    format!("primary_key column `{pk_name}` is not a field of this struct"),
                )
            })?;
        let pk_promoted = PromotedField::from_field(pk_field)?;
        let pk_decl = match pk_promoted.kind {
            PromotedKind::TextNotNull => format!("{pk_name} TEXT PRIMARY KEY"),
            PromotedKind::IntegerNotNull => format!("{pk_name} INTEGER PRIMARY KEY"),
            _ => {
                return Err(syn::Error::new_spanned(
                    &pk_field.ty,
                    "primary-key column must be `String` or `i64` (non-nullable)",
                ))
            }
        };

        // Typed columns = every field except the PK, declaration order.
        let typed: Vec<PromotedField> = other_fields
            .iter()
            .filter(|f| f.ident.as_ref().expect("named field") != &pk_name)
            .map(|f| PromotedField::from_field(f))
            .collect::<syn::Result<_>>()?;

        // CREATE TABLE: PK first, then typed columns, padded to align.
        let mut decls: Vec<String> = vec![pk_decl];
        decls.extend(typed.iter().map(|p| format!("{} {}", p.name, p.sql_type)));
        let max = decls
            .iter()
            .map(|d| d.split(' ').next().unwrap_or("").len())
            .max()
            .unwrap_or(0);
        let body = decls
            .iter()
            .map(|d| {
                let (name, rest) = d.split_once(' ').unwrap_or((d.as_str(), ""));
                let pad = " ".repeat(max.saturating_sub(name.len()) + 1);
                format!("{name}{pad}{rest}")
            })
            .collect::<Vec<_>>()
            .join(",\n    ");
        let ddl_string = format!("CREATE TABLE IF NOT EXISTS {table} (\n    {body}\n)");
        let ddl_lit = LitStr::new(&ddl_string, proc_macro2::Span::call_site());

        let pk_ident = pk_field.ident.as_ref().expect("named field");
        let pk_lit = LitStr::new(&pk_name, proc_macro2::Span::call_site());
        let typed_col_names: Vec<LitStr> = typed
            .iter()
            .map(|p| LitStr::new(&p.name.to_string(), proc_macro2::Span::call_site()))
            .collect();
        let typed_binds: Vec<TokenStream2> = typed.iter().map(|p| p.bind_expr()).collect();
        let pk_bind = pk_promoted.bind_expr();

        let ddl = quote! { ::std::string::String::from(#ddl_lit) };
        let bulk = quote! {
            impl ::frankweiler_etl::bulk::BulkUpsertable for #struct_name {
                const TABLE: &'static str = #table_lit;
                const ID_COLUMN: &'static str = #pk_lit;
                const TYPED_COLUMNS: &'static [&'static str] = &[#(#typed_col_names),*];
                const PAYLOAD_COLUMN: ::std::option::Option<&'static str> =
                    ::std::option::Option::None;

                fn id(&self) -> &str {
                    &self.#pk_ident
                }

                fn bind_into<'q>(
                    &'q self,
                    q: ::sqlx::query::Query<'q, ::sqlx::Sqlite, ::sqlx::sqlite::SqliteArguments<'q>>,
                ) -> ::sqlx::query::Query<'q, ::sqlx::Sqlite, ::sqlx::sqlite::SqliteArguments<'q>> {
                    q.bind(#pk_bind)
                        #(.bind(#typed_binds))*
                }
            }
        };
        (ddl, bulk)
    };

    Ok(quote! {
        impl #struct_name {
            /// `CREATE TABLE IF NOT EXISTS …` for this table.
            pub fn ddl() -> ::std::string::String {
                #ddl_tokens
            }
            /// `CREATE INDEX IF NOT EXISTS …` for each declared index.
            pub fn index_ddls() -> ::std::vec::Vec<::std::string::String> {
                ::std::vec![#(::std::string::String::from(#index_literals)),*]
            }
            /// Table DDL followed by every index DDL — splice straight
            /// into a provider's `full_ddl()`.
            pub fn all_ddl() -> ::std::vec::Vec<::std::string::String> {
                let mut v = ::std::vec![Self::ddl()];
                v.extend(Self::index_ddls());
                v
            }
        }

        #bulk_impl
    })
}

/// Shared between `WirePayloadRow` and `RawTable` payload mode: turn a
/// promoted-column list into the width-aligned `name TYPE` decl string
/// literals `wire_payload_table_ddl` expects.
fn promoted_decl_literals(promoted: &[PromotedField]) -> Vec<LitStr> {
    let max_name_len = promoted
        .iter()
        .map(|p| p.name.to_string().len())
        .max()
        .unwrap_or(0);
    promoted
        .iter()
        .map(|p| {
            let name = p.name.to_string();
            let pad = " ".repeat(max_name_len.saturating_sub(name.len()) + 1);
            LitStr::new(
                &format!("{name}{pad}{}", p.sql_type),
                proc_macro2::Span::call_site(),
            )
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────
// CasEdgeRow
// ─────────────────────────────────────────────────────────────────────

/// Derive [`frankweiler_etl::blob_cas::CasEdgeRow`] and
/// [`frankweiler_etl::bulk::BulkUpsertable`] for a per-provider CAS
/// edge row struct.
///
/// **Required shape.** The struct must have **exactly four named
/// fields, in this order**:
///
///   1. `id: String` — synthesized PK (`"{owning_id}#{ref_id}"`)
///   2. `<owning>: String` — owning-entity FK (column name read from
///      this field's identifier; e.g. `message_uuid`)
///   3. `<ref>: String` — upstream ref id (column name read from this
///      field's identifier; e.g. `file_id`)
///   4. `blake3: Option<String>` — CAS hash, `NULL` until stored
///
/// **Attribute.** `#[cas_edge_row(table = "name")]` names the SQL
/// table. Required.
///
/// The fixed shape comes from the universal pattern of every
/// per-provider attachment-edge table; see
/// [`frankweiler_etl::blob_cas::CasEdgeRow`] for the
/// rationale. Field-name validation enforces that `id` is first and
/// `blake3` is last; the two middle fields' identifiers become the
/// emitted column names.
///
/// **Example.**
/// ```ignore
/// use frankweiler_etl_macros::CasEdgeRow;
///
/// #[derive(CasEdgeRow)]
/// #[cas_edge_row(table = "slack_attachments")]
/// pub struct SlackAttachmentRow {
///     pub id: String,
///     pub message_uuid: String,
///     pub file_id: String,
///     pub blake3: Option<String>,
/// }
/// ```
///
/// emits the table DDL, the two index DDLs, the
/// [`frankweiler_etl::bulk::BulkUpsertable`] impl, and the
/// [`frankweiler_etl::blob_cas::CasEdgeRow`] impl with
/// `OWNING_COLUMN = "message_uuid"` and `REF_COLUMN = "file_id"`.
#[proc_macro_derive(CasEdgeRow, attributes(cas_edge_row))]
pub fn derive_cas_edge_row(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_cas_edge_row(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_cas_edge_row(input: DeriveInput) -> syn::Result<TokenStream2> {
    let struct_name = input.ident.clone();
    let table = parse_cas_edge_table_attr(&input.attrs, &struct_name)?;
    let fields = collect_named_fields(&input)?;
    if fields.len() != 4 {
        return Err(syn::Error::new_spanned(
            &struct_name,
            "CasEdgeRow requires exactly 4 fields: id, <owning>, <ref>, blake3",
        ));
    }
    let id_field = fields[0];
    let owning_field = fields[1];
    let ref_field = fields[2];
    let blake3_field = fields[3];

    // Field-name discipline: first must be `id`, last must be
    // `blake3`. The middle two carry the column names.
    let id_ident = id_field.ident.as_ref().expect("named field");
    let blake3_ident = blake3_field.ident.as_ref().expect("named field");
    if id_ident != "id" {
        return Err(syn::Error::new_spanned(
            id_field,
            "CasEdgeRow's first field must be named `id`",
        ));
    }
    if blake3_ident != "blake3" {
        return Err(syn::Error::new_spanned(
            blake3_field,
            "CasEdgeRow's last field must be named `blake3`",
        ));
    }
    if !is_plain_string(&id_field.ty) {
        return Err(syn::Error::new_spanned(
            &id_field.ty,
            "CasEdgeRow's `id` field must be `String`",
        ));
    }
    if !is_option_string(&blake3_field.ty) {
        return Err(syn::Error::new_spanned(
            &blake3_field.ty,
            "CasEdgeRow's `blake3` field must be `Option<String>`",
        ));
    }
    if !is_plain_string(&owning_field.ty) {
        return Err(syn::Error::new_spanned(
            &owning_field.ty,
            "CasEdgeRow's owning-FK field must be `String`",
        ));
    }
    if !is_plain_string(&ref_field.ty) {
        return Err(syn::Error::new_spanned(
            &ref_field.ty,
            "CasEdgeRow's ref-id field must be `String`",
        ));
    }

    let owning_ident = owning_field.ident.as_ref().expect("named field");
    let ref_ident = ref_field.ident.as_ref().expect("named field");
    let owning_name = owning_ident.to_string();
    let ref_name = ref_ident.to_string();

    let table_lit = LitStr::new(&table, proc_macro2::Span::call_site());
    let owning_lit = LitStr::new(&owning_name, proc_macro2::Span::call_site());
    let ref_lit = LitStr::new(&ref_name, proc_macro2::Span::call_site());

    Ok(quote! {
        impl ::frankweiler_etl::bulk::BulkUpsertable for #struct_name {
            const TABLE: &'static str = #table_lit;
            const TYPED_COLUMNS: &'static [&'static str] = &[#owning_lit, #ref_lit, "blake3"];
            const PAYLOAD_COLUMN: ::std::option::Option<&'static str> = ::std::option::Option::None;

            fn id(&self) -> &str {
                &self.#id_ident
            }

            fn bind_into<'q>(
                &'q self,
                q: ::sqlx::query::Query<
                    'q,
                    ::sqlx::Sqlite,
                    ::sqlx::sqlite::SqliteArguments<'q>,
                >,
            ) -> ::sqlx::query::Query<
                'q,
                ::sqlx::Sqlite,
                ::sqlx::sqlite::SqliteArguments<'q>,
            > {
                q.bind(&self.#id_ident)
                    .bind(&self.#owning_ident)
                    .bind(&self.#ref_ident)
                    .bind(self.#blake3_ident.as_deref())
            }
        }

        impl ::frankweiler_etl::blob_cas::CasEdgeRow for #struct_name {
            const OWNING_COLUMN: &'static str = #owning_lit;
            const REF_COLUMN: &'static str = #ref_lit;
        }
    })
}

fn parse_cas_edge_table_attr(attrs: &[Attribute], struct_name: &Ident) -> syn::Result<String> {
    for attr in attrs {
        if !attr.path().is_ident("cas_edge_row") {
            continue;
        }
        let mut table: Option<String> = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("table") {
                let lit: LitStr = meta.value()?.parse()?;
                table = Some(lit.value());
                Ok(())
            } else {
                Err(meta.error("unknown #[cas_edge_row(...)] key; supported keys: `table`"))
            }
        })?;
        return table.ok_or_else(|| {
            syn::Error::new_spanned(attr, "#[cas_edge_row(table = \"...\")] is required")
        });
    }
    Err(syn::Error::new_spanned(
        struct_name,
        "#[derive(CasEdgeRow)] requires #[cas_edge_row(table = \"…\")]",
    ))
}

fn is_plain_string(ty: &Type) -> bool {
    matches!(classify(ty), Some(PromotedKind::TextNotNull))
}

fn is_option_string(ty: &Type) -> bool {
    matches!(classify(ty), Some(PromotedKind::TextNullable))
}

// ─────────────────────────────────────────────────────────────────────
// PortableTable
// ─────────────────────────────────────────────────────────────────────

/// Derive the portable `CREATE TABLE` surface (`DDL` + `TABLES` +
/// `COLUMNS` module consts) for a hand-written *presentation* row
/// struct — the denormalized tables that back the grid / UI
/// (`grid_rows`, `edges`, `markdowns`, `feedback`, `sync_jobs`).
///
/// This is the sibling of [`WirePayloadRow`]/[`RawTable`] for the
/// *consumer* side of the system. Where those derive the raw-store
/// wire shape (id + payload + promoted columns, `TEXT`/`INTEGER`/`REAL`,
/// plus a `BulkUpsertable` impl bound through sqlx), `PortableTable`
/// covers flat typed tables whose columns use portable MySQL/Dolt/SQLite
/// types (`VARCHAR(n)`, `LONGTEXT`, `JSON`, `DOUBLE`, …) and whose rows
/// are written by hand-rolled `INSERT`s. So this derive emits **only**
/// the DDL/metadata consts — no `BulkUpsertable`, no serde (the struct
/// derives `Serialize`/`Deserialize` itself).
///
/// It replaces the old `schemas/codegen.py` JSON-Schema → Rust path:
/// the struct is the single source of truth, the same way extract's
/// `schema_raw.rs` already is.
///
/// **Struct attribute.** `#[portable_table(table = "grid_rows",
/// primary_key = "uuid")]`. Both keys are required. `primary_key`
/// accepts a comma-separated list for composite keys.
///
/// **Per-field attribute.** `#[col(sql = "VARCHAR(96)")]` gives the
/// portable SQL base type. Required on every field. Nullability is
/// inferred from the Rust type: `Option<T>` → nullable (bare type),
/// anything else → `… NOT NULL`.
///
/// **Derived columns.** Some tables carry columns that live in the DB
/// but are computed at load time and so are absent from the struct
/// (e.g. `grid_rows.when_ts_utc` / `when_offset`, derived from
/// `when_ts`). Declare them with a repeatable field attribute on the
/// column they follow: `#[derived(name = "when_ts_utc", sql =
/// "VARCHAR(40)")]`. Derived columns are always nullable and are
/// emitted into the DDL + `COLUMNS` immediately after their host field.
///
/// **Example.**
/// ```ignore
/// #[derive(Debug, Clone, Serialize, Deserialize, PortableTable)]
/// #[portable_table(table = "edges", primary_key = "edge_uuid")]
/// pub struct EdgeRow {
///     #[col(sql = "VARCHAR(96)")]
///     pub edge_uuid: String,
///     #[col(sql = "VARCHAR(96)")]
///     pub src_markdown_uuid: String,
///     #[col(sql = "VARCHAR(64)")]
///     pub label: Option<String>,
/// }
/// ```
///
/// emits module-level `pub const TABLES`, `pub const DDL`, and
/// `pub const COLUMNS` matching the byte shape `codegen.py` produced.
#[proc_macro_derive(PortableTable, attributes(portable_table, col, derived))]
pub fn derive_portable_table(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_portable_table(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

struct PortableColumn {
    name: String,
    decl: String,
}

fn expand_portable_table(input: DeriveInput) -> syn::Result<TokenStream2> {
    let struct_name = input.ident.clone();
    let (table, primary_key) = parse_portable_table_attr(&input.attrs, &struct_name)?;
    let fields = collect_named_fields(&input)?;

    let mut columns: Vec<PortableColumn> = Vec::new();
    for f in &fields {
        let name = f.ident.as_ref().expect("named field").to_string();
        let sql = parse_col_attr(f)?;
        // Nullability follows the Rust type: Option<T> → nullable.
        let decl = if is_option(&f.ty) {
            format!("{name} {sql}")
        } else {
            format!("{name} {sql} NOT NULL")
        };
        columns.push(PortableColumn { name, decl });
        // Load-time-derived columns trail their host field, always
        // nullable (they are absent from the struct).
        for (dname, dsql) in parse_derived_attrs(f)? {
            columns.push(PortableColumn {
                decl: format!("{dname} {dsql}"),
                name: dname,
            });
        }
    }

    let mut decl_lines: Vec<String> = columns.iter().map(|c| c.decl.clone()).collect();
    decl_lines.push(format!("PRIMARY KEY ({primary_key})"));
    let body = decl_lines.join(",\n    ");
    let ddl = format!("CREATE TABLE IF NOT EXISTS {table} (\n    {body}\n)");

    let table_lit = LitStr::new(&table, proc_macro2::Span::call_site());
    let struct_name_lit = LitStr::new(&struct_name.to_string(), proc_macro2::Span::call_site());
    let ddl_lit = LitStr::new(&ddl, proc_macro2::Span::call_site());
    let col_lits: Vec<LitStr> = columns
        .iter()
        .map(|c| LitStr::new(&c.name, proc_macro2::Span::call_site()))
        .collect();

    Ok(quote! {
        /// `(table_name, Rust struct name)` for the table this schema defines.
        pub const TABLES: &[(&str, &str)] = &[(#table_lit, #struct_name_lit)];

        /// Portable `CREATE TABLE IF NOT EXISTS` — the SQL subset
        /// accepted by Dolt, MySQL, and SQLite.
        pub const DDL: &[(&str, &str)] = &[(#table_lit, #ddl_lit)];

        /// Column names, in declaration order (struct fields plus any
        /// load-time-derived columns).
        pub const COLUMNS: &[(&str, &[&str])] = &[(#table_lit, &[#(#col_lits),*])];
    })
}

fn parse_portable_table_attr(
    attrs: &[Attribute],
    struct_name: &Ident,
) -> syn::Result<(String, String)> {
    for attr in attrs {
        if !attr.path().is_ident("portable_table") {
            continue;
        }
        let mut table: Option<String> = None;
        let mut primary_key: Option<String> = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("table") {
                table = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else if meta.path.is_ident("primary_key") {
                primary_key = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else {
                Err(meta.error(
                    "unknown #[portable_table(...)] key; supported keys: `table`, `primary_key`",
                ))
            }
        })?;
        let table = table.ok_or_else(|| {
            syn::Error::new_spanned(attr, "#[portable_table(table = \"...\")] is required")
        })?;
        let primary_key = primary_key.ok_or_else(|| {
            syn::Error::new_spanned(attr, "#[portable_table(primary_key = \"...\")] is required")
        })?;
        return Ok((table, primary_key));
    }
    Err(syn::Error::new_spanned(
        struct_name,
        "#[derive(PortableTable)] requires #[portable_table(table = \"…\", primary_key = \"…\")]",
    ))
}

/// Read the required `#[col(sql = "…")]` portable type for one field.
fn parse_col_attr(field: &Field) -> syn::Result<String> {
    for attr in &field.attrs {
        if !attr.path().is_ident("col") {
            continue;
        }
        let mut sql: Option<String> = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("sql") {
                sql = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else {
                Err(meta.error("unknown #[col(...)] key; supported keys: `sql`"))
            }
        })?;
        return sql
            .ok_or_else(|| syn::Error::new_spanned(attr, "#[col(sql = \"...\")] is required"));
    }
    Err(syn::Error::new_spanned(
        field,
        "every #[derive(PortableTable)] field needs #[col(sql = \"…\")]",
    ))
}

/// Collect the repeatable `#[derived(name = "…", sql = "…")]` columns
/// that trail a field.
fn parse_derived_attrs(field: &Field) -> syn::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for attr in &field.attrs {
        if !attr.path().is_ident("derived") {
            continue;
        }
        let mut name: Option<String> = None;
        let mut sql: Option<String> = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                name = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else if meta.path.is_ident("sql") {
                sql = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else {
                Err(meta.error("unknown #[derived(...)] key; supported keys: `name`, `sql`"))
            }
        })?;
        let name = name.ok_or_else(|| {
            syn::Error::new_spanned(
                attr,
                "#[derived(name = \"...\", sql = \"...\")] needs `name`",
            )
        })?;
        let sql = sql.ok_or_else(|| {
            syn::Error::new_spanned(
                attr,
                "#[derived(name = \"...\", sql = \"...\")] needs `sql`",
            )
        })?;
        out.push((name, sql));
    }
    Ok(out)
}

fn is_option(ty: &Type) -> bool {
    let Type::Path(TypePath { path, .. }) = ty else {
        return false;
    };
    path.segments
        .last()
        .map(|seg| seg.ident == "Option")
        .unwrap_or(false)
}
