//! The frontend store: every custom component the app can render.
//!
//! One mechanism, and the filesystem is the source of truth. Under
//! `<root>/system/frontend/` sits one directory per **namespace**, and
//! a namespace holds exactly two kinds of file:
//!
//! ```text
//! system/frontend/
//!   user/                        components a person or an agent wrote
//!     9f2a1c….js                 a component, named by the sha256 of its bytes
//!     tetris.json                metadata: what `comp.user.tetris` is
//!   slack_work/                  written by the `slack_work` applet
//!     7ae808….js
//!     channels.json
//! ```
//!
//! Nothing in this module knows what an applet is. An applet's only
//! privilege is that the gateway *calls* it to write its directory
//! (see [`crate::applets`]); once the files are there they are read,
//! validated and served exactly like the ones a user dropped in by
//! hand. That is the whole point of the layout: there is no second
//! code path for "applet components", so there is nothing for the two
//! to disagree about.
//!
//! # Why the filename is the hash
//!
//! A component is addressed by content, which buys two things at once.
//! The browser keeps one module instance per resolved URL, so two
//! namespaces shipping byte-identical code resolve to the same
//! `/modules/<hash>` and are evaluated once — sharing falls out with no
//! bookkeeping. And because a name (`tetris.json`) points *at* a hash
//! rather than being a filename itself, editing a component is an
//! ordinary write of a new file plus a one-line metadata update: the
//! old bytes stay addressable for any card still mid-render, and the
//! URL changes, which is the only way a module registry that never
//! evicts will re-evaluate anything.
//!
//! # Why metadata is a separate file
//!
//! The `.js` on disk must stay byte-identical to what the browser
//! evaluates, or its name stops being a content hash. So title,
//! description and arguments live in `<name>.json` beside it rather
//! than as frontmatter.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::sha256_hex;

/// `<root>/system/frontend` — the parent of every namespace directory.
pub fn frontend_dir(data_root: &Path) -> PathBuf {
    datalib_core::layout::system_dir(data_root).join("frontend")
}

/// The namespace holding components the app itself never regenerates.
/// A refresh wipes every *other* namespace directory, so this name is
/// refused to applets at config load
/// ([`datalib_dag::config::RESERVED_APPLET_ID`]).
pub const USER_NAMESPACE: &str = "user";

// ---------------------------------------------------------------------------
// On-disk metadata
// ---------------------------------------------------------------------------

/// What a `<name>.json` says.
///
/// Untagged, with [`Meta::Component`] first: a component document has
/// required fields a rename document lacks, so a rename can only match
/// the second arm. Anything matching neither is reported rather than
/// silently ignored — a typo in a metadata file should be visible, not
/// a component that quietly stops existing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Meta {
    Component {
        title: String,
        #[serde(default)]
        description: String,
        /// sha256 of the component's bytes — the `<hash>.js` in this
        /// same namespace, and the last segment of its
        /// `/modules/<hash>` URL.
        component_hash: String,
        /// Arguments the gallery entry passes. Serialized as JSON
        /// literals into the constructed call, so
        /// `["slack_work"]` yields `comp.<ns>.<name>("slack_work")`.
        #[serde(default)]
        component_args: Vec<serde_json::Value>,
    },
    /// This name no longer holds a component; it moved, within this
    /// same namespace. The UI follows the chain to repoint cards.
    Renamed { renamed_to: String },
}

// ---------------------------------------------------------------------------
// What the UI is told
// ---------------------------------------------------------------------------

/// One namespace as `GET /api/frontend` reports it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct NamespaceView {
    /// `<name>` → its metadata, for every well-formed `<name>.json`.
    pub entries: BTreeMap<String, Meta>,
    /// Files this namespace could not use, each with why. Surfaced
    /// rather than dropped: a component that silently fails to appear
    /// looks identical to one that was never written.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

/// The whole store: namespaces plus the content map behind
/// `/modules/<hash>`.
#[derive(Debug, Default)]
pub struct FrontendStore {
    pub namespaces: BTreeMap<String, NamespaceView>,
    /// hash → the file holding those bytes. Built across *all*
    /// namespaces, so identical code in two of them is one entry and
    /// therefore one URL.
    content: BTreeMap<String, PathBuf>,
}

/// A cheap fingerprint of the frontend tree, for deciding whether a
/// rescan is needed.
///
/// Directory mtimes catch a file appearing or disappearing; the
/// per-metadata-file size and mtime catch a `<name>.json` rewritten in
/// place, which does not touch its directory's mtime. Component files
/// need no entry of their own: they are named by their content, so new
/// bytes always mean a new filename, which the directory mtime sees.
///
/// Deliberately `stat`-only — no file is read — so this can sit on a
/// polled endpoint.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StoreStamp(Vec<(String, u64, Option<std::time::SystemTime>)>);

impl StoreStamp {
    pub fn of(data_root: &Path) -> Self {
        let root = frontend_dir(data_root);
        let mut out: Vec<(String, u64, Option<std::time::SystemTime>)> = Vec::new();
        let Ok(rd) = std::fs::read_dir(&root) else {
            return Self(out);
        };
        for ent in rd.flatten() {
            let dir = ent.path();
            if !dir.is_dir() {
                continue;
            }
            let Some(ns) = dir.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let md = std::fs::metadata(&dir).ok();
            out.push((
                ns.to_string(),
                md.as_ref().map(|m| m.len()).unwrap_or(0),
                md.and_then(|m| m.modified().ok()),
            ));
            let Ok(inner) = std::fs::read_dir(&dir) else {
                continue;
            };
            for f in inner.flatten() {
                let path = f.path();
                let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                if !name.ends_with(".json") {
                    continue;
                }
                let md = f.metadata().ok();
                out.push((
                    format!("{ns}/{name}"),
                    md.as_ref().map(|m| m.len()).unwrap_or(0),
                    md.and_then(|m| m.modified().ok()),
                ));
            }
        }
        out.sort();
        Self(out)
    }
}

impl FrontendStore {
    /// Scan `<root>/system/frontend`. Never fails: an unreadable
    /// namespace becomes an empty one with a problem recorded, because
    /// the app has to keep serving whatever else is well-formed.
    pub fn scan(data_root: &Path) -> Self {
        let root = frontend_dir(data_root);
        let mut store = Self::default();
        let Ok(rd) = std::fs::read_dir(&root) else {
            // No store yet is the normal state of a fresh data root.
            return store;
        };
        for ent in rd.flatten() {
            let path = ent.path();
            if !path.is_dir() {
                continue;
            }
            let Some(ns) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let view = store.scan_namespace(ns, &path);
            store.namespaces.insert(ns.to_string(), view);
        }
        store
    }

    fn scan_namespace(&mut self, ns: &str, dir: &Path) -> NamespaceView {
        let mut view = NamespaceView::default();
        let mut metas: Vec<(String, PathBuf)> = Vec::new();

        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) => {
                view.problems.push(format!("{}: {e}", dir.display()));
                return view;
            }
        };
        for ent in rd.flatten() {
            let path = ent.path();
            let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Some(stem) = fname.strip_suffix(".js") {
                // The filename is a claim about the bytes. Check it:
                // an unvalidated claim would let a stale build serve
                // old code forever from a URL that promises to be
                // immutable, and nothing downstream could detect it.
                if !is_sha256_hex(stem) {
                    view.problems
                        .push(format!("{fname}: not named by a sha256 digest, skipped"));
                    continue;
                }
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(e) => {
                        view.problems.push(format!("{fname}: {e}"));
                        continue;
                    }
                };
                let actual = sha256_hex(&bytes);
                if actual != stem {
                    view.problems
                        .push(format!("{fname}: contents hash to {actual}, skipped"));
                    continue;
                }
                // Same bytes in two namespaces is one entry, hence one
                // URL, hence one evaluated module in the browser.
                self.content.insert(actual, path);
            } else if let Some(stem) = fname.strip_suffix(".json") {
                metas.push((stem.to_string(), path));
            }
        }

        // Metadata is resolved after the whole directory is scanned, so
        // a `<name>.json` may name a component regardless of the order
        // `read_dir` happened to return them in.
        for (name, path) in metas {
            if !valid_name(&name) {
                view.problems
                    .push(format!("{name}.json: not a valid identifier, skipped"));
                continue;
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    view.problems.push(format!("{name}.json: {e}"));
                    continue;
                }
            };
            let meta: Meta = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(e) => {
                    view.problems.push(format!("{name}.json: {e}"));
                    continue;
                }
            };
            if let Meta::Component { component_hash, .. } = &meta {
                if !self.content.contains_key(component_hash) {
                    view.problems.push(format!(
                        "{name}.json: names component {component_hash}, \
                         which is not in namespace {ns}"
                    ));
                    continue;
                }
            }
            view.entries.insert(name, meta);
        }
        view
    }

    /// Read a component's bytes by hash. The shape is checked before
    /// the value is used as a key, so a request path can never reach
    /// outside the store.
    pub fn read_component(&self, hash: &str) -> Option<Vec<u8>> {
        if !is_sha256_hex(hash) {
            return None;
        }
        std::fs::read(self.content.get(hash)?).ok()
    }

    /// Every namespace's view, for `GET /api/frontend`.
    pub fn view(&self) -> &BTreeMap<String, NamespaceView> {
        &self.namespaces
    }
}

/// Guard before a hash is used as a key or a path segment. Lowercase
/// hex only — the same predicate the card store applies.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A component name reaches card source as `comp.<ns>.<name>`, so it
/// has to be a JavaScript identifier — which also keeps it safe as a
/// filename (no separators, no traversal).
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let first_ok =
        matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$');
    first_ok
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    /// A component and its metadata, the ordinary case.
    #[test]
    fn reads_a_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let ns = frontend_dir(tmp.path()).join("user");
        let body = "export default () => () => {};";
        let hash = sha256_hex(body.as_bytes());
        write(&ns, &format!("{hash}.js"), body);
        write(
            &ns,
            "tetris.json",
            &format!(
                r#"{{"title":"Tetris","description":"A game.","component_hash":"{hash}","component_args":[]}}"#
            ),
        );

        let store = FrontendStore::scan(tmp.path());
        let user = &store.namespaces["user"];
        assert!(user.problems.is_empty(), "{:?}", user.problems);
        match &user.entries["tetris"] {
            Meta::Component {
                title,
                component_hash,
                component_args,
                ..
            } => {
                assert_eq!(title, "Tetris");
                assert_eq!(component_hash, &hash);
                assert!(component_args.is_empty());
            }
            other => panic!("expected a component, got {other:?}"),
        }
        assert_eq!(store.read_component(&hash).unwrap(), body.as_bytes());
    }

    /// The rename arm has to win for a document that carries only
    /// `renamed_to`, even though the component arm is tried first.
    #[test]
    fn reads_a_rename_tombstone() {
        let tmp = tempfile::tempdir().unwrap();
        let ns = frontend_dir(tmp.path()).join("user");
        write(&ns, "old.json", r#"{"renamed_to":"new"}"#);
        let store = FrontendStore::scan(tmp.path());
        match &store.namespaces["user"].entries["old"] {
            Meta::Renamed { renamed_to } => assert_eq!(renamed_to, "new"),
            other => panic!("expected a rename, got {other:?}"),
        }
    }

    /// The filename is a claim about the bytes; an unchecked claim
    /// would serve stale code from a URL that promises immutability.
    #[test]
    fn skips_a_file_whose_name_lies_about_its_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let ns = frontend_dir(tmp.path()).join("user");
        let lie = "0".repeat(64);
        write(&ns, &format!("{lie}.js"), "export default 1;");
        write(
            &ns,
            "x.json",
            &format!(r#"{{"title":"X","component_hash":"{lie}"}}"#),
        );

        let store = FrontendStore::scan(tmp.path());
        let user = &store.namespaces["user"];
        assert!(store.read_component(&lie).is_none());
        // Both the bad file and the now-dangling metadata are reported.
        assert_eq!(user.problems.len(), 2, "{:?}", user.problems);
        assert!(user.entries.is_empty());
    }

    #[test]
    fn skips_a_js_file_not_named_by_a_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let ns = frontend_dir(tmp.path()).join("user");
        write(&ns, "tetris.js", "export default 1;");
        let store = FrontendStore::scan(tmp.path());
        let p = &store.namespaces["user"].problems;
        assert_eq!(p.len(), 1);
        assert!(p[0].contains("sha256"), "{p:?}");
    }

    /// Metadata pointing at code that isn't there is a problem, not a
    /// half-registered component.
    #[test]
    fn reports_metadata_with_no_component() {
        let tmp = tempfile::tempdir().unwrap();
        let ns = frontend_dir(tmp.path()).join("user");
        write(
            &ns,
            "ghost.json",
            &format!(
                r#"{{"title":"Ghost","component_hash":"{}"}}"#,
                "a".repeat(64)
            ),
        );
        let store = FrontendStore::scan(tmp.path());
        let user = &store.namespaces["user"];
        assert!(user.entries.is_empty());
        assert_eq!(user.problems.len(), 1, "{:?}", user.problems);
    }

    #[test]
    fn reports_unparseable_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let ns = frontend_dir(tmp.path()).join("user");
        write(&ns, "bad.json", "{not json");
        let store = FrontendStore::scan(tmp.path());
        assert_eq!(store.namespaces["user"].problems.len(), 1);
    }

    /// Byte-identical components in two namespaces share one content
    /// entry, which is what makes them share one URL — and therefore
    /// one evaluated module — in the browser.
    #[test]
    fn identical_components_across_namespaces_share_one_address() {
        let tmp = tempfile::tempdir().unwrap();
        let body = "export default (id) => () => {};";
        let hash = sha256_hex(body.as_bytes());
        for ns in ["slack_work", "slack_personal"] {
            let dir = frontend_dir(tmp.path()).join(ns);
            write(&dir, &format!("{hash}.js"), body);
            write(
                &dir,
                "channels.json",
                &format!(
                    r#"{{"title":"Channels","component_hash":"{hash}","component_args":["{ns}"]}}"#
                ),
            );
        }
        let store = FrontendStore::scan(tmp.path());
        assert_eq!(store.content.len(), 1, "one address for identical bytes");
        // …and each namespace still carries its own arguments.
        for ns in ["slack_work", "slack_personal"] {
            match &store.namespaces[ns].entries["channels"] {
                Meta::Component { component_args, .. } => {
                    assert_eq!(component_args[0], serde_json::json!(ns));
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn a_missing_store_is_empty_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(FrontendStore::scan(tmp.path()).namespaces.is_empty());
    }

    #[test]
    fn component_paths_cannot_escape_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FrontendStore::scan(tmp.path());
        for bad in ["../../etc/passwd", "not-a-hash", &"A".repeat(64)] {
            assert!(store.read_component(bad).is_none(), "{bad}");
        }
    }
}
