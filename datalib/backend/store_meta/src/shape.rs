//! The one check every open of a store makes before it reads a row: the
//! shape the store's `_datalib_meta` records is the shape this build
//! writes, or the open fails naming the store and the step to run
//! (`README.md` beside this crate).

use std::path::{Path, PathBuf};

use crate::{Meta, Versions};

/// What a store is made of, as its owner records it: its tables
/// (`schema_hash`), the indexes it keeps apart from them (`index_hash`, so
/// adding one costs making it rather than a rebuild) and where it stands
/// on its ladders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    pub tables: String,
    pub indexes: Option<String>,
    pub versions: Versions,
}

impl Shape {
    /// A store whose every index is in its tables, on no ladder.
    pub fn of_tables(tables: String) -> Self {
        Self {
            tables,
            indexes: None,
            versions: Versions::default(),
        }
    }
}

impl Meta {
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

/// A store this build does not read, and what brings it to this build's
/// shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherShape {
    pub store: PathBuf,
    /// `None`: it records no shape.
    pub have: Option<Shape>,
    pub want: Shape,
    /// The step whose run rewrites it, when one does.
    pub step: Option<String>,
}

impl std::fmt::Display for OtherShape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let had = if self.have.is_some() {
            "another shape"
        } else {
            "no recorded shape"
        };
        write!(
            f,
            "{} is in {had} than this build reads, so nothing reads it until it is \
             brought up to date. ",
            self.store.display()
        )?;
        match &self.step {
            Some(step) => write!(
                f,
                "Run `{step}` to do that from what is already downloaded: the app runs \
                 it after an upgrade, or run `datalib-pipeline sync <data_root> {step}`."
            ),
            None => write!(
                f,
                "The program that writes it does that when it next opens it."
            ),
        }
    }
}

impl std::error::Error for OtherShape {}

/// The step whose tree holds `store`: its `<group>/<function>` folder.
pub fn step_of(store: &Path) -> Option<String> {
    let function = store.parent()?;
    let group = function.parent()?.file_name()?.to_str()?;
    Some(format!("{group}/{}", function.file_name()?.to_str()?))
}

/// The check: `meta` is what `store` records; `want` is the shape this
/// build writes it in; `step` is what to run when they differ. Refuses
/// with an [`OtherShape`], which [`other_shape`] finds again.
pub fn require(
    store: &Path,
    step: Option<&str>,
    meta: Option<&Meta>,
    want: &Shape,
) -> anyhow::Result<()> {
    require_recorded(store, step, meta.map(Meta::shape), want)
}

/// [`require`] for a shape the caller read out of the store's meta
/// itself: one part of it, for a reader that reads only that part.
pub fn require_recorded(
    store: &Path,
    step: Option<&str>,
    have: Option<Shape>,
    want: &Shape,
) -> anyhow::Result<()> {
    if have.as_ref() == Some(want) {
        return Ok(());
    }
    Err(anyhow::Error::new(OtherShape {
        store: store.to_path_buf(),
        have,
        want: want.clone(),
        step: step.map(str::to_string),
    }))
}

/// The refusal anywhere in `e`'s chain.
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
            parts: Default::default(),
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
    /// store that records nothing is refused, not let through, and the
    /// refusal says what to run, however deep a caller wrapped it.
    #[test]
    fn any_part_of_the_shape_moving_refuses_the_store() {
        let store = Path::new("/root/slack/render_markdown/s.doltlite_db");
        let step = step_of(store);
        assert_eq!(step.as_deref(), Some("slack/render_markdown"));
        require(
            store,
            step.as_deref(),
            Some(&meta("t", Some("i"), 2)),
            &want(),
        )
        .unwrap();
        for other in [
            meta("older", Some("i"), 2),
            meta("t", None, 2),
            meta("t", Some("fewer"), 2),
            meta("t", Some("i"), 1),
        ] {
            let e = require(store, None, Some(&other), &want()).unwrap_err();
            assert_eq!(other_shape(&e).unwrap().have, Some(other.shape()));
        }
        let e = require(store, step.as_deref(), None, &want())
            .unwrap_err()
            .context("open the render store");
        let refused = other_shape(&e).expect("found under context");
        assert_eq!(refused.have, None);
        let said = refused.to_string();
        assert!(said.contains("no recorded shape"), "{said}");
        assert!(
            said.contains("datalib-pipeline sync <data_root> slack/render_markdown"),
            "{said}"
        );
        assert!(other_shape(&anyhow::anyhow!("no such table: grid_rows")).is_none());
    }
}
