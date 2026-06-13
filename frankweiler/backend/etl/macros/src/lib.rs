//! Proc-macros for the frankweiler ETL crates.
//!
//! Two derives today:
//!
//!   - [`WirePayloadRow`] — DDL + bulk-upsert plumbing for any row
//!     struct that maps to a wire-payload entity table (id + payload
//!     + promoted columns).
//!   - [`CasEdgeRow`] — every per-provider CAS edge table (each
//!     attachment / blob-link table) follows the same four-column
//!     shape; this derive emits the
//!     [`frankweiler_etl::blob_cas::CasEdgeRow`] +
//!     [`frankweiler_etl::bulk::BulkUpsertable`] impls so the
//!     provider's `schema_raw.rs` is just the struct.

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
    // DDLs the macro is replacing.
    let max_name_len = promoted
        .iter()
        .map(|p| p.name.to_string().len())
        .max()
        .unwrap_or(0);
    let promoted_decls: Vec<String> = promoted
        .iter()
        .map(|p| {
            let name = p.name.to_string();
            let pad = " ".repeat(max_name_len.saturating_sub(name.len()) + 1);
            format!("{name}{pad}{}", p.sql_type)
        })
        .collect();
    let ddl_literals: Vec<LitStr> = promoted_decls
        .iter()
        .map(|s| LitStr::new(s, proc_macro2::Span::call_site()))
        .collect();
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
