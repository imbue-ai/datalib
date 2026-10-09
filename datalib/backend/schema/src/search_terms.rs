// What a grid row answers to beyond its own columns' search keys: its ids,
// the people it names, its title, its names. One row of the search terms
// file per term, so a pasted id or address is one lookup, whatever column
// holds it. The file, and why it is plain SQLite beside the grid index
// rather than a table in it: `docs/dev/plans/search_tabs.md` §
// "The search terms".

/// What a term is to its row. Stored as its [`code`](SearchTermKind::code).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[strum(serialize_all = "snake_case")]
#[repr(u8)]
pub enum SearchTermKind {
    /// The row's own uuid.
    Id = 1,
    /// The uuid of something the row is in: its conversation, its
    /// document, its Notion page.
    Container = 2,
    /// The handle of the row's author.
    From = 3,
    /// The title of the row's conversation or document.
    Title = 4,
    /// A name the row shows: its author, its channel, its account.
    Name = 5,
    /// The handle of someone the row was addressed to: an email's To.
    To = 6,
    /// The handle of someone copied on the row: an email's Cc.
    Cc = 7,
    /// Where the row is filed upstream: an email's mailboxes and labels.
    Label = 8,
}

impl SearchTermKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }

    /// What the search terms file stores. A code, once given, keeps its kind.
    pub fn code(self) -> u8 {
        self as u8
    }

    /// `None` for a code this build does not know.
    pub fn from_code(code: i64) -> Option<Self> {
        use strum::VariantArray;
        Self::VARIANTS
            .iter()
            .copied()
            .find(|k| i64::from(k.code()) == code)
    }

    /// How strongly a match in this kind says the row is the one meant:
    /// its own id beats its author or addressee, which beat someone
    /// copied and what contains it, then its title and labels, and last a
    /// name it shows.
    pub fn affinity(self) -> u8 {
        match self {
            SearchTermKind::Id => 5,
            SearchTermKind::From | SearchTermKind::To => 4,
            SearchTermKind::Cc | SearchTermKind::Container => 3,
            SearchTermKind::Title | SearchTermKind::Label => 2,
            SearchTermKind::Name => 1,
        }
    }

    /// Whether the value is a person's handle, in some role on the row.
    pub fn is_person(self) -> bool {
        match self {
            SearchTermKind::From | SearchTermKind::To | SearchTermKind::Cc => true,
            SearchTermKind::Id
            | SearchTermKind::Container
            | SearchTermKind::Title
            | SearchTermKind::Name
            | SearchTermKind::Label => false,
        }
    }
}

/// A term a render supplies for one of its rows, beyond what
/// [`search_terms_of`] derives from the row's columns: who it was
/// addressed to, where it is filed. Stored in the render store and the
/// grid index as a `supplied_search_terms` row
/// (`crate::supplied_search_terms`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppliedSearchTerm {
    pub uuid: String,
    pub kind: SearchTermKind,
    pub value: String,
}

/// The columns of a `grid_rows` row its terms come from.
#[derive(Debug, Clone, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct SearchTermSource {
    pub uuid: String,
    pub conversation_uuid: String,
    pub markdown_uuid: Option<String>,
    pub notion_page_uuid: Option<String>,
    pub author_handle: Option<String>,
    pub conversation_name: Option<String>,
    pub author: Option<String>,
    pub channel: Option<String>,
    pub account: Option<String>,
    pub touched_at_utc: Option<String>,
}

impl SearchTermSource {
    /// The `SELECT` list that reads one from `grid_rows`.
    pub const COLUMNS: &'static str = "uuid, conversation_uuid, markdown_uuid, notion_page_uuid, \
         author_handle, conversation_name, author, channel, account, touched_at_utc";
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchTerm {
    pub kind: SearchTermKind,
    pub value: String,
}

/// Every term `row` answers to, each `(kind, value)` once and in kind
/// order. An id is a `Container` only when it is not the row's own, and
/// an empty value is no term.
pub fn search_terms_of(row: &SearchTermSource) -> Vec<SearchTerm> {
    let containers = [
        Some(row.conversation_uuid.as_str()),
        row.markdown_uuid.as_deref(),
        row.notion_page_uuid.as_deref(),
    ];
    let names = [
        row.author.as_deref(),
        row.channel.as_deref(),
        row.account.as_deref(),
    ];
    let candidates = std::iter::once((SearchTermKind::Id, Some(row.uuid.as_str())))
        .chain(
            containers
                .into_iter()
                .filter(|id| *id != Some(row.uuid.as_str()))
                .map(|id| (SearchTermKind::Container, id)),
        )
        .chain(std::iter::once((
            SearchTermKind::From,
            row.author_handle.as_deref(),
        )))
        .chain(std::iter::once((
            SearchTermKind::Title,
            row.conversation_name.as_deref(),
        )))
        .chain(names.into_iter().map(|name| (SearchTermKind::Name, name)));
    let mut out: Vec<SearchTerm> = Vec::new();
    for (kind, value) in candidates {
        let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
            continue;
        };
        if !out.iter().any(|t| t.kind == kind && t.value == value) {
            out.push(SearchTerm {
                kind,
                value: value.to_string(),
            });
        }
    }
    out
}

/// What [`search_terms_of`] derives, which kinds renders supply, and how
/// the file lays it out. A file built under another shape is rebuilt
/// whole, so change it whenever any of them changes.
pub const TERMS_SHAPE: &str = "3";

/// The search terms file's tables, dictionary-encoded: each grid row once in
/// `rows`, each distinct value once in `vals`, and a term is three
/// integers. The FTS5 index covers `vals` alone, linked by rowid, and
/// keeps `@ . - _ + : /` inside a token so an id or a handle is one
/// token (`docs/dev/doltlite.md` § "Full-text search (FTS5)").
pub const TERMS_DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS rows (row_id INTEGER PRIMARY KEY, \
     uuid TEXT NOT NULL UNIQUE, touched_at_utc TEXT)",
    "CREATE TABLE IF NOT EXISTS vals (val_id INTEGER PRIMARY KEY, value TEXT NOT NULL UNIQUE)",
    "CREATE TABLE IF NOT EXISTS terms (val_id INTEGER NOT NULL, kind INTEGER NOT NULL, \
     row_id INTEGER NOT NULL, PRIMARY KEY (val_id, kind, row_id)) WITHOUT ROWID",
    "CREATE INDEX IF NOT EXISTS terms_by_row ON terms (row_id)",
    "CREATE VIRTUAL TABLE IF NOT EXISTS vals_fts USING fts5(value, content='', \
     contentless_delete=1, tokenize=\"unicode61 tokenchars '@.-_+:/'\")",
    "CREATE TABLE IF NOT EXISTS terms_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
];

/// The `terms_meta` key naming the grid index commit the terms reflect.
pub const META_GRID_COMMIT: &str = "grid_commit";
/// The `terms_meta` key naming the [`TERMS_SHAPE`] the file was built under.
pub const META_SHAPE: &str = "shape";

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> SearchTermSource {
        SearchTermSource {
            uuid: "m-1".into(),
            conversation_uuid: "c-1".into(),
            markdown_uuid: Some("c-1".into()),
            author_handle: Some("email:ann@example.com".into()),
            conversation_name: Some("Away team roster".into()),
            author: Some("Ann".into()),
            channel: Some("".into()),
            ..SearchTermSource::default()
        }
    }

    fn pairs(terms: &[SearchTerm]) -> Vec<(&str, &str)> {
        terms
            .iter()
            .map(|t| (t.kind.as_str(), t.value.as_str()))
            .collect()
    }

    #[test]
    fn a_message_answers_to_its_id_its_conversation_its_author_and_its_title() {
        assert_eq!(
            pairs(&search_terms_of(&row())),
            [
                ("id", "m-1"),
                ("container", "c-1"),
                ("from", "email:ann@example.com"),
                ("title", "Away team roster"),
                ("name", "Ann"),
            ]
        );
    }

    /// A conversation's own row is its conversation and its document: one
    /// id, not three.
    #[test]
    fn a_document_row_is_not_its_own_container() {
        let doc = SearchTermSource {
            uuid: "c-1".into(),
            ..row()
        };
        let terms = search_terms_of(&doc);
        assert_eq!(
            terms.iter().filter(|t| t.value == "c-1").count(),
            1,
            "{terms:?}"
        );
        assert_eq!(terms[0].kind, SearchTermKind::Id);
    }

    #[test]
    fn every_kind_reads_back_by_its_spelling_and_its_code() {
        use strum::VariantArray;
        for kind in SearchTermKind::VARIANTS {
            assert_eq!(SearchTermKind::parse(kind.as_str()), Some(*kind));
            assert_eq!(
                SearchTermKind::from_code(i64::from(kind.code())),
                Some(*kind)
            );
        }
        assert_eq!(SearchTermKind::parse("reactor"), None);
        assert_eq!(SearchTermKind::from_code(0), None);
    }

    /// A stored code is a promise: a kind keeps its number, or every file
    /// written before reads its terms as another kind.
    #[test]
    fn the_codes_are_the_ones_files_hold() {
        let codes: Vec<(SearchTermKind, u8)> = [
            SearchTermKind::Id,
            SearchTermKind::Container,
            SearchTermKind::From,
            SearchTermKind::Title,
            SearchTermKind::Name,
            SearchTermKind::To,
            SearchTermKind::Cc,
            SearchTermKind::Label,
        ]
        .into_iter()
        .map(|k| (k, k.code()))
        .collect();
        assert_eq!(
            codes,
            [
                (SearchTermKind::Id, 1),
                (SearchTermKind::Container, 2),
                (SearchTermKind::From, 3),
                (SearchTermKind::Title, 4),
                (SearchTermKind::Name, 5),
                (SearchTermKind::To, 6),
                (SearchTermKind::Cc, 7),
                (SearchTermKind::Label, 8),
            ]
        );
    }
}
