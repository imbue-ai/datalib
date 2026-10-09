//! Problems a run has that are not about one record: a configured entry
//! the upstream does not have, a listing that did not come back as an
//! enumeration, a phase that failed wholesale.
//!
//! Distinct from the per-item transient failures `FetchSummary` counts as
//! `errors` and `record_object_error` pins to the record. These are about
//! the run's shape, so they are keyed by what failed rather than by a
//! record, and a run's set replaces the last one's whole: a corrected
//! config, or a listing that answers again, clears its row.
//!
//! Reporting one must not fail the run. A misspelling in a five-entry
//! list costs that entry and nothing else, the same way a config entry
//! the loader cannot use costs that entry and nothing else.

use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr, VariantArray};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProblemReason {
    /// The upstream has nothing by that name.
    NotFound,
    /// It exists, but this credential cannot read it.
    Forbidden,
}

impl ProblemReason {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadProblem {
    /// The config key that named it, e.g. `only_extract_labels`.
    pub setting: String,
    /// The value, spelled the way the config spelled it.
    pub value: String,
    pub reason: ProblemReason,
    /// What upstream said, or what the reader should do about it.
    pub detail: String,
}

impl DownloadProblem {
    pub fn not_found(setting: &str, value: &str, detail: impl Into<String>) -> Self {
        Self {
            setting: setting.to_string(),
            value: value.to_string(),
            reason: ProblemReason::NotFound,
            detail: detail.into(),
        }
    }

    pub fn forbidden(setting: &str, value: &str, detail: impl Into<String>) -> Self {
        Self {
            setting: setting.to_string(),
            value: value.to_string(),
            reason: ProblemReason::Forbidden,
            detail: detail.into(),
        }
    }
}

/// What a configured list of names resolved to.
#[derive(Debug)]
pub struct Resolution<T> {
    pub resolved: Vec<T>,
    pub problems: Vec<DownloadProblem>,
}

impl<T> Default for Resolution<T> {
    fn default() -> Self {
        Self {
            resolved: Vec::new(),
            problems: Vec::new(),
        }
    }
}

impl<T> Resolution<T> {
    /// Every configured entry missed.
    ///
    /// The caller has to decide what that means, because it depends on
    /// what an empty result does downstream. For a *filter* it is
    /// usually fatal: an empty filter means "everything", so falling
    /// through would mirror the whole account the config was narrowing.
    pub fn nothing_resolved(&self) -> bool {
        self.resolved.is_empty() && !self.problems.is_empty()
    }
}

/// Resolve a configured list against what upstream actually has,
/// keeping the entries that resolve and recording the ones that do not.
///
/// `lookup` returns `Err(detail)` for a miss, where `detail` says what
/// the reader should do — usually the list of valid names.
///
/// Never returns an error itself. One misspelling costs that entry, the
/// same way a config entry the loader cannot use costs that entry and
/// nothing else.
pub fn resolve_configured<T, F>(setting: &str, specs: &[String], mut lookup: F) -> Resolution<T>
where
    F: FnMut(&str) -> Result<T, String>,
{
    let mut out = Resolution::default();
    for spec in specs {
        match lookup(spec) {
            Ok(v) => out.resolved.push(v),
            Err(detail) => out
                .problems
                .push(DownloadProblem::not_found(setting, spec, detail)),
        }
    }
    out
}

/// The sweep key's prefix of a configured entry's row: every row
/// [`report`] writes, and only those, so a run's report can replace the
/// last one's whole.
const CONFIG_PREFIX: &str = "config:";

/// What a run could not do as a whole. Each variant is a sweep-key
/// prefix, so a run that reaches its end can replace the last run's rows
/// of every kind at once.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RunProblemKind {
    /// A listing that did not come back as an enumeration — not an
    /// array, nothing at all, or a page walk that stopped on an error —
    /// so absence from it means nothing and the stored rows were left
    /// alone. What is stored is stale until it lists again.
    Listing,
    /// A whole phase of the run failed before it did its work; the
    /// other phases ran.
    Phase,
}

impl RunProblemKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }

    fn key_prefix(self) -> String {
        format!("{}:", self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunProblem {
    pub kind: RunProblemKind,
    /// The listing or phase, as the provider names it: `workouts`,
    /// `devices`, `weight`.
    pub name: String,
    /// What went wrong, in the words of the error.
    pub detail: String,
    /// Whether the credential was refused: a listing the service
    /// would not give this token is a warning the reader can act on
    /// (ask for access), where one that failed is an error.
    #[serde(default)]
    pub forbidden: bool,
}

impl RunProblem {
    pub fn listing(name: &str, detail: impl Into<String>) -> Self {
        Self {
            kind: RunProblemKind::Listing,
            name: name.to_string(),
            detail: detail.into(),
            forbidden: false,
        }
    }

    pub fn phase(name: &str, detail: impl Into<String>) -> Self {
        Self {
            kind: RunProblemKind::Phase,
            name: name.to_string(),
            detail: detail.into(),
            forbidden: false,
        }
    }

    /// A listing the service refused this credential: an org, a
    /// workspace, a scope the token does not reach.
    pub fn forbidden(name: &str, detail: impl Into<String>) -> Self {
        Self {
            kind: RunProblemKind::Listing,
            name: name.to_string(),
            detail: detail.into(),
            forbidden: true,
        }
    }

    pub fn key(&self) -> String {
        format!("{}{}", self.kind.key_prefix(), self.name)
    }
}

/// One record a download could not fetch, named by the id **upstream**
/// uses for it.
///
/// The other way to report this is
/// [`crate::doltlite_raw::record_object_error`], and it is the right one
/// wherever the record has a `_bookkeeping` sidecar to stamp. This is
/// for the case that has none: a fetch that fails never reaches the
/// point of minting an id in our own keyspace, so the only name the run
/// has for it is the one upstream gave. Gmail is the example —
/// `gmail_messages` maps Gmail's id to the row it produced, and a
/// message that would not fetch produced none.
#[derive(Debug, Clone)]
pub struct RecordProblem {
    /// The raw table the id belongs to, e.g. `gmail_messages`.
    pub table: String,
    /// Upstream's id.
    pub id: String,
    /// What upstream said.
    pub detail: String,
}

/// The sweep key's prefix of a per-record row: every row
/// [`report_records`] writes, and only those, so a run's report
/// replaces the last one's whole and a record that fetches this time
/// stops being a problem.
pub const RECORD_PREFIX: &str = "record:";

impl RecordProblem {
    pub fn new(table: &str, id: &str, detail: impl Into<String>) -> Self {
        Self {
            table: table.to_string(),
            id: id.to_string(),
            detail: detail.into(),
        }
    }

    fn key(&self) -> String {
        format!("{RECORD_PREFIX}{}:{}", self.table, self.id)
    }
}

/// An entry the download read and chose not to store: no usable key, or
/// a kind the mirror does not hold. Named by `entry`, whatever names it
/// stably in the export (a URL, the entry's own text); the key hashes it,
/// so the length and contents of `entry` never reach the sweep key.
#[derive(Debug, Clone)]
pub struct SkippedRecord {
    pub entry: String,
    pub problem: datalib_problems::Problem,
}

/// The sweep key's prefix of a [`report_skipped`] row.
pub const SKIPPED_PREFIX: &str = "skipped:";

/// A configured entry upstream has sent nothing new for a while, named
/// the way the config names it.
#[derive(Debug, Clone)]
pub struct SilentEntry {
    pub name: String,
    /// Since when, in words the reader can act on.
    pub detail: String,
}

/// The sweep key's prefix of a [`report_silent`] row.
const SILENT_PREFIX: &str = "silent:";

/// One row to write: its sweep key, what became of the thing, and why.
pub(crate) type Row = (String, datalib_problems::Outcome, datalib_problems::Problem);

pub(crate) fn config_rows(problems: &[DownloadProblem]) -> Vec<Row> {
    use datalib_problems::{Outcome, Problem, Reason, Severity};
    problems
        .iter()
        .map(|p| {
            let reason = match p.reason {
                ProblemReason::NotFound => Reason::NotFound,
                ProblemReason::Forbidden => Reason::Forbidden,
            };
            (
                format!("{CONFIG_PREFIX}{}:{}", p.setting, p.value),
                Outcome::Dropped,
                Problem::explained(reason, Some(p.setting.clone()), &p.detail)
                    .severity(Severity::Warning),
            )
        })
        .collect()
}

pub(crate) fn run_rows(problems: &[RunProblem]) -> Vec<Row> {
    use datalib_problems::{Outcome, Problem, Reason, Severity};
    problems
        .iter()
        .map(|p| {
            let problem = if p.forbidden {
                Problem::explained(Reason::Forbidden, None, &p.detail).severity(Severity::Warning)
            } else {
                Problem::explained(Reason::FetchFailed, None, &p.detail).severity(Severity::Error)
            };
            (p.key(), Outcome::Dropped, problem)
        })
        .collect()
}

pub(crate) fn run_prefixes() -> Vec<String> {
    RunProblemKind::VARIANTS
        .iter()
        .map(|k| k.key_prefix())
        .collect()
}

pub(crate) fn record_rows(problems: &[RecordProblem]) -> Vec<Row> {
    use datalib_problems::{Outcome, Problem, Reason, Severity};
    problems
        .iter()
        .map(|p| {
            (
                p.key(),
                Outcome::Dropped,
                Problem::record(Reason::FetchFailed, &p.detail).severity(Severity::Error),
            )
        })
        .collect()
}

pub(crate) fn record_prefix(table: &str) -> String {
    format!("{RECORD_PREFIX}{table}:")
}

pub(crate) fn skipped_rows(part: &str, skipped: &[SkippedRecord]) -> Vec<Row> {
    skipped
        .iter()
        .map(|s| {
            let hash = blake3::hash(s.entry.as_bytes()).to_hex();
            (
                format!("{}{}", skipped_prefix(part), &hash[..16]),
                datalib_problems::Outcome::Dropped,
                s.problem.clone(),
            )
        })
        .collect()
}

pub(crate) fn skipped_prefix(part: &str) -> String {
    format!("{SKIPPED_PREFIX}{part}:")
}

pub(crate) fn silent_rows(silent: &[SilentEntry]) -> Vec<Row> {
    use datalib_problems::{Outcome, Problem, Reason, Severity};
    silent
        .iter()
        .map(|s| {
            (
                format!("{SILENT_PREFIX}{}", s.name),
                Outcome::Ok,
                Problem::explained(Reason::Silent, None, &s.detail).severity(Severity::Warning),
            )
        })
        .collect()
}

/// What a download's row is about, in words, read off its sweep key:
/// the listing, phase, configured entry or record it names. `None` for
/// a key no download writes.
pub fn about(scope_key: &str) -> Option<String> {
    let listing = RunProblemKind::Listing.key_prefix();
    let phase = RunProblemKind::Phase.key_prefix();
    if let Some(name) = scope_key.strip_prefix(listing.as_str()) {
        return Some(format!("listing {name}"));
    }
    if let Some(name) = scope_key.strip_prefix(phase.as_str()) {
        return Some(format!("the {name} phase"));
    }
    if let Some(rest) = scope_key.strip_prefix(CONFIG_PREFIX) {
        let (setting, value) = rest.split_once(':')?;
        return Some(format!("config {setting}: {value}"));
    }
    if let Some(name) = scope_key.strip_prefix(SILENT_PREFIX) {
        return Some(format!("config entry {name}"));
    }
    if let Some(rest) = scope_key.strip_prefix(RECORD_PREFIX) {
        let (table, id) = rest.split_once(':')?;
        return Some(format!("{table} record {id}"));
    }
    if let Some(rest) = scope_key.strip_prefix(SKIPPED_PREFIX) {
        let (part, _) = rest.split_once(':')?;
        return Some(format!("an entry in {part}, skipped"));
    }
    None
}

pub(crate) const CONFIG_SWEEP: &str = CONFIG_PREFIX;
pub(crate) const SILENT_SWEEP: &str = SILENT_PREFIX;

/// The rows a run has a verdict on: every entity-scoped row whose key
/// starts with `prefix`, less the ones `keep` says the run did not try
/// again (it is given the key with the prefix taken off).
pub(crate) struct Sweep {
    pub prefix: String,
    pub keep: Option<Untried>,
}

/// Says, of a key with its sweep prefix taken off, whether the run left
/// it untried.
pub(crate) type Untried = std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// SQLite's default bound-parameter limit is far above this; one
/// statement per chunk keeps a big sweep from being one statement per row.
const KEY_CHUNK: usize = 500;

/// Delete what `sweeps` cover, then write `rows`, in one transaction. A
/// key that was there before keeps its `first_seen_at_utc`, so the screen
/// can say how long something has been failing, and one recorded again
/// unchanged keeps every stamp (`ProblemRow::stamped`); two rows on one
/// key keep the first, since the key is the row's identity.
pub(crate) async fn apply(
    pool: &sqlx::SqlitePool,
    sweeps: &[Sweep],
    rows: Vec<Row>,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use datalib_problems::{ProblemRow, Scope, ScopeKind, Stage};
    use datalib_table::BulkUpsertable as _;
    use std::collections::{HashMap, HashSet};

    let mut seen = HashSet::new();
    let rows: Vec<Row> = rows
        .into_iter()
        .filter(|(key, _, _)| seen.insert(key.clone()))
        .collect();

    let mut tx = pool.begin().await.context("begin")?;
    let mut earlier: HashMap<String, ProblemRow> = HashMap::new();
    let mut gone: Vec<String> = Vec::new();
    for sweep in sweeps {
        // `INSTR(x, ?) = 1` rather than `LIKE`: `_` in a value is a
        // wildcard to LIKE.
        let swept =
            sqlx::query("SELECT * FROM problems WHERE scope_kind = ? AND INSTR(scope_key, ?) = 1")
                .bind(ScopeKind::Entity.as_str())
                .bind(&sweep.prefix)
                .fetch_all(&mut *tx)
                .await
                .with_context(|| format!("read the last run's {} problems", sweep.prefix))?;
        for r in &swept {
            let row = ProblemRow::from_row(r)?;
            let key = row.scope_key.clone();
            let kept = sweep
                .keep
                .as_ref()
                .is_some_and(|keep| keep(key.strip_prefix(sweep.prefix.as_str()).unwrap_or(&key)));
            if !kept {
                gone.push(key.clone());
            }
            earlier.entry(key).or_insert(row);
        }
    }
    let unswept: Vec<&str> = rows
        .iter()
        .map(|(key, _, _)| key.as_str())
        .filter(|key| !earlier.contains_key(*key))
        .collect();
    for chunk in unswept.chunks(KEY_CHUNK) {
        // Audited: only `?` placeholders are built, one per key; every
        // key is bound.
        let sql = format!(
            "SELECT * FROM problems WHERE scope_kind = ? AND scope_key IN ({})",
            vec!["?"; chunk.len()].join(",")
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(ScopeKind::Entity.as_str());
        for key in chunk {
            q = q.bind(*key);
        }
        let found = q
            .fetch_all(&mut *tx)
            .await
            .context("read the rows this run writes again")?;
        for r in &found {
            let row = ProblemRow::from_row(r)?;
            gone.push(row.scope_key.clone());
            earlier.entry(row.scope_key.clone()).or_insert(row);
        }
    }
    for chunk in gone.chunks(KEY_CHUNK) {
        // Audited: as above.
        let sql = format!(
            "DELETE FROM problems WHERE scope_kind = ? AND scope_key IN ({})",
            vec!["?"; chunk.len()].join(",")
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(ScopeKind::Entity.as_str());
        for key in chunk {
            q = q.bind(key);
        }
        q.execute(&mut *tx)
            .await
            .context("clear the rows this run has a verdict on")?;
    }
    let (now, tz_offset) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
    let mut stored = Vec::with_capacity(rows.len());
    for (key, outcome, problem) in &rows {
        let row = ProblemRow::new(
            "",
            Stage::Fetch,
            Scope::Entity(key),
            None,
            *outcome,
            problem.clone(),
            None,
        )
        .stamped(earlier.get(key), &now, Some(&tz_offset));
        let sql = crate::bulk::insert_sql::<ProblemRow>();
        // Audited: `sql` is built from `ProblemRow`'s associated consts,
        // never from row data; all values bound.
        row.bind_into(sqlx::query(sqlx::AssertSqlSafe(sql)))
            .execute(&mut *tx)
            .await
            .with_context(|| format!("record {key}"))?;
        stored.push(row);
    }
    tx.commit().await.context("commit")?;
    datalib_problems::note_recorded(&stored);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// strum and serde are independent derives producing independent
    /// strings; the agreement is a real check, not a tautology.
    #[test]
    fn strum_and_serde_agree_on_every_variant() {
        for r in ProblemReason::VARIANTS {
            let serde = serde_json::to_string(r).unwrap();
            let serde = serde.trim_matches('"');
            assert_eq!(serde, r.as_str(), "{r:?}");
            assert_eq!(ProblemReason::parse(serde), Some(*r));
        }
        for k in RunProblemKind::VARIANTS {
            let serde = serde_json::to_string(k).unwrap();
            let serde = serde.trim_matches('"');
            assert_eq!(serde, k.as_str(), "{k:?}");
            assert_eq!(RunProblemKind::parse(serde), Some(*k));
        }
    }

    /// Every kind of row a download writes reads as what it is about,
    /// and the explanation of a run-level one is stored whole rather
    /// than cut at a sample's 80 characters.
    #[test]
    fn every_download_row_says_what_it_is_about() {
        let long = "this org refuses the credential's requests (conversations and \
                    projects); nothing from it is mirrored";
        let runs = run_rows(&[
            RunProblem::forbidden("org:Acme", long),
            RunProblem::phase("devices", "boom"),
        ]);
        assert_eq!(about(&runs[0].0).as_deref(), Some("listing org:Acme"));
        assert_eq!(runs[0].2.sample, long);
        assert_eq!(about(&runs[1].0).as_deref(), Some("the devices phase"));
        let config = config_rows(&[DownloadProblem::not_found(
            "labels",
            "Work",
            "no such label",
        )]);
        assert_eq!(about(&config[0].0).as_deref(), Some("config labels: Work"));
        let records = record_rows(&[RecordProblem::new("messages", "m1", "403")]);
        assert_eq!(about(&records[0].0).as_deref(), Some("messages record m1"));
        let silent = silent_rows(&[SilentEntry {
            name: "porch".into(),
            detail: "nothing since Tuesday".into(),
        }]);
        assert_eq!(about(&silent[0].0).as_deref(), Some("config entry porch"));
        assert_eq!(about("users:u1"), None, "not a download's key");
    }

    #[test]
    fn keeps_the_hits_and_records_the_misses() {
        let specs = vec!["a".to_string(), "nope".to_string(), "b".to_string()];
        let out = resolve_configured("things", &specs, |s| match s {
            "a" | "b" => Ok(s.to_uppercase()),
            _ => Err("known: a, b".to_string()),
        });
        assert_eq!(out.resolved, vec!["A", "B"]);
        assert_eq!(out.problems.len(), 1);
        assert_eq!(out.problems[0].value, "nope");
        assert_eq!(out.problems[0].setting, "things");
        assert!(!out.nothing_resolved());
    }

    /// The distinction the callers branch on. An empty configured list
    /// resolves to nothing and that is fine — it means "no filter". A
    /// list where every entry missed also resolves to nothing, and for a
    /// filter that would silently widen the scope to everything.
    #[test]
    fn tells_an_empty_config_apart_from_a_wholly_unresolvable_one() {
        let none = resolve_configured("things", &[], |s: &str| Ok::<_, String>(s.to_string()));
        assert!(
            !none.nothing_resolved(),
            "no filter configured is not a miss"
        );

        let specs = vec!["nope".to_string()];
        let all_missed = resolve_configured("things", &specs, |_| {
            Err::<String, _>("known: a".to_string())
        });
        assert!(all_missed.nothing_resolved());
        assert!(all_missed.resolved.is_empty());
    }

    #[test]
    fn an_unknown_spelling_is_none_rather_than_a_guess() {
        assert_eq!(ProblemReason::parse("teleported"), None);
    }
}
