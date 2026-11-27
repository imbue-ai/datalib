//! A checked-in `datalib_curated/` laid over a data root, and the checks
//! that it fits: the TNG fixture's and the live bake's share every step
//! (README.md beside this file).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use datalib_problems::Severity;
use datalib_store_dump::Keep;
use serde_json::{json, Value};

/// The curated stores, by their path under a data root.
fn stores(data_root: &Path) -> Vec<PathBuf> {
    vec![
        datalib_contacts::store_path(data_root),
        datalib_problem_mutes::store_path(data_root),
    ]
}

/// Tables a checked-in store may leave empty, and why: nothing the app
/// offers can write a row there yet.
const MAY_BE_EMPTY: &[(&str, &str)] = &[];

/// The `_datalib_meta` keys naming the build that last opened a store,
/// which move with every build and are no change to the store.
const BUILD_KEYS: &[&str] = &["datalib_version", "git_hash", "doltlite_version"];

const REDACTED: &str = "[redacted]";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for the curated stores")
}

/// The checked-in folder's stores into the root, where a person's own
/// `datalib_curated/` is when a new build first opens their library.
pub fn copy_in(src: &Path, data_root: &Path) {
    assert!(
        src.is_dir(),
        "no {}: the curated stores laid over this root are checked in there \
         (datalib/backend/curated_overlay/README.md)",
        src.display()
    );
    let dst = data_root.join(datalib_runtime::layout::CURATED_DIR);
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry.expect("walk the checked-in curated stores");
        let rel = entry.path().strip_prefix(src).expect("under src");
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(dst.join(rel)).expect("make a curated dir");
        } else if entry.path().extension().is_some_and(|e| e == "doltlite_db") {
            std::fs::copy(entry.path(), dst.join(rel)).expect("copy a curated store");
            let mut perms = std::fs::metadata(dst.join(rel)).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            std::fs::set_permissions(dst.join(rel), perms).expect("make the copy writable");
        }
    }
    let known = stores(data_root);
    let unknown: Vec<String> = walkdir::WalkDir::new(&dst)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file() && !known.contains(&e.path().to_path_buf()))
        .map(|e| e.path().display().to_string())
        .collect();
    assert!(
        unknown.is_empty(),
        "curated stores this crate does not know: {unknown:?}. Add each to \
         `stores`, so it is opened and checked"
    );
    for path in &known {
        assert!(
            path.is_file(),
            "{} is not checked in; every curated store the app has must be, \
             with a row in each table",
            path.display()
        );
    }
}

/// Each store opened as its one writer opens it at launch, which runs
/// its migrations up to this build's shape: the contacts applet's
/// `Store::open`, and the index applet's open of the mutes
/// (`applets/src/unified_grid_and_search_index/mutes.rs`).
pub fn open_as_owners(data_root: &Path) {
    runtime().block_on(async {
        let contacts = datalib_contacts::Store::open(&datalib_contacts::store_path(data_root))
            .await
            .expect("the contacts applet's open of the checked-in contacts store");
        contacts.close().await;
        let mutes = datalib_etl::doltlite_raw::open_curated(
            &datalib_problem_mutes::store_path(data_root),
            datalib_problem_mutes::DDL,
            datalib_store_meta::StoreKind::ProblemMutes,
            &[],
        )
        .await
        .expect("the index applet's open of the checked-in mutes store");
        mutes.close().await;
    });
}

fn raw_dump(path: &Path) -> Value {
    runtime().block_on(datalib_store_dump::dump(path, |_| Keep::Rows))
}

fn rows<'a>(dump: &'a Value, table: &str) -> &'a [Value] {
    dump.get(table)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// A table with no row cannot show a missing rung: an open rebuilds an
/// empty table whatever shape it is in.
pub fn assert_every_table_has_a_row(data_root: &Path) {
    let mut empty = Vec::new();
    for path in stores(data_root) {
        let dump = raw_dump(&path);
        for (table, table_rows) in dump.as_object().into_iter().flatten() {
            let allowed = MAY_BE_EMPTY.iter().any(|(t, _)| t == table);
            if table_rows.as_array().is_some_and(Vec::is_empty) && !allowed {
                empty.push(format!("{}#{table}", path.display()));
            }
        }
    }
    assert!(
        empty.is_empty(),
        "checked-in curated tables with no row: {empty:?}. Make one through the \
         app and copy the store back (datalib/backend/curated_overlay/README.md), \
         or say in MAY_BE_EMPTY why nothing can yet"
    );
}

/// A store's rows with the build that last opened it left out, keyed by
/// its path under `datalib_curated/`.
fn without_build(mut dump: Value, redact: bool) -> Value {
    if let Some(Value::Array(meta)) = dump.get_mut("_datalib_meta") {
        if !redact {
            meta.retain(|r| !BUILD_KEYS.contains(&r["key"].as_str().unwrap_or_default()));
        }
        for r in meta.iter_mut() {
            let Value::Object(m) = r else { continue };
            let is_build = BUILD_KEYS.contains(&m["key"].as_str().unwrap_or_default());
            for k in ["written_at_utc", "tz_offset"] {
                match redact {
                    true => m.insert(k.into(), REDACTED.into()),
                    false => m.remove(k),
                };
            }
            if is_build {
                m.insert("value".into(), REDACTED.into());
            }
        }
    }
    dump
}

/// Every curated store's rows, for a snapshot and for comparing one point
/// of a run with another; the build that last opened each is redacted.
pub fn dumps(data_root: &Path) -> BTreeMap<String, Value> {
    let curated = data_root.join(datalib_runtime::layout::CURATED_DIR);
    stores(data_root)
        .iter()
        .map(|p| {
            let rel = p.strip_prefix(&curated).expect("under datalib_curated");
            (rel.display().to_string(), without_build(raw_dump(p), true))
        })
        .collect()
}

/// One search, as the app's search box answers it, by the shipped binary.
fn search(applet: &Path, data_root: &Path, q: &str) -> Vec<(String, String)> {
    let out = Command::new(applet)
        .arg("search")
        .arg(data_root)
        .arg(q)
        .args(["--limit", "1000000"])
        .output()
        .expect("run datalib-applet search");
    assert!(
        out.status.success(),
        "datalib-applet search {q:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let answer: Value = serde_json::from_slice(&out.stdout).expect("search answers JSON");
    assert!(
        answer.get("refused").is_none_or(Value::is_null)
            && answer["query_echo"]["search_error"].is_null(),
        "search {q:?} was not answered: {answer}"
    );
    let found: Vec<(String, String)> = answer["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|r| {
            let s = |k: &str| r[k].as_str().unwrap_or_default().to_string();
            (s("uuid"), s("source_id"))
        })
        .collect();
    assert_eq!(
        Some(found.len() as u64),
        answer["total"].as_u64(),
        "search {q:?} answered one page of several"
    );
    found
}

fn uuids(found: &[(String, String)]) -> BTreeSet<&str> {
    found.iter().map(|(u, _)| u.as_str()).collect()
}

fn sources(found: &[(String, String)]) -> Value {
    let set: BTreeSet<&str> = found.iter().map(|(_, s)| s.as_str()).collect();
    json!(set)
}

/// What the curated stores come to over this root's data, as the app's
/// search answers: for each contact, the sources each of its handles and
/// the contact itself reach; for `from:me`, the same per handle of the
/// contact marked as you; for each mute, the problem it mutes.
///
/// Fails on a handle no document has, a merged-away contact that finds
/// other rows than its survivor, a group that misses a member's rows, a
/// `from:me` that misses a row from a handle of the contact marked as
/// you, and a mute that names no problem or that the counts still count.
pub fn overlay(applet: &Path, data_root: &Path) -> Value {
    let store = raw_dump(&datalib_contacts::store_path(data_root));
    let s = |row: &Value, k: &str| row[k].as_str().map(str::to_string);
    let names: BTreeMap<String, String> = rows(&store, "contacts")
        .iter()
        .filter_map(|c| Some((s(c, "contact_id")?, s(c, "name")?)))
        .collect();
    let handles_of = |id: &str| -> Vec<String> {
        rows(&store, "handles")
            .iter()
            .filter(|h| h["contact_id"].as_str() == Some(id))
            .filter_map(|h| s(h, "handle"))
            .collect()
    };
    let find = |q: &str| search(applet, data_root, q);

    let mut failures = Vec::new();
    let mut found_for: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut contacts = BTreeMap::new();
    for c in rows(&store, "contacts") {
        let (Some(id), Some(name)) = (s(c, "contact_id"), s(c, "name")) else {
            continue;
        };
        let label = name.clone();
        let found = find(&format!("with:contact:{id}"));
        if let Some(into) = s(c, "merged_into") {
            contacts.insert(label, json!(format!("merged into {}", names[&into])));
            found_for.insert(id, found);
            continue;
        }
        if found.is_empty() {
            failures.push(format!("{name:?} (contact:{id}) finds nothing"));
        }
        let mut entry = BTreeMap::new();
        for h in handles_of(&id) {
            assert!(
                !h.contains(char::is_whitespace),
                "{h:?}: a search term cannot hold a space"
            );
            let by_handle = find(&format!("with:{h}"));
            if !by_handle.is_empty() {
                entry.insert(h, sources(&by_handle));
                continue;
            }
            // A handle only an address book or a member list holds is on
            // no row, and still fits: the person card shows that record.
            let records = record_sources(data_root, &h);
            if records.is_empty() {
                failures.push(format!(
                    "{name:?}'s {h} is in no document and on no source's record \
                     of a person: if the data moved, link another"
                ));
            }
            entry.insert(h, json!({ "only on the records of": records }));
        }
        entry.insert(format!("contact:{name}"), sources(&found));
        contacts.insert(label, json!(entry));
        found_for.insert(id, found);
    }
    for c in rows(&store, "contacts") {
        let (Some(id), Some(into)) = (s(c, "contact_id"), s(c, "merged_into")) else {
            continue;
        };
        if uuids(&found_for[&id]) != uuids(&found_for[&into]) {
            failures.push(format!(
                "{:?}, merged into {:?}, finds other rows than it",
                names[&id], names[&into]
            ));
        }
    }
    let me = find("from:me");
    let mut from_me = BTreeMap::new();
    for m in rows(&store, "me") {
        let Some(id) = s(m, "contact_id") else {
            continue;
        };
        for h in handles_of(&id) {
            let from_h = find(&format!("from:{h}"));
            if !uuids(&from_h).is_subset(&uuids(&me)) {
                failures.push(format!("from:me misses rows that from:{h} finds"));
            }
            from_me.insert(format!("from:{h}"), sources(&from_h));
        }
    }
    from_me.insert("from:me".to_string(), sources(&me));

    let mutes = mutes_overlay(data_root, &mut failures);
    assert!(
        failures.is_empty(),
        "the curated stores do not fit this root's data:\n  {}",
        failures.join("\n  ")
    );
    json!({ "contacts": contacts, "me": from_me, "mutes": mutes })
}

/// The sources whose records of a person, in the grid index, hold `handle`.
fn record_sources(data_root: &Path, handle: &str) -> Vec<String> {
    let db =
        data_root.join("unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite");
    runtime().block_on(async {
        let pool = datalib_store_dump::open_readonly(&db).await;
        let found = datalib_sql::test_sql!(
            "SELECT DISTINCT r.source_id FROM contact_record_handles h \
             JOIN contact_records r USING (record_id) WHERE h.handle = ? ORDER BY 1"
        )
        .query_scalar()
        .bind(handle)
        .fetch_all(&pool)
        .await
        .expect("read the index's records of people");
        pool.close().await;
        found
    })
}

/// Each mute names a problem this root has, and the counts leave it out.
fn mutes_overlay(data_root: &Path, failures: &mut Vec<String>) -> BTreeMap<String, String> {
    let store = raw_dump(&datalib_problem_mutes::store_path(data_root));
    let muted: BTreeSet<String> = rows(&store, "mutes")
        .iter()
        .filter_map(|m| m["problem_uuid"].as_str().map(str::to_string))
        .collect();
    let rt = runtime();
    let problems: Vec<datalib_problems::ProblemRow> = rt
        .block_on(datalib_step_problems::of_steps(data_root))
        .into_iter()
        .flat_map(|step| step.rows)
        .collect();
    let mut out = BTreeMap::new();
    let mut counted = 0;
    for id in &muted {
        let Some(p) = problems.iter().find(|p| &p.problem_uuid == id) else {
            failures.push(format!(
                "the mute of {id} names no problem this root has: the data changed, \
                 or what problem_id mints it from moved"
            ));
            continue;
        };
        if matches!(p.severity, Severity::Error | Severity::Warning) {
            counted += 1;
        }
        out.insert(
            id.clone(),
            format!(
                "{} {} {}: {}",
                p.source_id,
                p.stage.as_str(),
                p.reason.as_str(),
                p.scope_key
            ),
        );
    }
    let counts = rt
        .block_on(datalib_problem_mutes::counts(data_root))
        .expect("count the problems as the Manage rows do")
        .expect("counts");
    let muted_in_counts: i64 = counts
        .values()
        .map(|c| c.muted.errors + c.muted.warnings)
        .sum();
    if muted_in_counts != counted {
        failures.push(format!(
            "the Manage counts set aside {muted_in_counts} muted rows, not the {counted} muted"
        ));
    }
    out
}

/// Each store this build changed (a migration run, a table added), with
/// the checked-in file it came from: `(the root's copy, the checked-in
/// file)`.
pub fn changed_by_this_build(data_root: &Path, src: &Path) -> Vec<(PathBuf, PathBuf)> {
    let curated = data_root.join(datalib_runtime::layout::CURATED_DIR);
    stores(data_root)
        .into_iter()
        .filter_map(|path| {
            let checked_in = src.join(path.strip_prefix(&curated).expect("under datalib_curated"));
            // Read a copy: an open leaves a lock file beside the store, and
            // the checked-in one may be a link into the source tree.
            let tmp = tempfile::tempdir().expect("tempdir");
            let copy = tmp.path().join(checked_in.file_name().expect("a file"));
            std::fs::copy(&checked_in, &copy).expect("copy the checked-in store");
            let same =
                without_build(raw_dump(&path), false) == without_build(raw_dump(&copy), false);
            (!same).then_some((path, checked_in))
        })
        .collect()
}
