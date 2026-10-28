//! datalib's names: a `datalib:<kind>/<id>` link saying what something is
//! (docs/dev/plans/datalib_uris.md). `ui/src/uri.ts` is the same in the
//! UI; both read and write `ui/tests/uri-fixtures.json` alike. Only the
//! `doc` kind lives here so far.

/// A name read back, or one to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Name {
    /// A rendered page, a section of it (a message's uuid, its
    /// `data-section-uuid`), and words to find inside that section.
    Doc {
        page: String,
        section: Option<String>,
        text: Option<String>,
    },
}

const SCHEME: &str = "datalib:";
const DOC: &str = "doc/";
/// The web's text-fragment directive, which a section may carry.
const TEXT: &str = ":~:text=";

impl Name {
    /// A page, at `section` when it is not the page itself, at `text`
    /// within it when there is any.
    pub fn doc(page: &str, section: Option<&str>, text: Option<&str>) -> Self {
        Name::Doc {
            page: page.to_string(),
            section: section.filter(|s| *s != page).map(str::to_string),
            text: text.filter(|t| !t.trim().is_empty()).map(str::to_string),
        }
    }

    pub fn write(&self) -> String {
        match self {
            Name::Doc {
                page,
                section,
                text,
            } => {
                let mut out = format!("{SCHEME}{DOC}{page}");
                if section.is_some() || text.is_some() {
                    out.push('#');
                    out.push_str(section.as_deref().unwrap_or_default());
                }
                if let Some(text) = text {
                    out.push_str(TEXT);
                    out.push_str(&percent_encode(text));
                }
                out
            }
        }
    }

    /// `None` for anything that is not a name this build knows, never a
    /// guess.
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix(SCHEME)?.strip_prefix(DOC)?;
        let (page, fragment) = rest.split_once('#').unwrap_or((rest, ""));
        if page.is_empty() || page.contains('/') {
            return None;
        }
        let (section, text) = match fragment.split_once(TEXT) {
            Some((section, text)) => (section, Some(percent_decode(text)?)),
            None => (fragment, None),
        };
        Some(Name::Doc {
            page: page.to_string(),
            section: (!section.is_empty()).then(|| section.to_string()),
            text: text.filter(|t| !t.is_empty()),
        })
    }
}

/// Everything but the unreserved characters, as `encodeURIComponent`
/// leaves them, so the UI writes the same bytes.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
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
        text: Option<String>,
    }

    #[test]
    fn every_fixture_reads_and_writes_alike() {
        let fixtures: Vec<Fixture> = serde_json::from_str(FIXTURES).unwrap();
        for f in fixtures {
            let want = f.doc.map(|d| Name::Doc {
                page: d.page,
                section: d.section,
                text: d.text,
            });
            assert_eq!(Name::parse(&f.link), want, "{}: reading {}", f.name, f.link);
            if let Some(name) = want {
                assert_eq!(name.write(), f.link, "{}: writing", f.name);
            }
        }
    }

    /// A section that is the page itself, or blank words, add nothing.
    #[test]
    fn a_doc_name_says_only_what_narrows_it() {
        assert_eq!(
            Name::doc("p", Some("p"), Some(" ")).write(),
            "datalib:doc/p"
        );
        assert_eq!(Name::doc("p", Some("m"), None).write(), "datalib:doc/p#m");
    }
}
