//! What `PortableTable` writes beside the DDL: an enum naming every column,
//! and, for a table marked `search(...)`, the `datalib_query::table::
//! SearchTable` the search bar reads it through. The attributes are in
//! this crate's README.

use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::meta::ParseNestedMeta;
use syn::{Ident, LitStr, Token, Visibility};

/// A column's search attributes, from its `#[col(...)]` or `#[derived(...)]`.
#[derive(Default)]
pub struct ColSearch {
    /// `search` (the key is the column's name) or `search = "key"`.
    key: Option<Option<String>>,
    aliases: Vec<String>,
    uuid: bool,
    sort_by: Option<String>,
    is: Option<String>,
    like: bool,
}

impl ColSearch {
    /// Reads one key of the attribute; false when it is not a search key.
    pub fn parse_meta(&mut self, meta: &ParseNestedMeta) -> syn::Result<bool> {
        let text = |meta: &ParseNestedMeta| -> syn::Result<String> {
            Ok(meta.value()?.parse::<LitStr>()?.value())
        };
        if meta.path.is_ident("search") {
            self.key = Some(if meta.input.peek(Token![=]) {
                Some(text(meta)?)
            } else {
                None
            });
        } else if meta.path.is_ident("alias") {
            self.aliases.push(text(meta)?);
        } else if meta.path.is_ident("uuid") {
            self.uuid = true;
        } else if meta.path.is_ident("sort_by") {
            self.sort_by = Some(text(meta)?);
        } else if meta.path.is_ident("is") {
            self.is = Some(text(meta)?);
        } else if meta.path.is_ident("like") {
            self.like = true;
        } else {
            return Ok(false);
        }
        Ok(true)
    }

    fn any(&self) -> bool {
        self.key.is_some()
            || !self.aliases.is_empty()
            || self.uuid
            || self.sort_by.is_some()
            || self.is.is_some()
            || self.like
    }
}

pub const COL_KEYS: &str = "`search`, `alias`, `uuid`, `sort_by`, `is`, `like`";

/// `#[portable_table(search(order = "…", range = "…", qmd))]`.
pub struct TableSearch {
    span: Span,
    order: Option<LitStr>,
    range: Option<String>,
    qmd: bool,
}

pub fn parse_table_search(meta: &ParseNestedMeta) -> syn::Result<TableSearch> {
    let mut search = TableSearch {
        span: meta.path.get_ident().map_or(Span::call_site(), Ident::span),
        order: None,
        range: None,
        qmd: false,
    };
    meta.parse_nested_meta(|m| {
        if m.path.is_ident("order") {
            search.order = Some(m.value()?.parse()?);
        } else if m.path.is_ident("range") {
            search.range = Some(m.value()?.parse::<LitStr>()?.value());
        } else if m.path.is_ident("qmd") {
            search.qmd = true;
        } else {
            return Err(m.error("unknown search(...) key; supported keys: `order`, `range`, `qmd`"));
        }
        Ok(())
    })?;
    Ok(search)
}

/// One column as the enum and the search see it.
pub struct Col<'a> {
    pub name: &'a str,
    pub span: Span,
    pub search: &'a ColSearch,
}

fn variant(column: &str) -> Ident {
    let camel: String = column
        .split('_')
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or(String::new(), |first| {
                first.to_ascii_uppercase().to_string() + chars.as_str()
            })
        })
        .collect();
    Ident::new(&camel, Span::call_site())
}

pub fn expand(
    struct_name: &Ident,
    vis: &Visibility,
    table: &str,
    primary_key: &str,
    columns: &[Col],
    search: Option<TableSearch>,
) -> syn::Result<TokenStream2> {
    let enum_name = format_ident!("{struct_name}Column");
    let variants: Vec<Ident> = columns.iter().map(|c| variant(c.name)).collect();
    let names: Vec<LitStr> = columns
        .iter()
        .map(|c| LitStr::new(c.name, Span::call_site()))
        .collect();
    let doc = format!("Every column of `{table}`, the derived ones included, in DDL order.");
    let column_enum = quote! {
        #[doc = #doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #vis enum #enum_name {
            #(#variants),*
        }

        impl #enum_name {
            pub const ALL: &'static [Self] = &[#(Self::#variants),*];

            pub fn as_str(self) -> &'static str {
                match self {
                    #(Self::#variants => #names),*
                }
            }

            /// `None` for a name the table does not have.
            pub fn parse(s: &str) -> ::std::option::Option<Self> {
                match s {
                    #(#names => ::std::option::Option::Some(Self::#variants),)*
                    _ => ::std::option::Option::None,
                }
            }
        }
    };

    let Some(search) = search else {
        if let Some(c) = columns.iter().find(|c| c.search.any()) {
            return Err(syn::Error::new(
                c.span,
                format!(
                    "{} has search attributes, but {table} is not marked \
                     #[portable_table(search(...))]",
                    c.name
                ),
            ));
        }
        return Ok(column_enum);
    };
    let search_impl = expand_search(struct_name, &enum_name, table, primary_key, columns, search)?;
    Ok(quote! {
        #column_enum
        #search_impl
    })
}

fn expand_search(
    struct_name: &Ident,
    enum_name: &Ident,
    table: &str,
    primary_key: &str,
    columns: &[Col],
    search: TableSearch,
) -> syn::Result<TokenStream2> {
    // Every name an attribute gives must be a column, or the SQL built
    // from it fails at prepare time on every search.
    let column = |name: &str, span: Span| -> syn::Result<Ident> {
        if columns.iter().any(|c| c.name == name) {
            Ok(variant(name))
        } else {
            Err(syn::Error::new(
                span,
                format!("{name:?} is not a column of {table}"),
            ))
        }
    };
    if primary_key.contains(',') {
        return Err(syn::Error::new(
            search.span,
            "a searched table needs a single-column primary key to break the ties of an order",
        ));
    }
    let pk = column(primary_key.trim(), search.span)?;

    let order_lit = search.order.ok_or_else(|| {
        syn::Error::new(search.span, "search(...) needs `order = \"col desc, …\"`")
    })?;
    let mut order = Vec::new();
    for part in order_lit.value().split(',') {
        let mut words = part.split_whitespace();
        let (Some(name), dir, None) = (words.next(), words.next(), words.next()) else {
            return Err(syn::Error::new_spanned(
                &order_lit,
                format!("cannot read {part:?} as `column [asc|desc]`"),
            ));
        };
        let dir = match dir.unwrap_or("asc") {
            "asc" => quote! { ::datalib_query::table::Direction::Asc },
            "desc" => quote! { ::datalib_query::table::Direction::Desc },
            other => {
                return Err(syn::Error::new_spanned(
                    &order_lit,
                    format!("{other:?} is not asc or desc"),
                ))
            }
        };
        let v = column(name, order_lit.span())?;
        order.push(quote! { (#enum_name::#v, #dir) });
    }

    let range = match &search.range {
        Some(name) => {
            let v = column(name, search.span)?;
            quote! { ::std::option::Option::Some(#enum_name::#v) }
        }
        None => quote! { ::std::option::Option::None },
    };

    let mut keys = Vec::new();
    let mut spelled: Vec<String> = Vec::new();
    let mut flags = Vec::new();
    let mut likes = Vec::new();
    let mut sort_arms = Vec::new();
    for c in columns {
        let s = c.search;
        let v = variant(c.name);
        if let Some(key) = &s.key {
            let key = key.clone().unwrap_or_else(|| c.name.to_string());
            for word in std::iter::once(&key).chain(&s.aliases) {
                // `before:`, `after:` and `is:` are the search's own; `qmd:`
                // routes free text on a qmd table.
                if ["before", "after", "is", "qmd", "qmd_vsearch"].contains(&word.as_str())
                    || spelled.contains(word)
                {
                    return Err(syn::Error::new(
                        c.span,
                        format!("the key {word:?} is taken"),
                    ));
                }
                spelled.push(word.clone());
            }
            let aliases = &s.aliases;
            let uuid = s.uuid;
            keys.push(quote! {
                ::datalib_query::table::SearchKey {
                    key: #key,
                    aliases: &[#(#aliases),*],
                    column: #enum_name::#v,
                    uuid: #uuid,
                }
            });
        } else if !s.aliases.is_empty() || s.uuid {
            return Err(syn::Error::new(
                c.span,
                "`alias` and `uuid` describe a key: add `search`",
            ));
        }
        if let Some(word) = &s.is {
            flags.push(quote! { (#word, #enum_name::#v) });
        }
        if s.like {
            likes.push(quote! { #enum_name::#v });
        }
        if let Some(twin) = &s.sort_by {
            let t = column(twin, c.span)?;
            sort_arms.push(quote! { #enum_name::#v => #enum_name::#t, });
        }
    }
    let free_text = match (search.qmd, likes.is_empty()) {
        (true, true) => quote! { ::datalib_query::table::FreeText::Qmd },
        (false, false) => quote! { ::datalib_query::table::FreeText::Like(&[#(#likes),*]) },
        (true, false) => {
            return Err(syn::Error::new(
                search.span,
                "free text is qmd's or `like` columns', not both",
            ))
        }
        (false, true) => {
            return Err(syn::Error::new(
                search.span,
                "say what free text matches: `qmd` here, or `like` on the columns it reads",
            ))
        }
    };
    let table_lit = LitStr::new(table, Span::call_site());

    Ok(quote! {
        impl ::datalib_query::table::Column for #enum_name {
            type Table = #struct_name;

            fn as_str(self) -> &'static str {
                #enum_name::as_str(self)
            }

            fn parse(s: &str) -> ::std::option::Option<Self> {
                #enum_name::parse(s)
            }
        }

        impl ::datalib_query::table::SearchTable for #struct_name {
            type Column = #enum_name;
            const TABLE: &'static str = #table_lit;
            const KEYS: &'static [::datalib_query::table::SearchKey<#enum_name>] = &[#(#keys),*];
            const ORDER: &'static [(#enum_name, ::datalib_query::table::Direction)] = &[#(#order),*];
            const PRIMARY_KEY: #enum_name = #enum_name::#pk;
            const RANGE: ::std::option::Option<#enum_name> = #range;
            const FLAGS: &'static [(&'static str, #enum_name)] = &[#(#flags),*];
            const FREE_TEXT: ::datalib_query::table::FreeText<#enum_name> = #free_text;

            fn sorts_by(column: #enum_name) -> #enum_name {
                match column {
                    #(#sort_arms)*
                    other => other,
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_column_name_becomes_its_variant() {
        assert_eq!(variant("created_at_utc").to_string(), "CreatedAtUtc");
        assert_eq!(variant("uuid").to_string(), "Uuid");
    }
}
