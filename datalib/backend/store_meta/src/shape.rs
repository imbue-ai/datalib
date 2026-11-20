//! The one check every open of a store makes before it reads a row: the
//! shape the store's `_datalib_meta` records is the shape this build
//! writes, or the open fails naming both. Nothing past the check reads
//! around a table, a column or an index an older build left out
//! (`README.md` beside this crate).

use std::path::{Path, PathBuf};

use crate::{Meta, Versions};

/// What a store is made of, as its owner records it: its tables, its
/// indexes and where it stands on its ladders. Two stores with one shape
/// answer every statement this build writes the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    /// blake3 over the DDL of its tables (`schema_hash`).
    pub tables: String,
    /// blake3 over the indexes its owner keeps out of `tables`, so adding
    /// one costs creating it rather than rebuilding the store
    /// (`index_hash`). `None` for a store whose every index is in
    /// `tables`.
    pub indexes: Option<String>,
    pub versions: Versions,
}

impl Shape {
    /// A store whose every index is in its table DDL, on no ladder.
    pub fn of_tables(tables: String) -> Self {
        Self {
            tables,
            indexes: None,
            versions: Versions::default(),
        }
    }
}

impl Meta {
    /// The shape this store records.
    pub fn shape(&self) -> Shape {
        Shape {
            tables: self.schema_hash.clone(),
            indexes: self.index_hash.clone(),
            versions: Versions {
                schema: self.schema_version,
                shared: self.shared_schema_version,
            },
        }
    }
}

/// A store this build does not read: the shape it records is not the
/// one this build writes. Its step brings it to this build's shape when
/// it runs; the prepare pass at launch asks for that run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherShape {
    pub store: PathBuf,
    /// `None`: the store records no shape at all.
    pub have: Option<Shape>,
    pub want: Shape,
    /// The `datalib_version` that wrote it, when it says.
    pub written_by: Option<String>,
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

fn describe(s: &Shape) -> String {
    let indexes = s
        .indexes
        .as_deref()
        .map(|i| format!(", indexes {}", short(i)))
        .unwrap_or_default();
    format!(
        "tables {}{indexes}, ladder v{}/shared v{}",
        short(&s.tables),
        s.versions.schema,
        s.versions.shared
    )
}

/// The step that writes `store`: its `<group>/<function>` folder, which is
/// how every step's tree is laid out under a data root.
fn step_of(store: &Path) -> Option<String> {
    let function = store.parent()?;
    let group = function.parent()?;
    Some(format!(
        "{}/{}",
        group.file_name()?.to_str()?,
        function.file_name()?.to_str()?
    ))
}

impl OtherShape {
    /// The step to run to bring the store to this build's shape.
    pub fn step(&self) -> Option<String> {
        step_of(&self.store)
    }
}

impl std::fmt::Display for OtherShape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let have = match &self.have {
            Some(have) => describe(have),
            None => "no recorded shape".to_string(),
        };
        let by = self
            .written_by
            .as_deref()
            .map(|v| format!(", written by Datalib {v}"))
            .unwrap_or_default();
        write!(
            f,
            "{} is not in the shape this build reads ({have}{by}; this build reads {}), \
             so nothing reads it until it is brought up to date. ",
            self.store.display(),
            describe(&self.want),
        )?;
        match self.step() {
            Some(step) => write!(
                f,
                "Run `{step}` to do that from what is already downloaded: accept the \
                 app's offer to re-run it, sync it from the Manage screen, or run \
                 `datalib-pipeline sync <data_root> {step}`."
            ),
            None => write!(f, "Run the step that writes it."),
        }
    }
}

impl std::error::Error for OtherShape {}

/// The check. `meta` is what `store` records, `None` when it records
/// nothing; `want` is the shape this build writes it in. Refuses with an
/// [`OtherShape`], which [`other_shape`] finds again.
pub fn require(store: &Path, meta: Option<&Meta>, want: &Shape) -> anyhow::Result<()> {
    let have = meta.map(Meta::shape);
    if have.as_ref() == Some(want) {
        return Ok(());
    }
    Err(anyhow::Error::new(OtherShape {
        store: store.to_path_buf(),
        have,
        want: want.clone(),
        written_by: meta.map(|m| m.datalib_version.clone()),
    }))
}

/// The refusal anywhere in `e`'s chain, when `e` is a store refused for
/// its shape.
pub fn other_shape(e: &anyhow::Error) -> Option<&OtherShape> {
    e.chain().find_map(|c| c.downcast_ref::<OtherShape>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(tables: &str, indexes: Option<&str>, schema: u32) -> Meta {
        Meta {
            datalib_version: "0.40.0".into(),
            git_hash: None,
            doltlite_version: None,
            schema_hash: tables.into(),
            index_hash: indexes.map(Into::into),
            schema_version: schema,
            shared_schema_version: 1,
            store_kind: None,
            written_at_utc: String::new(),
        }
    }

    fn want() -> Shape {
        Shape {
            tables: "t".into(),
            indexes: Some("i".into()),
            versions: Versions {
                schema: 2,
                shared: 1,
            },
        }
    }

    /// The whole shape must match: tables, indexes and both ladders. A
    /// store that records nothing is refused, not let through.
    #[test]
    fn any_part_of_the_shape_moving_refuses_the_store() {
        let store = Path::new("/r/s.doltlite_db");
        require(store, Some(&meta("t", Some("i"), 2)), &want()).unwrap();
        for other in [
            meta("older", Some("i"), 2),
            meta("t", None, 2),
            meta("t", Some("fewer"), 2),
            meta("t", Some("i"), 1),
        ] {
            let e = require(store, Some(&other), &want()).unwrap_err();
            let refused = other_shape(&e).expect("an OtherShape");
            assert_eq!(refused.have, Some(other.shape()));
            assert_eq!(refused.written_by.as_deref(), Some("0.40.0"));
        }
        let e = require(store, None, &want()).unwrap_err();
        let bare = other_shape(&e).unwrap();
        assert_eq!(bare.have, None);
        let said = bare.to_string();
        assert!(said.contains("/r/s.doltlite_db"), "{said}");
        assert!(said.contains("no recorded shape"), "{said}");
        assert_eq!(bare.step(), None);
        let named = require(
            Path::new("/root/slack/render_markdown/s.doltlite_db"),
            None,
            &want(),
        )
        .unwrap_err();
        let named = other_shape(&named).unwrap();
        assert_eq!(named.step().as_deref(), Some("slack/render_markdown"));
        let said = named.to_string();
        assert!(
            said.contains("datalib-pipeline sync <data_root> slack/render_markdown"),
            "the refusal says what to run: {said}"
        );
    }

    /// A refusal is found however deep a caller wrapped it.
    #[test]
    fn a_refusal_is_found_under_context() {
        let e = require(Path::new("s"), None, &want())
            .unwrap_err()
            .context("open the render store")
            .context("index the source");
        assert!(other_shape(&e).is_some());
        assert!(other_shape(&anyhow::anyhow!("no such table: grid_rows")).is_none());
    }
}
