//! Proc-macros for the frankweiler ETL crates.
//!
//! Today there is exactly one: [`WirePayloadRow`], which derives the
//! DDL + bulk-upsert plumbing for any Rust row struct that maps to a
//! wire-payload entity table (id + payload + promoted columns). See
//! the doc on the macro itself for the conventions the derived code
//! follows.

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
}

impl PromotedKind {
    fn sql_type(self) -> &'static str {
        match self {
            PromotedKind::TextNotNull => "TEXT NOT NULL",
            PromotedKind::TextNullable => "TEXT NULL",
            PromotedKind::IntegerNotNull => "INTEGER NOT NULL",
            PromotedKind::IntegerNullable => "INTEGER NULL",
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
                 supported types: String, Option<String>, i64, Option<i64>",
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
    /// `Option<i64>` by value for INTEGER columns.
    fn bind_expr(&self) -> TokenStream2 {
        let name = self.name;
        match self.kind {
            PromotedKind::TextNotNull => quote! { &self.#name },
            PromotedKind::TextNullable => quote! { self.#name.as_deref() },
            PromotedKind::IntegerNotNull => quote! { self.#name },
            PromotedKind::IntegerNullable => quote! { self.#name },
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
                _ => None,
            }
        }
        _ => None,
    }
}
