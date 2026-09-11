//! Proc-macros for the datalib ETL crates.
//!
//! Four table derives, one per table shape, so a provider's
//! `schema_raw.rs` is its row structs and nothing else — plus
//! `RawStoreHandle`, which reads a store handle's fields so that closing
//! every pool it opened is not something anyone has to remember. Required
//! struct shapes, attributes and the Rust→SQL type mapping are in this
//! crate's README.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    parse_macro_input, Attribute, Data, DataStruct, DeriveInput, Field, Fields, GenericArgument,
    Ident, LitStr, PathArguments, Type, TypePath,
};

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
        impl ::datalib_etl::doltlite_raw::WirePayloadRow for #struct_name {
            fn ddl() -> ::std::string::String {
                ::datalib_etl::doltlite_raw::wire_payload_table_ddl(
                    #table_lit,
                    &[#(#ddl_literals),*],
                )
            }
        }

        impl ::datalib_etl::bulk::BulkUpsertable for #struct_name {
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
                    ::sqlx::sqlite::SqliteArguments,
                >,
            ) -> ::sqlx::query::Query<
                'q,
                ::sqlx::Sqlite,
                ::sqlx::sqlite::SqliteArguments,
            > {
                q.bind(&self.#id_and_payload_ident.id)
                    #(.bind(#promoted_binds))*
                    .bind(&self.#id_and_payload_ident.payload)
            }
        }
    })
}

// Attribute parsing

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

/// Implement `datalib_etl::store_handle::RawStoreHandle` by reading the
/// struct's fields: every `SqlitePool` and every `BlobCas`, in declaration
/// order, and nothing else.
///
/// The point is exhaustiveness. A handle that grows a second store is the
/// shape that has already gone wrong here — `RawStoreSession::finish`
/// named its entity pool directly and left the blob CAS open — and a
/// hand-written list is exactly where that recurs. Matching is on the
/// trailing path segment, as `WirePayloadRow` does, so `SqlitePool`,
/// `sqlx::SqlitePool` and `Option<BlobCas>` all resolve.
#[proc_macro_derive(RawStoreHandle)]
pub fn derive_raw_store_handle(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_raw_store_handle(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// How a field contributes pools, or `None` when it is not a store.
enum StoreField {
    Pool,
    Cas,
    OptionalPool,
    OptionalCas,
}

fn classify_store_field(ty: &Type) -> Option<StoreField> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return None;
    };
    let seg = path.segments.last()?;
    match seg.ident.to_string().as_str() {
        "SqlitePool" => Some(StoreField::Pool),
        "BlobCas" => Some(StoreField::Cas),
        "Option" => {
            let PathArguments::AngleBracketed(args) = &seg.arguments else {
                return None;
            };
            let GenericArgument::Type(inner) = args.args.first()? else {
                return None;
            };
            match classify_store_field(inner)? {
                StoreField::Pool => Some(StoreField::OptionalPool),
                StoreField::Cas => Some(StoreField::OptionalCas),
                _ => None,
            }
        }
        _ => None,
    }
}

fn expand_raw_store_handle(input: DeriveInput) -> syn::Result<TokenStream2> {
    let name = &input.ident;
    let fields = collect_named_fields(&input)?;
    let mut pushes: Vec<TokenStream2> = Vec::new();
    for f in &fields {
        let ident = f.ident.as_ref().expect("named field");
        match classify_store_field(&f.ty) {
            Some(StoreField::Pool) => pushes.push(quote! { out.push(&self.#ident); }),
            Some(StoreField::Cas) => pushes.push(quote! { out.push(self.#ident.pool()); }),
            Some(StoreField::OptionalPool) => pushes.push(quote! {
                if let Some(p) = self.#ident.as_ref() { out.push(p); }
            }),
            Some(StoreField::OptionalCas) => pushes.push(quote! {
                if let Some(c) = self.#ident.as_ref() { out.push(c.pool()); }
            }),
            None => {}
        }
    }
    if pushes.is_empty() {
        return Err(syn::Error::new_spanned(
            name,
            "#[derive(RawStoreHandle)] found no store field; add a SqlitePool or BlobCas, \
             or drop the derive",
        ));
    }
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    Ok(quote! {
        impl #impl_generics ::datalib_etl::store_handle::RawStoreHandle
            for #name #ty_generics #where_clause
        {
            fn pools(&self) -> ::std::vec::Vec<&::sqlx::sqlite::SqlitePool> {
                let mut out = ::std::vec::Vec::new();
                #(#pushes)*
                out
            }
        }
    })
}

// Field walking

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

// Promoted-column type mapping

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

// RawTable

/// Derive a raw-store table's **entire** SQL surface from a Rust
/// struct: the `CREATE TABLE` DDL, any `CREATE INDEX` DDLs, and the
/// [`datalib_etl::bulk::BulkUpsertable`] impl that the generic
/// bulk-upsert helper writes through. The goal is that a provider's
/// `schema_raw.rs` contains *no* hand-written `CREATE TABLE` strings
/// and *no* hand-written `BulkUpsertable` impls — just structs.
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
            ::datalib_etl::doltlite_raw::wire_payload_table_ddl(
                #table_lit,
                &[#(#ddl_literals),*],
            )
        };
        let bulk = quote! {
            impl ::datalib_etl::bulk::BulkUpsertable for #struct_name {
                const TABLE: &'static str = #table_lit;
                const TYPED_COLUMNS: &'static [&'static str] = &[#(#typed_col_names),*];

                fn id(&self) -> &str {
                    &self.#wp.id
                }

                fn bind_into<'q>(
                    &'q self,
                    q: ::sqlx::query::Query<'q, ::sqlx::Sqlite, ::sqlx::sqlite::SqliteArguments>,
                ) -> ::sqlx::query::Query<'q, ::sqlx::Sqlite, ::sqlx::sqlite::SqliteArguments> {
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
            impl ::datalib_etl::bulk::BulkUpsertable for #struct_name {
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
                    q: ::sqlx::query::Query<'q, ::sqlx::Sqlite, ::sqlx::sqlite::SqliteArguments>,
                ) -> ::sqlx::query::Query<'q, ::sqlx::Sqlite, ::sqlx::sqlite::SqliteArguments> {
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
            pub fn all_ddl() -> ::std::vec::Vec<::std::string::String> {
                let mut v = ::std::vec![Self::ddl()];
                v.extend(Self::index_ddls());
                v
            }
        }

        #bulk_impl
    })
}

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

// CasEdgeRow

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
        impl ::datalib_etl::bulk::BulkUpsertable for #struct_name {
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
                    ::sqlx::sqlite::SqliteArguments,
                >,
            ) -> ::sqlx::query::Query<
                'q,
                ::sqlx::Sqlite,
                ::sqlx::sqlite::SqliteArguments,
            > {
                q.bind(&self.#id_ident)
                    .bind(&self.#owning_ident)
                    .bind(&self.#ref_ident)
                    .bind(self.#blake3_ident.as_deref())
            }
        }

        impl ::datalib_etl::blob_cas::CasEdgeRow for #struct_name {
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

// PortableTable

/// Derive the portable `CREATE TABLE` surface (`DDL` + `TABLES` +
/// `COLUMNS` module consts) for a hand-written *presentation* row
/// struct — the denormalized tables that back the grid / UI
/// (`grid_rows`, `edges`, `markdowns`, `feedback`, `sync_jobs`).
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
    /// How this column's value is bound in the generated
    /// `BulkUpsertable::bind_into`. A struct field binds itself; a
    /// `#[derived]` column has no field to bind, so it calls a
    /// `derived_<column>()` method the struct must provide by hand.
    bind: TokenStream2,
}

fn expand_portable_table(input: DeriveInput) -> syn::Result<TokenStream2> {
    let struct_name = input.ident.clone();
    let (table, primary_key) = parse_portable_table_attr(&input.attrs, &struct_name)?;
    let fields = collect_named_fields(&input)?;

    let mut columns: Vec<PortableColumn> = Vec::new();
    for f in &fields {
        let ident = f.ident.as_ref().expect("named field");
        let ColAttr { sql, name } = parse_col_attr(f)?;
        let name = name.unwrap_or_else(|| ident.to_string());
        // Nullability follows the Rust type: Option<T> → nullable.
        let decl = if is_option(&f.ty) {
            format!("{name} {sql}")
        } else {
            format!("{name} {sql} NOT NULL")
        };
        // Same three shapes `PromotedKind::bind_expr` uses on the raw
        // side: a borrow for owned text, `as_deref` for optional text,
        // and a copy for scalars. Anything else is rejected rather than
        // guessed at — a silently mis-bound column is exactly the class
        // of bug generating this is meant to remove.
        let bind = match classify(&f.ty) {
            Some(PromotedKind::TextNotNull) => quote! { &self.#ident },
            Some(PromotedKind::TextNullable) => quote! { self.#ident.as_deref() },
            Some(
                PromotedKind::IntegerNotNull
                | PromotedKind::IntegerNullable
                | PromotedKind::RealNotNull
                | PromotedKind::RealNullable,
            ) => quote! { self.#ident },
            None => {
                return Err(syn::Error::new_spanned(
                    &f.ty,
                    "PortableTable can only bind String, i64, f64 or Option of those; \
                     add support to `classify` rather than binding this column by hand",
                ))
            }
        };
        columns.push(PortableColumn { name, decl, bind });
        // Load-time-derived columns trail their host field, always
        // nullable (they are absent from the struct).
        for (dname, dsql) in parse_derived_attrs(f)? {
            // No field to bind, so the struct supplies the value
            // through a method named for the column. A missing one is a
            // compile error, which is the point: adding a derived
            // column without saying how to compute it should not
            // silently write NULL.
            let hook = Ident::new(&format!("derived_{dname}"), proc_macro2::Span::call_site());
            columns.push(PortableColumn {
                decl: format!("{dname} {dsql}"),
                name: dname,
                bind: quote! { self.#hook() },
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

    // The write path. TYPED_COLUMNS and the bind order below are built
    // from one list, so they cannot disagree — which is the failure the
    // trait's docs warn about ("Mismatch → mis-binding at runtime") and
    // the reason to generate this rather than hand-write it.
    let composite_pk = primary_key.contains(',');
    let write_path = if composite_pk {
        quote! {}
    } else {
        let pk_ident = Ident::new(primary_key.trim(), proc_macro2::Span::call_site());
        let pk_lit = LitStr::new(primary_key.trim(), proc_macro2::Span::call_site());
        let typed: Vec<&PortableColumn> = columns
            .iter()
            .filter(|c| c.name != primary_key.trim())
            .collect();
        let typed_col_lits: Vec<LitStr> = typed
            .iter()
            .map(|c| LitStr::new(&c.name, proc_macro2::Span::call_site()))
            .collect();
        let typed_binds: Vec<TokenStream2> = typed.iter().map(|c| c.bind.clone()).collect();
        quote! {
        /// Generated write path, so the render schema binds its columns
        /// the same way the raw stores do. `datalib_etl::bulk`'s helpers
        /// take any `BulkUpsertable`.
        impl ::datalib_table::BulkUpsertable for #struct_name {
            const TABLE: &'static str = #table_lit;
            const ID_COLUMN: &'static str = #pk_lit;
            const TYPED_COLUMNS: &'static [&'static str] = &[#(#typed_col_lits),*];
            /// Presentation tables carry no JSONB wire payload; every
            /// column is typed.
            const PAYLOAD_COLUMN: ::std::option::Option<&'static str> =
                ::std::option::Option::None;

            fn id(&self) -> &str {
                &self.#pk_ident
            }

            fn bind_into<'q>(
                &'q self,
                q: ::sqlx::query::Query<'q, ::sqlx::Sqlite, ::sqlx::sqlite::SqliteArguments>,
            ) -> ::sqlx::query::Query<'q, ::sqlx::Sqlite, ::sqlx::sqlite::SqliteArguments> {
                q.bind(&self.#pk_ident)
                    #(.bind(#typed_binds))*
            }
        }
        }
    };

    Ok(quote! {
        #write_path

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

/// A parsed `#[col(...)]`. `name` overrides the column name, which
/// otherwise follows the field ident — for a field renamed after its
/// column was already written to disk.
struct ColAttr {
    sql: String,
    name: Option<String>,
}

fn parse_col_attr(field: &Field) -> syn::Result<ColAttr> {
    for attr in &field.attrs {
        if !attr.path().is_ident("col") {
            continue;
        }
        let mut sql: Option<String> = None;
        let mut name: Option<String> = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("sql") {
                sql = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else if meta.path.is_ident("name") {
                name = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else {
                Err(meta.error("unknown #[col(...)] key; supported keys: `sql`, `name`"))
            }
        })?;
        let sql =
            sql.ok_or_else(|| syn::Error::new_spanned(attr, "#[col(sql = \"...\")] is required"))?;
        return Ok(ColAttr { sql, name });
    }
    Err(syn::Error::new_spanned(
        field,
        "every #[derive(PortableTable)] field needs #[col(sql = \"…\")]",
    ))
}

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
