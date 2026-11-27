//! Snapshot of the TNG fixture's `backend_index.sqlite` contents.
//!
//! One row per source is a storage report (`provider: "datalib"`,
//! `kind: "Source Size"`). Its body deliberately carries no byte
//! figure: a store's size is not reproducible — it drifts
//! between rebuilds on one machine and differs outright between
//! machines — so no row's hash may contain it. That is enforced at
//! the producer (`datalib_step::introspect`), not scrubbed here. The
//! report as a whole does show the sizes, so its hash is left out.

use std::collections::BTreeMap;
use std::path::PathBuf;

use datalib_store_dump::{dump, store_as_jsonl, Keep};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

pub(crate) fn fixture_db_path() -> PathBuf {
    if let Ok(r) = runfiles::Runfiles::create() {
        if let Some(candidate) = r.rlocation("_main/tests/fixtures/ingested/backend_index.sqlite") {
            if candidate.exists() {
                return candidate;
            }
        }
    }
    let cargo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let bazel_bin = cargo_root
        .join("../../../bazel-bin/tests/fixtures/ingested/backend_index.sqlite")
        .canonicalize()
        .unwrap_or_else(|_| {
            panic!(
                "fixture backend_index.sqlite not found. Run \
                 `bazelisk build //tests/fixtures:ingested_tng` first."
            )
        });
    bazel_bin
}

/// SHA-256 of a long string, truncated to 16 hex chars, for the few long
/// fields a `.snap` would otherwise carry whole. A row's body is already
/// digested for us: `content_hash`, which this snapshot cuts to the same
/// width.
fn digest(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let d = h.finalize();
    format!("sha256:{:.16x}", BytesAsHex(&d[..]))
}

struct BytesAsHex<'a>(&'a [u8]);
impl<'a> std::fmt::LowerHex for BytesAsHex<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// A local file's own path is the build sandbox's.
fn stable_source_url(url: &str) -> String {
    match url.strip_prefix("file://") {
        Some(rest) => format!("file://…/{}", rest.rsplit('/').next().unwrap_or(rest)),
        None => url.to_string(),
    }
}

/// What a row says beside the text it stands for: a document's text is the
/// render preview's to show, and the terms have a snapshot of their own
/// (`fixture_terms_snapshot`). A row's text is in its `content_hash`.
const NOT_SHOWN: &[&str] = &["preview", "front", "supplied_terms"];

/// `row` as the snapshot has it. A field that is not there is null; one
/// that is empty is written `""`.
fn shown(row: &Map<String, Value>) -> Value {
    let cells = row
        .iter()
        .filter(|(k, v)| !v.is_null() && !NOT_SHOWN.contains(&k.as_str()));
    let cells = cells.map(|(k, v)| {
        let v = match (k.as_str(), v.as_str()) {
            ("source_url", Some(url)) => json!(stable_source_url(url)),
            ("content_hash", Some(hash)) => json!(hash.get(..16).unwrap_or(hash)),
            _ => v.clone(),
        };
        (k.clone(), v)
    });
    Value::Object(cells.collect())
}

/// The storage reports' rows, a digest for each report and kind: a
/// measurement's own fields are pinned by `datalib_step::introspect`'s
/// tests, and there are nearly half as many of them as of everything
/// else. A row added, removed, re-keyed, or whose count moved changes the
/// digest.
fn storage_digests(storage: &[&Map<String, Value>]) -> Vec<Value> {
    let text = |r: &Map<String, Value>, k: &str| r[k].as_str().unwrap_or_default().to_string();
    let mut by_report: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for r in storage {
        by_report
            .entry((text(r, "conversation_name"), text(r, "kind")))
            .or_default()
            .push(format!("{}\t{}", text(r, "uuid"), text(r, "content_hash")));
    }
    let digests = by_report.into_iter().map(|((report, kind), mut members)| {
        members.sort();
        json!({
            "report": report,
            "kind": kind,
            "rows": members.len(),
            "members_sha": digest(&members.join("\n")),
        })
    });
    digests.collect()
}

/// Dumped as `//datalib/backend/dag:manual_e2e_live_sync_golden` (the
/// sync of real accounts into goldens) dumps its stores. `git diff
/// --word-diff` shows which field of a row moved.
#[tokio::test]
async fn snapshot_grid_rows_and_documents() {
    let path = fixture_db_path().canonicalize().expect("the fixture index");
    let dump = dump(&path, |t| match t {
        "grid_rows" | "markdowns" => Keep::Rows,
        _ => Keep::Nothing,
    })
    .await;
    let rows = |t: &str| {
        dump[t]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
    };
    let datalib = datalib_schema::providers::Provider::Datalib.as_str();
    let (storage, grid_rows): (Vec<_>, Vec<_>) =
        rows("grid_rows").partition(|r| r["provider"] == datalib);
    let documents = rows("markdowns").map(|r| {
        let mut document = shown(r);
        // A storage report's own text has the stores' sizes in it, and
        // those differ from one machine to the next.
        if let (true, Some(cells)) = (r["provider"] == datalib, document.as_object_mut()) {
            cells.remove("content_hash");
        }
        document
    });
    let snapshot = json!({
        "grid_rows": grid_rows.into_iter().map(shown).collect::<Vec<_>>(),
        "markdowns": documents.collect::<Vec<_>>(),
        "storage_rows": storage_digests(&storage),
    });
    insta::assert_snapshot!("fixture_backend_index", store_as_jsonl(&snapshot));
}
