//! datalib's names: a `datalib:<kind>/<id>` link saying what something is
//! (docs/dev/plans/datalib_uris.md). `ui/src/uri.ts` is the same in the
//! UI; both read and write `ui/tests/uri-fixtures.json` alike. Only the
//! `doc` kind lives here so far.

/// A name read back, or one to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Name {
    /// A rendered page, and a section of it: a message's uuid, its
    /// `data-section-uuid`.
    Doc {
        page: String,
        section: Option<String>,
    },
}

const SCHEME: &str = "datalib:";
const DOC: &str = "doc/";

impl Name {
    /// A page, at `section` when it is not the page itself.
    pub fn doc(page: &str, section: Option<&str>) -> Self {
        Name::Doc {
            page: page.to_string(),
            section: section.filter(|s| *s != page).map(str::to_string),
        }
    }

    pub fn write(&self) -> String {
        match self {
            Name::Doc { page, section } => match section {
                Some(section) => format!("{SCHEME}{DOC}{page}#{section}"),
                None => format!("{SCHEME}{DOC}{page}"),
            },
        }
    }

    /// `None` for anything that is not a name this build knows, never a
    /// guess.
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix(SCHEME)?.strip_prefix(DOC)?;
        let (page, section) = rest.split_once('#').unwrap_or((rest, ""));
        if page.is_empty() || page.contains('/') {
            return None;
        }
        Some(Name::Doc {
            page: page.to_string(),
            section: (!section.is_empty()).then(|| section.to_string()),
        })
    }
}

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
            let want = f.doc.map(|d| Name::Doc {
                page: d.page,
                section: d.section,
            });
            assert_eq!(Name::parse(&f.link), want, "{}: reading {}", f.name, f.link);
            if let Some(name) = want {
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
