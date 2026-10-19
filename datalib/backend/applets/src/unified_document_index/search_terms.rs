//! Searches answered from the grid's search terms file rather than qmd: the
//! Fields tab, and a search made only of identifiers (a uuid, an email
//! address, a handle) whichever tab asks. The rows that answer to each
//! word, best match first, as deep as the other tabs rank. The file and
//! what it holds:
//! `datalib_etl_render::search_terms`.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_schema::search_terms::{SearchTermKind, META_GRID_COMMIT};
use datalib_unified_document_index::query::{extract_uuid_suffix, is_uuid_shape};
use datalib_unified_document_index::terms_keys::handle_of;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};
use strum::VariantArray;

/// What a search asks the search terms file: FTS5 expressions over the terms'
/// values, each of which a row must answer to, and those it must not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

/// The free text as terms to look up, when every word of it is an
/// identifier; `None` when any word is not.
pub fn identifiers(free_text: &str) -> Option<Match> {
    let words = datalib_query::tokenize(free_text);
    if words.is_empty() {
        return None;
    }
    let include = words
        .iter()
        .map(|w| identifier(w).map(|id| phrase(&id)))
        .collect::<Option<Vec<String>>>()?;
    Some(Match {
        include,
        exclude: Vec::new(),
    })
}

/// The free text as the Fields tab looks it up: an identifier whole, a
/// quoted phrase as typed, any other word as the start of one (so a row
/// shows while its word is still being typed), and a `-` one excluded.
/// `None` when nothing is required.
pub fn fields(free_text: &str) -> Option<Match> {
    let mut m = Match {
        include: Vec::new(),
        exclude: Vec::new(),
    };
    for word in datalib_query::tokenize(free_text) {
        let (bucket, word) = match word.strip_prefix('-').filter(|w| !w.is_empty()) {
            Some(w) => (&mut m.exclude, w.to_string()),
            None => (&mut m.include, word),
        };
        bucket.push(match identifier(&word) {
            Some(id) => phrase(&id),
            None if word.len() >= 2 && word.starts_with('"') && word.ends_with('"') => {
                phrase(&word[1..word.len() - 1])
            }
            None if word.chars().count() < MIN_PREFIX_CHARS => phrase(&word),
            None => format!("{}*", phrase(&word)),
        });
    }
    (!m.include.is_empty()).then_some(m)
}

/// The fewest characters a word needs before the Fields tab takes it as the
/// start of a term; a shorter one starts too many to look up, so it is
/// matched whole.
const MIN_PREFIX_CHARS: usize = 3;

/// `s` as one FTS5 string, a quote inside it doubled so it cannot end it.
fn phrase(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn identifier(word: &str) -> Option<String> {
    // A quoted word is a phrase for qmd, and a quote would end the
    // search terms file's own phrase early.
    if word.contains('"') {
        return None;
    }
    let id = extract_uuid_suffix(word);
    if is_uuid_shape(id) {
        return Some(id.to_lowercase());
    }
    handle_of(word)
}

/// One term an identifier matched.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Hit {
    pub uuid: String,
    /// A [`SearchTermKind`] code.
    pub kind: i64,
    pub value: String,
    pub touched_at_utc: Option<String>,
}

/// Each row [`lookup`] found, best first, scored by the affinity of its
/// best match; the words shown are the kind and the value matched.
pub fn rank(found: &Found) -> Vec<(String, (f64, String))> {
    found
        .hits
        .iter()
        .map(|h| {
            let kind = SearchTermKind::from_code(h.kind);
            let affinity = kind.map_or(0, SearchTermKind::affinity);
            let kind = kind.map_or("term", SearchTermKind::as_str);
            (
                h.uuid.clone(),
                (f64::from(affinity), format!("{kind}: {}", h.value)),
            )
        })
        .collect()
}

/// What the search terms file says about a [`Match`], read in one transaction.
#[derive(Debug, Default)]
pub struct Found {
    /// The rows every included expression matched and no excluded one did,
    /// each at its best match: the most telling kind first
    /// (`SearchTermKind::affinity`), then newest.
    pub hits: Vec<Hit>,
    /// The grid commit the file reflects. A ranking read at any other
    /// commit may be one pass out, so it is not kept.
    pub grid_commit: Option<String>,
}

/// The rows of [`Found::hits`], at most `depth` of them. The file holds a
/// term per row and value, so a short prefix matches a large share of it:
/// the intersecting, ranking and cut happen in SQLite, never in a list here.
const HITS_SQL: &str = "\
    WITH inc AS (SELECT key AS n, value AS expr FROM json_each(?1)), \
    exc AS (SELECT value AS expr FROM json_each(?2)), \
    aff AS (SELECT value ->> 0 AS kind, value ->> 1 AS affinity FROM json_each(?3)), \
    matched AS ( \
        SELECT inc.n, t.row_id, t.kind, v.value FROM inc \
        JOIN vals_fts ON vals_fts MATCH inc.expr \
        JOIN vals v ON v.val_id = vals_fts.rowid \
        JOIN terms t ON t.val_id = v.val_id), \
    kept AS ( \
        SELECT row_id FROM matched GROUP BY row_id \
        HAVING COUNT(DISTINCT n) = (SELECT COUNT(*) FROM inc) \
        EXCEPT \
        SELECT t.row_id FROM exc \
        JOIN vals_fts ON vals_fts MATCH exc.expr \
        JOIN terms t ON t.val_id = vals_fts.rowid), \
    best AS ( \
        SELECT m.row_id, m.kind, m.value, MAX(COALESCE(a.affinity, 0)) AS affinity \
        FROM matched m JOIN kept USING (row_id) LEFT JOIN aff a ON a.kind = m.kind \
        GROUP BY m.row_id) \
    SELECT r.uuid, b.kind, b.value, r.touched_at_utc FROM best b \
    JOIN rows r ON r.row_id = b.row_id \
    ORDER BY b.affinity DESC, r.touched_at_utc DESC, r.uuid \
    LIMIT ?4";

/// `None` when the root has no search terms file yet.
pub async fn lookup(root: &Path, m: &Match, depth: usize) -> Result<Option<Found>> {
    let path = datalib_runtime::layout::search_terms_db(root);
    if !path.exists() {
        return Ok(None);
    }
    let affinities: Vec<(u8, u8)> = SearchTermKind::VARIANTS
        .iter()
        .map(|k| (k.code(), k.affinity()))
        .collect();
    let mut conn = SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(&path))
        .read_only(true)
        .create_if_missing(false)
        .busy_timeout(Duration::from_secs(5))
        .connect()
        .await
        .with_context(|| format!("open {}", path.display()))?;
    let read = async {
        sqlx::query("BEGIN").execute(&mut conn).await?;
        let grid_commit: Option<String> =
            sqlx::query_scalar("SELECT value FROM terms_meta WHERE key = ?")
                .bind(META_GRID_COMMIT)
                .fetch_optional(&mut conn)
                .await?;
        let hits: Vec<Hit> = sqlx::query_as(HITS_SQL)
            .bind(serde_json::to_string(&m.include)?)
            .bind(serde_json::to_string(&m.exclude)?)
            .bind(serde_json::to_string(&affinities)?)
            .bind(i64::try_from(depth).unwrap_or(i64::MAX))
            .fetch_all(&mut conn)
            .await?;
        sqlx::query("COMMIT").execute(&mut conn).await?;
        anyhow::Ok(Found { hits, grid_commit })
    }
    .await
    .context("read the search terms file");
    conn.close().await.ok();
    read.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn includes(m: Option<Match>) -> Option<Vec<String>> {
        m.map(|m| m.include)
    }

    #[test]
    fn identifiers_are_uuids_and_handles_and_nothing_else() {
        assert_eq!(
            includes(identifiers("00000000-0000-8B8A-896D-63addc7b31ad")),
            Some(vec!["\"00000000-0000-8b8a-896d-63addc7b31ad\"".to_string()])
        );
        assert_eq!(
            includes(identifiers(
                "away-team-00000000-0000-4000-8000-000000000001"
            )),
            Some(vec!["\"00000000-0000-4000-8000-000000000001\"".to_string()])
        );
        assert_eq!(
            identifiers("Ann@Example.com +1 555 0100"),
            None,
            "a phone number in several words is not one identifier"
        );
        assert_eq!(
            includes(identifiers("Ann@Example.com slack:T1/U2")),
            Some(vec![
                "\"email:ann@example.com\"".to_string(),
                "\"slack:T1/U2\"".to_string()
            ])
        );
        assert_eq!(identifiers("ann@example.com budget"), None);
        assert_eq!(identifiers("\"ann@example.com\""), None);
        assert_eq!(identifiers(""), None);
    }

    #[test]
    fn fields_take_identifiers_whole_phrases_as_typed_and_words_as_prefixes() {
        assert_eq!(
            fields("Ann@Example.com budg \"deck twelve\" -spam"),
            Some(Match {
                include: vec![
                    "\"email:ann@example.com\"".into(),
                    "\"budg\"*".into(),
                    "\"deck twelve\"".into(),
                ],
                exclude: vec!["\"spam\"*".into()],
            })
        );
        assert_eq!(
            fields("o\"brien").map(|m| m.include),
            Some(vec!["\"o\"\"brien\"*".into()])
        );
        assert_eq!(fields("-spam"), None);
    }

    /// A search terms file at `root` holding `(row, kind, value, touched)`.
    async fn terms_file(root: &Path, terms: &[(&str, &str, &str, &str)]) {
        let path = datalib_runtime::layout::search_terms_db(root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut conn = SqliteConnectOptions::new()
            .filename(datalib_runtime::plain_sqlite::uri(&path))
            .create_if_missing(true)
            .connect()
            .await
            .unwrap();
        for ddl in datalib_schema::search_terms::TERMS_DDL {
            sqlx::query(sqlx::AssertSqlSafe(*ddl))
                .execute(&mut conn)
                .await
                .unwrap();
        }
        for (uuid, kind, value, touched) in terms {
            sqlx::query("INSERT OR IGNORE INTO rows (uuid, touched_at_utc) VALUES (?, ?)")
                .bind(uuid)
                .bind(touched)
                .execute(&mut conn)
                .await
                .unwrap();
            sqlx::query("INSERT OR IGNORE INTO vals (value) VALUES (?)")
                .bind(value)
                .execute(&mut conn)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO vals_fts (rowid, value) SELECT val_id, value FROM vals \
                 WHERE value = ? AND val_id NOT IN (SELECT rowid FROM vals_fts)",
            )
            .bind(value)
            .execute(&mut conn)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO terms (val_id, kind, row_id) SELECT v.val_id, ?, r.row_id \
                 FROM vals v, rows r WHERE v.value = ? AND r.uuid = ?",
            )
            .bind(i64::from(
                SearchTermKind::parse(kind).expect("a kind").code(),
            ))
            .bind(value)
            .bind(uuid)
            .execute(&mut conn)
            .await
            .unwrap();
        }
        conn.close().await.unwrap();
    }

    async fn ranked(root: &Path, q: &str, depth: usize) -> Vec<(String, (f64, String))> {
        let found = lookup(root, &fields(q).expect("a word to look up"), depth)
            .await
            .unwrap()
            .expect("a search terms file");
        rank(&found)
    }

    fn order(ranked: &[(String, (f64, String))]) -> Vec<&str> {
        ranked.iter().map(|(u, _)| u.as_str()).collect()
    }

    #[tokio::test]
    async fn rows_rank_by_their_best_kind_then_newest() {
        let tmp = tempfile::tempdir().unwrap();
        terms_file(
            tmp.path(),
            &[
                ("r-name", "name", "warp", "2026-03"),
                ("r-old", "from", "warp", "2026-01"),
                ("r-new", "from", "warp", "2026-02"),
                ("r-id", "id", "warp", "2025-01"),
                ("r-id", "name", "warp", "2025-01"),
            ],
        )
        .await;
        let ranked = ranked(tmp.path(), "warp", 10).await;
        assert_eq!(order(&ranked), ["r-id", "r-new", "r-old", "r-name"]);
        assert_eq!(ranked[0].1, (5.0, "id: warp".to_string()));
    }

    #[tokio::test]
    async fn rows_must_match_every_included_word_and_no_excluded_one() {
        let tmp = tempfile::tempdir().unwrap();
        terms_file(
            tmp.path(),
            &[
                ("a", "from", "warp", "1"),
                ("b", "from", "warp", "1"),
                ("c", "from", "warp", "1"),
                ("b", "container", "core", "1"),
                ("c", "title", "core", "1"),
                ("c", "name", "breach", "1"),
            ],
        )
        .await;
        assert_eq!(
            order(&ranked(tmp.path(), "warp core -breach", 10).await),
            ["b"]
        );
    }

    /// The cut is taken after the rows are ranked and intersected, so the
    /// rows kept are the best of those that match every word.
    #[tokio::test]
    async fn a_search_keeps_its_best_rows_up_to_the_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let rows: Vec<String> = (0..20).map(|i| format!("r-{i:02}")).collect();
        let mut terms: Vec<(&str, &str, &str, &str)> = rows
            .iter()
            .map(|r| (r.as_str(), "name", "warp", "1"))
            .collect();
        terms.push(("r-19", "id", "warpcore", "1"));
        terms.push(("r-07", "from", "warp", "1"));
        terms.push(("r-19", "title", "deck", "1"));
        terms.push(("r-07", "title", "deck", "1"));
        terms.push(("r-03", "title", "deck", "1"));
        terms_file(tmp.path(), &terms).await;
        assert_eq!(
            order(&ranked(tmp.path(), "warp", 3).await),
            ["r-19", "r-07", "r-00"]
        );
        assert_eq!(
            order(&ranked(tmp.path(), "warp deck", 2).await),
            ["r-19", "r-07"]
        );
    }

    #[test]
    fn a_word_too_short_to_start_many_is_matched_whole() {
        assert_eq!(
            fields("ri x rik é").map(|m| m.include),
            Some(vec![
                "\"ri\"".into(),
                "\"x\"".into(),
                "\"rik\"*".into(),
                "\"é\"".into(),
            ])
        );
    }
}
