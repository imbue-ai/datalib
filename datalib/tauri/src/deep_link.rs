//! A `datalib:` link another app handed the shell: which library it is
//! for, and the address in that library's server it opens
//! (`datalib/backend/uri/README.md` § "Opening a link"). The page reads
//! the name itself; this only finds the library and the address.
//!
//! Written free of `tauri` and of every other module of the shell, so
//! `//datalib/tauri:deep_link_test` compiles it on its own.

use std::path::{Path, PathBuf};

/// A link the shell may hand on: the library it names, if any, and the
/// part of its address after the origin, `/name/<kind>/<id>` with its
/// query and fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub library: Option<String>,
    pub path: String,
    pub query: Option<String>,
    pub fragment: Option<String>,
}

const SCHEME: &str = "datalib:";
const LIBRARY: &str = "library=";

/// `None` for anything but a `datalib:<kind>/<id>` link of plain
/// characters, so nothing in it can climb out of `/name/` or carry markup
/// into an address. What the name means is the page's to read.
pub fn read(link: &str) -> Option<Link> {
    let rest = link.strip_prefix(SCHEME)?;
    let (rest, fragment) = match rest.split_once('#') {
        Some((rest, fragment)) => (rest, Some(fragment)),
        None => (rest, None),
    };
    let (name, query) = match rest.split_once('?') {
        Some((name, query)) => (name, Some(query)),
        None => (rest, None),
    };
    let (kind, id) = name.split_once('/')?;
    let plain = |s: &str, also: &[char]| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || also.contains(&c))
    };
    if !plain(kind, &['_']) || !plain(id, &[]) {
        return None;
    }
    if fragment.is_some_and(|f| !plain(f, &[])) {
        return None;
    }
    let library = match query {
        None => None,
        Some(q) => Some(q.strip_prefix(LIBRARY).filter(|id| is_uuid(id))?),
    };
    Some(Link {
        library: library.map(str::to_ascii_lowercase),
        path: format!("/name/{kind}/{id}"),
        query: query.map(str::to_string),
        fragment: fragment.map(str::to_string),
    })
}

/// The id a library's server minted for it, in `system/library-id`;
/// `None` for a library no server of this line has opened.
pub fn library_id(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("system").join("library-id")).ok()?;
    let id = text.trim();
    is_uuid(id).then(|| id.to_ascii_lowercase())
}

/// What opening a link does, given the library open now, its id, and the
/// libraries the shell knows of, in the libraries screen's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Show it in the library that is open.
    Here,
    /// Open this library to show it, once the person says so.
    Open(PathBuf),
    /// It names a library this computer does not have.
    NoSuchLibrary,
    /// It names no library, and none is open.
    NoneOpen,
}

pub fn plan(link: &Link, open: Option<&Path>, known: &[PathBuf]) -> Plan {
    let Some(wanted) = &link.library else {
        return if open.is_some() {
            Plan::Here
        } else {
            Plan::NoneOpen
        };
    };
    let is_it = |root: &Path| library_id(root).as_ref() == Some(wanted);
    if open.is_some_and(is_it) {
        return Plan::Here;
    }
    // A library copied whole keeps its id; the first the screen lists wins.
    match known
        .iter()
        .find(|root| Some(root.as_path()) != open && is_it(root))
    {
        Some(root) => Plan::Open(root.clone()),
        None => Plan::NoSuchLibrary,
    }
}

/// The address `link` opens at a server whose launch address is
/// `launch`: its origin, the link's path, query and fragment, and the
/// launch address's token, which the server turns into the page's cookie
/// and takes back out of the address.
pub fn address(launch: &str, link: &Link) -> Option<String> {
    let (origin, launch_query) = launch
        .split_once("/?")
        .unwrap_or((launch.trim_end_matches('/'), ""));
    let token = launch_query.split('&').find(|p| p.starts_with("token="));
    let query: Vec<&str> = link.query.as_deref().into_iter().chain(token).collect();
    let mut out = format!("{origin}{}", link.path);
    if !query.is_empty() {
        out.push('?');
        out.push_str(&query.join("&"));
    }
    if let Some(fragment) = &link.fragment {
        out.push('#');
        out.push_str(fragment);
    }
    origin.starts_with("http://").then_some(out)
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODE: &str = "00000000-0000-8000-a000-000000000003";
    const LIB: &str = "9b1d0000-0000-4000-8000-0000000000aa";

    fn library(dir: &Path, name: &str, id: Option<&str>) -> PathBuf {
        let root = dir.join(name);
        std::fs::create_dir_all(root.join("system")).unwrap();
        if let Some(id) = id {
            std::fs::write(root.join("system/library-id"), format!("{id}\n")).unwrap();
        }
        root
    }

    #[test]
    fn a_link_names_its_library_and_its_address() {
        let link = read(&format!(
            "datalib:doc/Ten-Forward-{NODE}?library={LIB}#{NODE}"
        ))
        .unwrap();
        assert_eq!(link.library.as_deref(), Some(LIB));
        assert_eq!(link.path, format!("/name/doc/Ten-Forward-{NODE}"));
        let at = address("http://127.0.0.1:5123/?token=abc", &link).unwrap();
        assert_eq!(
            at,
            format!(
                "http://127.0.0.1:5123/name/doc/Ten-Forward-{NODE}?library={LIB}&token=abc#{NODE}"
            )
        );
    }

    #[test]
    fn a_link_with_no_library_keeps_only_the_token() {
        let link = read(&format!("datalib:node/{NODE}")).unwrap();
        assert_eq!(link.library, None);
        assert_eq!(
            address("http://127.0.0.1:5123/?token=abc", &link).unwrap(),
            format!("http://127.0.0.1:5123/name/node/{NODE}?token=abc")
        );
    }

    /// Nothing that could climb out of `/name/`, start a second query or
    /// carry markup reaches an address.
    #[test]
    fn only_plain_links_are_handed_on() {
        for refused in [
            "datalib://chat/abc".to_string(),
            "datalib:node/..".to_string(),
            "datalib:node/a/b".to_string(),
            format!("datalib:node/{NODE}?x=1"),
            format!("datalib:node/{NODE}?library=Default"),
            format!("datalib:node/{NODE}?library={LIB}&token=x"),
            format!("datalib:doc/{NODE}#<b>"),
            format!("datalib:no%2Fde/{NODE}"),
            "mailto:riker@enterprise.org".to_string(),
            "datalib:node/".to_string(),
        ] {
            assert_eq!(read(&refused), None, "{refused}");
        }
    }

    #[test]
    fn the_library_a_link_names_is_found_by_its_id() {
        let dir = tempfile::tempdir().unwrap();
        let here = library(
            dir.path(),
            "Default",
            Some("9b1d0000-0000-4000-8000-0000000000bb"),
        );
        let there = library(dir.path(), "Work", Some(LIB));
        let unopened = library(dir.path(), "Old", None);
        let known = vec![here.clone(), unopened, there.clone()];
        let link = read(&format!("datalib:node/{NODE}?library={LIB}")).unwrap();
        assert_eq!(plan(&link, Some(&here), &known), Plan::Open(there.clone()));
        assert_eq!(plan(&link, None, &known), Plan::Open(there.clone()));
        assert_eq!(plan(&link, Some(&there), &known), Plan::Here);
        let elsewhere = read(&format!(
            "datalib:node/{NODE}?library=9b1d0000-0000-4000-8000-0000000000cc"
        ))
        .unwrap();
        assert_eq!(plan(&elsewhere, Some(&here), &known), Plan::NoSuchLibrary);
    }

    #[test]
    fn a_link_with_no_library_opens_in_the_open_one() {
        let link = read(&format!("datalib:node/{NODE}")).unwrap();
        assert_eq!(plan(&link, Some(Path::new("/x")), &[]), Plan::Here);
        assert_eq!(plan(&link, None, &[]), Plan::NoneOpen);
    }

    /// An id typed in capitals is the same library.
    #[test]
    fn a_library_id_is_read_in_either_case() {
        let dir = tempfile::tempdir().unwrap();
        let root = library(dir.path(), "Default", Some(&LIB.to_uppercase()));
        let link = read(&format!(
            "datalib:node/{NODE}?library={}",
            LIB.to_uppercase()
        ))
        .unwrap();
        assert_eq!(plan(&link, Some(&root), &[]), Plan::Here);
    }
}
