//! Datalib's names: a `datalib:<kind>/<id>` link saying what something is
//! (docs/dev/plans/datalib_uris.md). `ui/src/uri.ts` is the same in the
//! UI; both read and write `ui/tests/uri-fixtures.json` alike. The `doc`,
//! `photo` and `node` kinds are the ones defined here. A `doc` link may carry its page's
//! title as words before the uuid (`datalib:doc/Ten-Forward-<uuid>`), for
//! a person to read; this reads past them and writes none.

/// A name read back, or one to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Name {
    /// A rendered page, and a section of it: a message's uuid, its
    /// `data-section-uuid`.
    Doc {
        page: String,
        section: Option<String>,
    },
    /// A photo or video, by its grid row's uuid; whichever applet says it
    /// serves `photo` hands out its pictures (docs/dev/applets.md).
    Photo { id: String },
    /// A node of a source's tree, by its `data_nodes` uuid: a channel, a
    /// label, a folder, a page (docs/dev/chips.md § "A node").
    Node { id: String },
}

const SCHEME: &str = "datalib:";
const DOC: &str = "doc/";
const PHOTO: &str = "photo/";
const NODE: &str = "node/";

impl Name {
    /// A page, at `section` when it is not the page itself.
    pub fn doc(page: &str, section: Option<&str>) -> Self {
        Name::Doc {
            page: page.to_string(),
            section: section.filter(|s| *s != page).map(str::to_string),
        }
    }

    pub fn photo(id: &str) -> Self {
        Name::Photo { id: id.to_string() }
    }

    pub fn node(id: &str) -> Self {
        Name::Node { id: id.to_string() }
    }

    pub fn write(&self) -> String {
        match self {
            Name::Doc { page, section } => match section {
                Some(section) => format!("{SCHEME}{DOC}{page}#{section}"),
                None => format!("{SCHEME}{DOC}{page}"),
            },
            Name::Photo { id } => format!("{SCHEME}{PHOTO}{id}"),
            Name::Node { id } => format!("{SCHEME}{NODE}{id}"),
        }
    }

    /// `None` for anything that is not a name this build knows, never a
    /// guess. Every id is a uuid (docs/dev/entity_ids.md): an id goes
    /// into a route, so one of any other shape (`..`) could name another.
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix(SCHEME)?;
        if let Some(id) = rest.strip_prefix(PHOTO) {
            return datalib_uuid_text::is_dashed(id).then(|| Name::photo(id));
        }
        if let Some(id) = rest.strip_prefix(NODE) {
            return datalib_uuid_text::is_dashed(id).then(|| Name::node(id));
        }
        let rest = rest.strip_prefix(DOC)?;
        let (titled, section) = rest.split_once('#').unwrap_or((rest, ""));
        let page = after_title(titled)?;
        if !datalib_uuid_text::is_dashed(page)
            || !(section.is_empty() || datalib_uuid_text::is_dashed(section))
        {
            return None;
        }
        Some(Name::Doc {
            page: page.to_string(),
            section: (!section.is_empty()).then(|| section.to_string()),
        })
    }
}

/// The uuid at the end of a page's part of the link, past any title
/// written before it: words that end in a dash and hold nothing that would
/// end the path or start another part of the link.
fn after_title(titled: &str) -> Option<&str> {
    let at = titled.len().saturating_sub(UUID_LEN);
    let (words, page) = titled.split_at_checked(at)?;
    let fits = words.is_empty() || (words.ends_with('-') && !words.contains(['/', '?', '#']));
    fits.then_some(page)
}

const UUID_LEN: usize = 36;

#[cfg(test)]
mod tests {
    use super::*;

    /// The cases the UI's `uri.ts` reads and writes too.
    const FIXTURES: &str = include_str!("../../../ui/tests/uri-fixtures.json");

    #[derive(serde::Deserialize)]
    struct Fixture {
        name: String,
        link: String,
        doc: Option<Doc>,
        #[serde(default)]
        photo: Option<String>,
        #[serde(default)]
        node: Option<String>,
        /// What the UI wrote the link with; this writes no title.
        #[serde(default)]
        title: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct Doc {
        page: String,
        section: Option<String>,
    }

    #[test]
    fn every_fixture_reads_and_writes_alike() {
        let fixtures: Vec<Fixture> = serde_json::from_str(FIXTURES).unwrap();
        for f in fixtures {
            let want = f
                .doc
                .map(|d| Name::Doc {
                    page: d.page,
                    section: d.section,
                })
                .or(f.photo.map(|id| Name::Photo { id }))
                .or(f.node.map(|id| Name::Node { id }));
            assert_eq!(Name::parse(&f.link), want, "{}: reading {}", f.name, f.link);
            if let Some(name) = want.filter(|_| f.title.is_none()) {
                assert_eq!(name.write(), f.link, "{}: writing", f.name);
            }
        }
    }

    /// A section that is the page itself adds nothing.
    #[test]
    fn a_doc_name_says_only_what_narrows_it() {
        assert_eq!(Name::doc("p", Some("p")).write(), "datalib:doc/p");
        assert_eq!(Name::doc("p", Some("m")).write(), "datalib:doc/p#m");
    }
}
