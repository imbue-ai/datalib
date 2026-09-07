// What a source weighs, and how many things are in it.
//
// Two shapes of the same facts. [`MeasurementKind`] names what is being
// measured; [`SourceMeasurementRow`] accumulates one sample per run so a
// number can be drawn over time. The current value of each measurement
// also reaches `grid_rows` (as `byte_size` / `item_count`), where it is
// searchable — but only the current value: see the module docs on
// `datalib_etl::introspect` for why the history is kept apart.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// `markdowns.kind` for the per-source storage report.
///
/// Load-bearing in two places that must agree: `grid_index::doc_kind_for`
/// stamps it, and `IndexedMarkdownStore::render_versions` excludes it —
/// the report is rendered by datalib rather than by any provider
/// processor, so its version must not be measured against what those
/// processors declare.
pub const DOC_KIND: &str = "storage";

/// What sort of thing a measurement is about.
///
/// This is the `upstream_entity_kind` component of the measurement row's
/// id, so the strings are load-bearing: renaming a variant's `as_str`
/// re-keys every row it ever produced.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum MeasurementKind {
    /// The source's whole output tree — every byte under `<name>/`,
    /// including stores, rendered markdown and anything else that
    /// landed there. The number the disk cares about.
    Tree,
    /// One database file: `entities.doltlite_db`, `blobs.doltlite_db`,
    /// a render store. Bytes are the file's size on disk.
    Store,
    /// One table inside a store. Carries a row count; carries **no**
    /// byte size, because a content-addressed store has no per-table
    /// byte layout to report — chunks are shared between tables and
    /// between commits, so no honest number exists. See
    /// `datalib_etl::introspect`.
    Table,
}

impl MeasurementKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know — a row written
    /// by a newer one.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }

    /// The `grid_rows.kind` display label — what the grid's Type column
    /// and row-type filter show. Separate from [`Self::as_str`] on
    /// purpose: that one is part of an id and cannot be reworded, this
    /// one is a label and can.
    pub fn label(self) -> &'static str {
        match self {
            MeasurementKind::Tree => "Source Size",
            MeasurementKind::Store => "Store",
            MeasurementKind::Table => "Table",
        }
    }
}

impl std::fmt::Display for MeasurementKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One measurement of one thing, at one time.
///
/// Accumulates: nothing overwrites a row, and a run appends a new one
/// per subject. Pruning is a later problem — the rows are five short
/// columns and a source produces on the order of tens per run.
#[derive(Debug, Clone, Serialize, Deserialize, PortableTable, sqlx::FromRow)]
#[portable_table(table = "source_measurements", primary_key = "subject, measured_at")]
pub struct SourceMeasurementRow {
    /// What was measured, as a data-root-relative path — `slack/raw`,
    /// `slack/raw/entities.doltlite_db`, or
    /// `slack/raw/entities.doltlite_db#messages` for a table inside a
    /// store. Stable across runs, which is what makes the series join
    /// up.
    #[col(sql = "VARCHAR(512)")]
    pub subject: String,
    /// [`MeasurementKind`], as its `as_str`.
    #[col(sql = "VARCHAR(16)")]
    pub kind: String,
    /// The run-pinned `DATALIB_DAG_NOW` (ISO-8601 with explicit offset,
    /// per AGENTS.md), so every row one run writes carries one stamp
    /// and a run reads back as a single column in the series.
    #[col(sql = "VARCHAR(40)")]
    pub measured_at: String,
    /// Bytes on disk. NULL where no honest number exists — every
    /// `Table` row, since a prolly-tree store has no per-table byte
    /// layout.
    #[col(sql = "BIGINT")]
    pub bytes: Option<i64>,
    /// How many things: rows in a table, files under a directory. NULL
    /// where the subject is a single thing.
    #[col(sql = "BIGINT")]
    pub items: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The id-bearing string and the display label are allowed to
    /// differ, and one of them is allowed to change. Pin the one that
    /// is not: `as_str` is a component of every measurement row's uuid,
    /// so a rename here silently re-keys every row it ever produced.
    #[test]
    fn the_id_bearing_names_are_pinned() {
        assert_eq!(MeasurementKind::Tree.as_str(), "tree");
        assert_eq!(MeasurementKind::Store.as_str(), "store");
        assert_eq!(MeasurementKind::Table.as_str(), "table");
    }

    /// strum and serde are independent derives producing independent
    /// strings, so their agreeing is a real check rather than a
    /// tautology — see AGENTS.md, "Name a closed set of strings".
    #[test]
    fn strum_and_serde_agree_on_every_variant() {
        for &k in <MeasurementKind as strum::VariantArray>::VARIANTS {
            let via_serde = serde_json::to_string(&k).expect("serialize");
            assert_eq!(
                via_serde.trim_matches('"'),
                k.as_str(),
                "serde and strum disagree about {k:?}"
            );
            assert_eq!(MeasurementKind::parse(k.as_str()), Some(k));
        }
    }

    #[test]
    fn the_series_is_keyed_on_subject_and_time_so_a_rerun_appends() {
        let ddl = DDL
            .iter()
            .find(|(t, _)| *t == "source_measurements")
            .map(|(_, d)| *d)
            .expect("source_measurements DDL");
        assert!(
            ddl.contains("PRIMARY KEY (subject, measured_at)"),
            "a subject-only key would overwrite the history: {ddl}"
        );
    }
}
