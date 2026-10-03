//! Raw-store schema for the `gpx` provider. `INGEST.md` §"Tables" is
//! the reader's guide; this file is the shape.

/// A column's storage type, enough to read a row back generically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    Int,
    Text,
}

/// One table as the store layer handles it: the leading `key` columns
/// are the primary key.
#[derive(Debug, Clone, Copy)]
pub struct Table {
    pub name: &'static str,
    pub columns: &'static [(&'static str, Ty)],
    pub key: usize,
}

impl Table {
    pub fn ddl(&self) -> String {
        let cols: Vec<String> = self
            .columns
            .iter()
            .enumerate()
            .map(|(i, (c, ty))| {
                let ty = match ty {
                    Ty::Int => "INTEGER",
                    Ty::Text => "TEXT",
                };
                let null = if i < self.key || NOT_NULL.contains(c) {
                    "NOT NULL"
                } else {
                    "NULL"
                };
                format!("    {c} {ty} {null}")
            })
            .collect();
        let key: Vec<&str> = self.columns[..self.key].iter().map(|(c, _)| *c).collect();
        format!(
            "CREATE TABLE IF NOT EXISTS {} (\n{},\n    PRIMARY KEY ({})\n)",
            self.name,
            cols.join(",\n"),
            key.join(", ")
        )
    }
}

/// Columns every row has a value for; everything else is NULL when the
/// file had nothing there.
const NOT_NULL: &[&str] = &[
    "file_key",
    "blake3",
    "size",
    "prolog",
    "epilog",
    "layout",
    "fidelity",
    "wpt_order",
    "point_order",
    "self_closing",
    "trkpt_id",
    "rtept_id",
    "wpt_id",
];

const POINT_COLUMNS: &[(&str, Ty)] = &[
    ("id", Ty::Text),
    ("lat", Ty::Text),
    ("lon", Ty::Text),
    ("ele", Ty::Text),
    ("time", Ty::Text),
    ("time_ms", Ty::Int),
    ("attrs_xml", Ty::Text),
    ("rest_xml", Ty::Text),
    ("self_closing", Ty::Int),
];

/// The shared points: keyed by content, so a point two files hold is one
/// row.
pub const WPTS: Table = Table {
    name: "gpx_wpts",
    columns: POINT_COLUMNS,
    key: 1,
};
pub const RTEPTS: Table = Table {
    name: "gpx_rtepts",
    columns: POINT_COLUMNS,
    key: 1,
};
pub const TRKPTS: Table = Table {
    name: "gpx_trkpts",
    columns: POINT_COLUMNS,
    key: 1,
};

pub const FILES: Table = Table {
    name: "gpx_files",
    columns: &[
        ("path", Ty::Text),
        ("file_key", Ty::Text),
        ("blake3", Ty::Text),
        ("size", Ty::Int),
        ("version", Ty::Text),
        ("creator", Ty::Text),
        ("prolog", Ty::Text),
        ("epilog", Ty::Text),
        ("layout", Ty::Text),
        ("head_xml", Ty::Text),
        ("tail_xml", Ty::Text),
        ("wpt_order", Ty::Text),
        ("fidelity", Ty::Text),
    ],
    key: 1,
};

/// The per-file tables: everything a file is besides its points, keyed
/// under the file's `file_key` so one file's rows sit together.
pub const FILE_WPTS: Table = Table {
    name: "gpx_file_wpts",
    columns: &[
        ("file_key", Ty::Text),
        ("ord", Ty::Int),
        ("wpt_id", Ty::Text),
    ],
    key: 2,
};
pub const RTES: Table = Table {
    name: "gpx_rtes",
    columns: &[
        ("file_key", Ty::Text),
        ("rte", Ty::Int),
        ("attrs_xml", Ty::Text),
        ("head_xml", Ty::Text),
        ("tail_xml", Ty::Text),
        ("self_closing", Ty::Int),
        ("point_order", Ty::Text),
    ],
    key: 2,
};
pub const RTE_RTEPTS: Table = Table {
    name: "gpx_rte_rtepts",
    columns: &[
        ("file_key", Ty::Text),
        ("rte", Ty::Int),
        ("ord", Ty::Int),
        ("rtept_id", Ty::Text),
    ],
    key: 3,
};
pub const TRKS: Table = Table {
    name: "gpx_trks",
    columns: &[
        ("file_key", Ty::Text),
        ("trk", Ty::Int),
        ("attrs_xml", Ty::Text),
        ("head_xml", Ty::Text),
        ("tail_xml", Ty::Text),
        ("self_closing", Ty::Int),
    ],
    key: 2,
};
pub const TRKSEGS: Table = Table {
    name: "gpx_trksegs",
    columns: &[
        ("file_key", Ty::Text),
        ("trk", Ty::Int),
        ("seg", Ty::Int),
        ("attrs_xml", Ty::Text),
        ("head_xml", Ty::Text),
        ("tail_xml", Ty::Text),
        ("self_closing", Ty::Int),
        ("point_order", Ty::Text),
    ],
    key: 3,
};
pub const TRKSEG_TRKPTS: Table = Table {
    name: "gpx_trkseg_trkpts",
    columns: &[
        ("file_key", Ty::Text),
        ("trk", Ty::Int),
        ("seg", Ty::Int),
        ("ord", Ty::Int),
        ("trkpt_id", Ty::Text),
    ],
    key: 4,
};

/// Every table keyed under `file_key`, in the order a file is written.
pub const PER_FILE: &[Table] = &[FILE_WPTS, RTES, RTE_RTEPTS, TRKS, TRKSEGS, TRKSEG_TRKPTS];

/// Each shared point table and the member table that refers to it.
pub const MEMBERSHIP: &[(Table, Table, &str)] = &[
    (WPTS, FILE_WPTS, "wpt_id"),
    (RTEPTS, RTE_RTEPTS, "rtept_id"),
    (TRKPTS, TRKSEG_TRKPTS, "trkpt_id"),
];

pub const ALL: &[Table] = &[
    FILES,
    WPTS,
    RTEPTS,
    TRKPTS,
    FILE_WPTS,
    RTES,
    RTE_RTEPTS,
    TRKS,
    TRKSEGS,
    TRKSEG_TRKPTS,
];

pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = ALL.iter().map(Table::ddl).collect();
    // `gpx_files` is keyed by path; everything else joins on `file_key`.
    out.push("CREATE INDEX IF NOT EXISTS idx_gpx_files_file_key ON gpx_files (file_key)".into());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_reads_as_written() {
        assert_eq!(
            FILE_WPTS.ddl(),
            "CREATE TABLE IF NOT EXISTS gpx_file_wpts (\n    file_key TEXT NOT NULL,\n    \
             ord INTEGER NOT NULL,\n    wpt_id TEXT NOT NULL,\n    PRIMARY KEY (file_key, ord)\n)"
        );
    }

    #[test]
    fn every_table_has_ddl_and_a_key() {
        let ddl = full_ddl().join("\n");
        for t in ALL {
            assert!(
                ddl.contains(&format!("TABLE IF NOT EXISTS {} (", t.name)),
                "{}",
                t.name
            );
            assert!(t.key >= 1 && t.key <= t.columns.len(), "{}", t.name);
        }
    }
}
