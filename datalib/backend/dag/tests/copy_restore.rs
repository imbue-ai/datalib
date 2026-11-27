//! A library is its files: copied while nothing runs on it, the copy works
//! wherever it lands. Each test syncs the Docker image's demo library, the
//! TNG library, laid out by the image's own `stage_demo.sh` and
//! `unpack.sh`, copies some or all of it
//! the way a person backs one up (`cp -R`), and checks that the copy
//! answers search, `from:me` and the curated stores as the original did.
//! What to back up, and what these tests do not cover:
//! `docs/user/backing_up.md`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

const QUERIES: &[&str] = &["", "from:me", "picard", "warp core"];

/// Every sync runs at this instant, so what a run stamps (a problem row's
/// time) is the same in two runs and only the stores can differ. The
/// corpus's own "now" (`tests/fixtures/README.md`).
const NOW: &str = "2369-04-15T00:00:00+00:00";

struct Demo {
    _dir: tempfile::TempDir,
    inputs: PathBuf,
    library: PathBuf,
    /// The host's cache of file fingerprints, which a library does not
    /// hold: one per test, as one machine has one.
    cache: PathBuf,
}

impl Demo {
    fn path(&self, name: &str) -> PathBuf {
        self.library.with_file_name(name)
    }
}

fn runfile(rel: &str) -> PathBuf {
    let r = runfiles::Runfiles::create().expect("run this through bazel");
    let path = r
        .rlocation(format!("_main/{rel}"))
        .unwrap_or_else(|| panic!("no runfile {rel}"));
    assert!(path.exists(), "{rel} is not in the runfiles");
    path
}

/// The staged binary dir (`//datalib/backend:bin`): the runner finds
/// `datalib-step` beside itself.
fn bin_dir() -> PathBuf {
    let dir = runfile("datalib/backend/bin");
    for name in ["datalib-pipeline", "datalib-step", "datalib-applet"] {
        assert!(
            dir.join(name).exists(),
            "{name} is not in {}",
            dir.display()
        );
    }
    dir
}

/// Where the image puts the files the file-reading sources read.
const IMAGE_INPUTS: &str = "/opt/datalib/demo/inputs";

fn temp_dir() -> tempfile::TempDir {
    match std::env::var_os("TEST_TMPDIR") {
        Some(t) => tempfile::tempdir_in(t),
        None => tempfile::tempdir(),
    }
    .expect("a temp dir")
}

/// The demo library as `stage_demo.sh` stages it into a build context
/// under `dir`: its archive, its config and its curated stores.
fn staged_demo(dir: &Path) -> PathBuf {
    let ctx = dir.join("ctx");
    let ingested = runfile("tests/fixtures/ingested/config_body.toml");
    let staged = Command::new(runfile("datalib/docker/stage_demo.sh"))
        .arg(&ctx)
        .arg(ingested.parent().expect("the fixture build's folder"))
        .status()
        .expect("run stage_demo.sh");
    assert!(staged.success(), "stage_demo.sh failed");
    ctx.join("demo")
}

/// The demo library as the image builds it, less the embeddings, which
/// need the model and change nothing these tests compare: staged by
/// `stage_demo.sh`, unpacked by `unpack.sh`, then one sync. Its inputs
/// are moved out beside it and its config pointed there, because they
/// stand for a person's exports, which a copy of the library does not
/// carry.
fn synced_demo() -> Demo {
    let dir = temp_dir();
    let library = dir.path().join("library");
    cp_r(&staged_demo(dir.path()), &library);
    let unpacked = Command::new("sh")
        .arg(library.join("unpack.sh"))
        .arg(&library)
        .status()
        .expect("run unpack.sh");
    assert!(unpacked.success(), "unpack.sh failed");
    let inputs = dir.path().join("inputs");
    std::fs::rename(library.join("inputs"), &inputs).expect("move the inputs out");
    let config_path = library.join("config.toml");
    let config = std::fs::read_to_string(&config_path).expect("the staged config");
    assert!(config.contains(IMAGE_INPUTS), "the config names no inputs");
    let config = without_embeds(&config.replace(IMAGE_INPUTS, path_str(&inputs)));
    std::fs::write(&config_path, config).expect("write the config");
    let cache = dir.path().join("cache");
    let demo = Demo {
        _dir: dir,
        inputs,
        library,
        cache,
    };
    let first = demo.sync(&demo.library);
    assert!(
        first.values().all(|s| s.starts_with("succeeded")),
        "the first sync: {first:#?}"
    );
    demo
}

fn without_embeds(config: &str) -> String {
    let mut doc: toml::Table = toml::from_str(config).expect("the demo config parses");
    let steps = doc["steps"].as_array_mut().expect("[[steps]]");
    steps.retain(|s| !s["id"].as_str().expect("a step id").ends_with("/embed"));
    toml::to_string(&doc).expect("write the config")
}

/// The groups whose download is kept (`common.download = false`), and
/// those whose download reads its inputs.
fn kept_and_reading(library: &Path) -> (Vec<String>, Vec<String>) {
    let text = std::fs::read_to_string(library.join("config.toml")).expect("read the config");
    let doc: toml::Table = toml::from_str(&text).expect("the config parses");
    let (mut kept, mut reading) = (Vec::new(), Vec::new());
    for step in doc["steps"].as_array().expect("[[steps]]") {
        let id = step["id"].as_str().expect("a step id");
        let Some(group) = id.strip_suffix("/ingest") else {
            continue;
        };
        let download = step
            .get("params")
            .and_then(|p| p.get("common"))
            .and_then(|c| c.get("download"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(true);
        if download { &mut reading } else { &mut kept }.push(group.to_string());
    }
    kept.sort();
    reading.sort();
    assert!(
        !kept.is_empty() && !reading.is_empty(),
        "{kept:?} {reading:?}"
    );
    (kept, reading)
}

/// The config the image starts with is in the shape `datalib-http` leaves
/// as written: one it would rewrite is rewritten on every start of a copy,
/// with a `config.toml.bak` left each time.
#[test]
fn the_demo_config_is_in_the_shape_the_app_leaves_as_written() {
    let dir = temp_dir();
    let text = std::fs::read_to_string(staged_demo(dir.path()).join("config.toml"))
        .expect("the staged config");
    let upgrade = datalib_migrate_config::upgrade_unattended(&text);
    assert!(
        upgrade.done.is_empty() && upgrade.refused.is_empty(),
        "datalib-http would rewrite the demo config on load: {:?} {:?}",
        upgrade.done,
        upgrade
            .refused
            .iter()
            .map(|(why, e)| format!("{why}: {e:#}"))
            .collect::<Vec<_>>()
    );
}

fn path_str(p: &Path) -> &str {
    p.to_str().expect("a UTF-8 temp path")
}

/// `cp -R`, as a person copies a folder.
fn cp_r(from: &Path, to: &Path) {
    let status = Command::new("cp")
        .arg("-R")
        .arg(from)
        .arg(to)
        .status()
        .expect("run cp");
    assert!(
        status.success(),
        "cp -R {} {}",
        from.display(),
        to.display()
    );
}

impl Demo {
    /// One sync of every step, which must end `done`, and what each step
    /// came to.
    fn sync(&self, library: &Path) -> BTreeMap<String, String> {
        let (done, outcomes) = self.sync_as_it_ends(library);
        assert!(done, "the sync of {} did not end done", library.display());
        outcomes
    }

    /// One sync of every step: whether it ended `done`, and what each step
    /// came to, from the lines `datalib-pipeline sync` prints to stdout.
    fn sync_as_it_ends(&self, library: &Path) -> (bool, BTreeMap<String, String>) {
        let bin = bin_dir();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new(bin.join("datalib-pipeline"))
            .arg("sync")
            .arg(library)
            .args(["--by", "copy_restore_test", "--now", NOW])
            .env("PATH", path)
            .env("DATALIB_CACHE_DIR", &self.cache)
            .output()
            .expect("run datalib-pipeline sync");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let done = match out.status.code() {
            Some(0) => true,
            Some(2) => false,
            _ => panic!(
                "sync of {} exited {}:\n{stdout}\n{}",
                library.display(),
                out.status,
                tail(&String::from_utf8_lossy(&out.stderr))
            ),
        };
        let outcomes = stdout
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(step, outcome)| (step.to_string(), outcome.trim().to_string()))
            .collect();
        (done, outcomes)
    }
}

/// The end of a sync's event stream, less its metrics.
fn tail(s: &str) -> String {
    let lines: Vec<&str> = s
        .lines()
        .filter(|l| !l.contains(r#""event":"metric""#))
        .collect();
    lines[lines.len().saturating_sub(40)..].join("\n")
}

/// What a person sees of the library: every grid row as search returns
/// it, the rows a few searches find, and the curated stores' rows.
fn answers(library: &Path) -> Value {
    let applet = bin_dir().join("datalib-applet");
    let mut found = serde_json::Map::new();
    for q in QUERIES {
        let out = Command::new(&applet)
            .arg("search")
            .arg(library)
            .arg(q)
            .args(["--limit", "1000000"])
            .output()
            .expect("run datalib-applet search");
        assert!(
            out.status.success(),
            "search {q:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let answer: Value = serde_json::from_slice(&out.stdout).expect("search answers JSON");
        assert!(
            answer["query_echo"]["search_error"].is_null(),
            "search {q:?} was not answered: {}",
            answer["query_echo"]
        );
        let rows = answer["rows"].as_array().expect("rows");
        assert_eq!(
            Some(rows.len() as u64),
            answer["total"].as_u64(),
            "one page of {q:?}"
        );
        assert!(!rows.is_empty(), "search {q:?} found nothing in the demo");
        let key = answer["row_key"].as_str().expect("the rows' key column");
        let by_key: serde_json::Map<String, Value> = rows
            .iter()
            .map(|r| (r[key].as_str().expect("a row key").to_string(), r.clone()))
            .collect();
        assert_eq!(by_key.len(), rows.len(), "two rows of {q:?} share a {key}");
        found.insert(format!("search {q:?}"), Value::Object(by_key));
    }
    let curated = datalib_curated_overlay::dumps(library);
    assert!(!curated.is_empty(), "the demo has curated stores");
    json!({ "found": found, "curated": curated })
}

/// The answers less what each store measures on disk (the grid's own
/// `datalib` rows of storage), which any write moves, a download's that
/// finds its input gone included.
fn without_disk_use(mut answers: Value) -> Value {
    for rows in answers["found"]
        .as_object_mut()
        .expect("found")
        .values_mut()
    {
        for row in rows.as_object_mut().expect("rows by key").values_mut() {
            if row["provider"] == "datalib" {
                row["byte_size"] = Value::Null;
            }
        }
    }
    answers
}

/// Every place two answers part, each as the path to it and the two
/// values, so a failure names what changed rather than printing two
/// whole libraries.
fn differences(a: &Value, b: &Value, at: &str, out: &mut Vec<String>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let keys: std::collections::BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for k in keys {
                let (l, r) = (
                    x.get(k).unwrap_or(&Value::Null),
                    y.get(k).unwrap_or(&Value::Null),
                );
                differences(l, r, &format!("{at}.{k}"), out);
            }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            for (i, (l, r)) in x.iter().zip(y).enumerate() {
                differences(l, r, &format!("{at}[{i}]"), out);
            }
        }
        _ if a == b => {}
        _ => out.push(format!("{at}: {a} became {b}")),
    }
}

fn assert_same(before: &Value, after: &Value, what: &str) {
    let mut found = Vec::new();
    differences(before, after, "", &mut found);
    assert!(
        found.is_empty(),
        "{what} answers differently in {} places:\n{}",
        found.len(),
        found
            .iter()
            .take(30)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// A source's download runs on every sync, so it reads its input again
/// and finds nothing new; every other step was up to date.
fn assert_nothing_redone(outcomes: &BTreeMap<String, String>) {
    let redone: Vec<_> = outcomes
        .iter()
        .filter(|(step, outcome)| {
            let source =
                step.ends_with("/ingest") && outcome.as_str() == "succeeded, output unchanged";
            !(source || outcome.as_str() == "skipped_up_to_date")
        })
        .collect();
    assert!(!outcomes.is_empty(), "the sync named no steps");
    assert!(redone.is_empty(), "the copy redid work: {redone:#?}");
}

/// Every file under `dir` whose bytes name `needle`.
fn files_naming(dir: &Path, needle: &str) -> Vec<PathBuf> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .map(|e| e.expect("walk the copy"))
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            let bytes = std::fs::read(e.path()).expect("read a file of the copy");
            bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
        })
        .map(|e| e.path().to_path_buf())
        .collect()
}

/// A stopped library copied to another path, with the original gone,
/// works there: nothing is redone, it answers as the original did, and
/// no file in it names where it came from.
#[test]
fn a_library_copied_to_another_path_works_there() {
    let demo = synced_demo();
    let before = answers(&demo.library);
    let copy = demo.path("moved");
    cp_r(&demo.library, &copy);
    std::fs::remove_dir_all(&demo.library).expect("remove the original");

    assert_nothing_redone(&demo.sync(&copy));
    assert_same(&before, &answers(&copy), "the moved library");
    let naming = files_naming(&copy, path_str(&demo.library));
    assert!(
        naming.is_empty(),
        "files in the copy name the original's path: {naming:?}"
    );
}

/// An older copy put back after the inputs moved on catches up with one
/// sync, to what the library that was never put back holds.
#[test]
fn an_older_copy_put_back_catches_up() {
    let demo = synced_demo();
    let backup = demo.path("backup");
    cp_r(&demo.library, &backup);
    let first = answers(&demo.library);

    // The library's address books are the fixture's second ones; going
    // back to the first is a change too.
    let contacts = demo.inputs.join("tng_contacts/carddav_work");
    std::fs::remove_dir_all(&contacts).expect("remove the address books");
    cp_r(
        &runfile("datalib/backend/etl/providers/contacts/tests/fixtures/carddav_tng"),
        &contacts,
    );
    demo.sync(&demo.library);
    let moved_on = answers(&demo.library);
    let mut changed = Vec::new();
    differences(&first, &moved_on, "", &mut changed);
    assert!(
        !changed.is_empty(),
        "the first address books changed nothing, so this test checks nothing"
    );

    std::fs::remove_dir_all(&demo.library).expect("remove the library");
    std::fs::rename(&backup, &demo.library).expect("put the backup back");
    demo.sync(&demo.library);
    assert_same(&moved_on, &answers(&demo.library), "the library put back");
}

/// A backup needs the config, the raw stores and the curated stores, and
/// nothing else Datalib wrote: put back on a machine without the exports
/// it was made from, the library renders and indexes the rest and answers
/// as before. A download whose input is gone may fail, and a kept one does
/// not. The one input it still needs is the PDFs', which that source
/// reads where they lie (`etl/providers/pdf/INGEST.md`).
#[test]
fn the_config_raw_and_curated_stores_are_the_backup() {
    let demo = synced_demo();
    let before = answers(&demo.library);
    let kept = demo.path("kept");
    std::fs::create_dir(&kept).expect("make the new library");
    std::fs::copy(demo.library.join("config.toml"), kept.join("config.toml")).expect("the config");
    cp_r(
        &demo.library.join("datalib_curated"),
        &kept.join("datalib_curated"),
    );
    let mut raw = Vec::new();
    for entry in std::fs::read_dir(&demo.library).expect("list the library") {
        let group = entry.expect("an entry").file_name();
        let download = demo.library.join(&group).join("ingest");
        if download.is_dir() {
            std::fs::create_dir(kept.join(&group)).expect("make the group's tree");
            cp_r(&download, &kept.join(&group).join("ingest"));
            raw.push(group.to_string_lossy().into_owned());
        }
    }
    raw.sort();
    let (kept_sources, reading) = kept_and_reading(&demo.library);
    let mut sources = [kept_sources, reading.clone()].concat();
    sources.sort();
    assert_eq!(raw, sources, "every source left a raw store");
    std::fs::remove_dir_all(&demo.library).expect("remove the original");
    remove_inputs_but(&demo.inputs, "tng_pdfs");

    let (_done, outcomes) = demo.sync_as_it_ends(&kept);
    let failed: Vec<&String> = outcomes
        .iter()
        .filter(|(_, o)| o.starts_with("failed"))
        .map(|(s, _)| s)
        .collect();
    let gone: Vec<String> = reading
        .iter()
        .filter(|g| g.as_str() != "tng_pdfs")
        .map(|g| format!("{g}/ingest"))
        .collect();
    assert!(
        !failed.is_empty() && failed.iter().all(|s| gone.contains(s)),
        "only downloads whose inputs are gone fail: {outcomes:#?}"
    );
    assert_same(
        &without_disk_use(before),
        &without_disk_use(answers(&kept)),
        "the library put back from its stores",
    );
}

/// Delete every source's inputs but `keep`'s.
fn remove_inputs_but(inputs: &Path, keep: &str) {
    for input in std::fs::read_dir(inputs).expect("list the inputs") {
        let input = input.expect("an input").path();
        if input.is_dir() && !input.ends_with(keep) {
            std::fs::remove_dir_all(&input).expect("remove an input");
        }
    }
}

/// A copy of `from` as a backup tool that honours `CACHEDIR.TAG` makes
/// one (restic's and Borg's `--exclude-caches`): a tagged directory keeps
/// its tag and nothing else.
fn cp_skipping_caches(from: &Path, to: &Path) -> Vec<PathBuf> {
    let mut skipped = Vec::new();
    let mut walk = walkdir::WalkDir::new(from).into_iter();
    while let Some(entry) = walk.next() {
        let entry = entry.expect("walk the library");
        let dest = to.join(entry.path().strip_prefix(from).expect("under from"));
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dest).expect("make a dir of the backup");
            let tag = entry.path().join("CACHEDIR.TAG");
            if tag.is_file() {
                std::fs::copy(&tag, dest.join("CACHEDIR.TAG")).expect("keep the tag");
                skipped.push(entry.path().to_path_buf());
                walk.skip_current_dir();
            }
        } else {
            std::fs::copy(entry.path(), &dest).expect("copy a file of the library");
        }
    }
    skipped
}

/// A backup tool that skips cache directories copies the whole library.
/// Nothing in one may be marked as a cache: a tree left out of a backup
/// stays missing after a restore, because the loop's store in `system/`
/// says its step is up to date. A tag an earlier build left on each render
/// tree and the index tree is gone after the next sync's prepare pass.
#[test]
fn a_backup_that_skips_cache_directories_copies_the_whole_library() {
    let demo = synced_demo();
    let tagged: Vec<PathBuf> = std::fs::read_dir(&demo.library)
        .expect("list the library")
        .map(|e| e.expect("an entry").path().join("render_markdown"))
        .filter(|p| p.is_dir())
        .chain([demo.library.join("unified_grid_and_search_index")])
        .collect();
    assert!(tagged.len() >= 7, "a render tree per source: {tagged:?}");
    for dir in &tagged {
        std::fs::write(
            dir.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .expect("tag a tree as an earlier build did");
    }
    demo.sync(&demo.library);
    let before = answers(&demo.library);

    let restored = demo.path("restored");
    let skipped = cp_skipping_caches(&demo.library, &restored);
    assert!(skipped.is_empty(), "the library marks caches: {skipped:?}");
    std::fs::remove_dir_all(&demo.library).expect("remove the original");

    assert_nothing_redone(&demo.sync(&restored));
    assert_same(&before, &answers(&restored), "the library put back");
}

/// `config` with every source's download but those `except` names set
/// to `common.download = false`.
fn not_downloading(config: &str, except: &[&str]) -> String {
    let mut doc: toml::Table = toml::from_str(config).expect("the config parses");
    for step in doc["steps"].as_array_mut().expect("[[steps]]") {
        let id = step["id"].as_str().expect("a step id").to_string();
        let Some(group) = id.strip_suffix("/ingest") else {
            continue;
        };
        if except.contains(&group) {
            continue;
        }
        let params = step
            .as_table_mut()
            .expect("a step table")
            .entry("params")
            .or_insert_with(|| toml::Table::new().into());
        let common = params
            .as_table_mut()
            .expect("params are a table")
            .entry("common")
            .or_insert_with(|| toml::Table::new().into());
        common
            .as_table_mut()
            .expect("common is a table")
            .insert("download".into(), false.into());
    }
    toml::to_string(&doc).expect("write the config")
}

/// A source whose upstream is gone keeps what it has: with
/// `common.download = false` and its input deleted, each sync ends done,
/// redoes nothing and answers as before, and a reset, which nothing could
/// fill again, is refused.
#[test]
fn a_source_that_no_longer_downloads_keeps_what_it_has() {
    let demo = synced_demo();
    let before = answers(&demo.library);
    let config = demo.library.join("config.toml");
    let text = std::fs::read_to_string(&config).expect("read the config");
    std::fs::write(&config, not_downloading(&text, &["tng_pdfs"])).expect("write the config");
    remove_inputs_but(&demo.inputs, "tng_pdfs");

    // An edited step is stale, so what reads the ones edited runs once
    // and finds nothing new; after that, nothing runs.
    let edited = demo.sync(&demo.library);
    let made_anew: Vec<_> = edited
        .iter()
        .filter(|(step, outcome)| {
            !step.ends_with("/ingest")
                && !matches!(
                    outcome.as_str(),
                    "skipped_up_to_date" | "succeeded, output unchanged"
                )
        })
        .collect();
    assert!(
        made_anew.is_empty(),
        "the edit made something anew: {made_anew:#?}"
    );
    assert_nothing_redone(&demo.sync(&demo.library));
    assert_same(&before, &answers(&demo.library), "the library kept");

    let reset = Command::new(bin_dir().join("datalib-pipeline"))
        .arg("reset")
        .arg(&demo.library)
        .arg("linkedin/ingest")
        .env("PATH", bin_dir())
        .env("DATALIB_CACHE_DIR", &demo.cache)
        .output()
        .expect("run datalib-pipeline reset");
    let said = String::from_utf8_lossy(&reset.stderr);
    assert!(
        !reset.status.success() && said.contains("does not download"),
        "a reset of a source that does not download: {}\n{}",
        reset.status,
        tail(&said)
    );
    assert_same(
        &before,
        &answers(&demo.library),
        "the library after the reset",
    );
}
