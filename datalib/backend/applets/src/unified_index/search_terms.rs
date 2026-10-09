//! Searches answered from the grid's search terms file rather than qmd: the
//! Fields tab, and a search made only of identifiers (a uuid, an email
//! address, a handle) whichever tab asks. Every row that answers to each
//! word, best match first. The file and what it holds:
//! `datalib_etl_render::search_terms`.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_handle::Handle;
use datalib_schema::search_terms::{SearchTermKind, META_GRID_COMMIT};
use datalib_unified_index::query::{extract_uuid_suffix, is_uuid_shape};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};

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
            None => format!("{}*", phrase(&word)),
        });
    }
    (!m.include.is_empty()).then_some(m)
}

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
    let handle = if word.contains(':') {
        Handle::rebuild(word)
    } else if word.contains('@') {
        Handle::email(word)
    } else if word.starts_with('+') {
        Handle::tel(word)
    } else {
        None
    };
    handle.map(|h| h.as_str().to_string())
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

/// The rows every included expression matched and no excluded one did,
/// each at its best match: the most telling kind first
/// (`SearchTermKind::affinity`), then newest. The score is that affinity, and the
/// words shown are the kind and the value matched.
pub fn rank(found: &Found) -> Vec<(String, (f64, String))> {
    let excluded: std::collections::HashSet<&str> = found
        .excluded
        .iter()
        .flatten()
        .map(|h| h.uuid.as_str())
        .collect();
    let per_identifier = &found.included;
    let affinity = |h: &Hit| SearchTermKind::from_code(h.kind).map_or(0, SearchTermKind::affinity);
    let kind = |h: &Hit| SearchTermKind::from_code(h.kind).map_or("term", SearchTermKind::as_str);
    let Some((first, rest)) = per_identifier.split_first() else {
        return Vec::new();
    };
    let mut best: std::collections::HashMap<&str, &Hit> = std::collections::HashMap::new();
    for hit in first {
        let in_every = rest
            .iter()
            .all(|hits| hits.iter().any(|h| h.uuid == hit.uuid));
        if !in_every || excluded.contains(hit.uuid.as_str()) {
            continue;
        }
        let slot = best.entry(hit.uuid.as_str()).or_insert(hit);
        if affinity(hit) > affinity(slot) {
            *slot = hit;
        }
    }
    let mut ranked: Vec<&Hit> = best.into_values().collect();
    ranked.sort_by(|a, b| {
        affinity(b)
            .cmp(&affinity(a))
            .then_with(|| b.touched_at_utc.cmp(&a.touched_at_utc))
            .then_with(|| a.uuid.cmp(&b.uuid))
    });
    ranked
        .into_iter()
        .map(|h| {
            (
                h.uuid.clone(),
                (f64::from(affinity(h)), format!("{}: {}", kind(h), h.value)),
            )
        })
        .collect()
}

/// What the search terms file says about a [`Match`], read in one transaction:
/// the hits of each expression, in its order.
#[derive(Debug, Default)]
pub struct Found {
    pub included: Vec<Vec<Hit>>,
    pub excluded: Vec<Vec<Hit>>,
    /// The grid commit the file reflects. A ranking read at any other
    /// commit may be one pass out, so it is not kept.
    pub grid_commit: Option<String>,
}

/// `None` when the root has no search terms file yet.
pub async fn lookup(root: &Path, m: &Match) -> Result<Option<Found>> {
    let path = datalib_runtime::layout::search_terms_db(root);
    if !path.exists() {
        return Ok(None);
    }
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
        let mut found = Found {
            grid_commit,
            ..Found::default()
        };
        for (exprs, into) in [
            (&m.include, &mut found.included),
            (&m.exclude, &mut found.excluded),
        ] {
            for expr in exprs {
                let hits: Vec<Hit> = sqlx::query_as(
                    "SELECT r.uuid, t.kind, v.value, r.touched_at_utc FROM vals_fts \
                     JOIN vals v ON v.val_id = vals_fts.rowid \
                     JOIN terms t ON t.val_id = v.val_id \
                     JOIN rows r ON r.row_id = t.row_id WHERE vals_fts MATCH ?",
                )
                .bind(expr)
                .fetch_all(&mut conn)
                .await?;
                into.push(hits);
            }
        }
        sqlx::query("COMMIT").execute(&mut conn).await?;
        anyhow::Ok(found)
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

    fn hit(uuid: &str, kind: &str, touched: &str) -> Hit {
        Hit {
            uuid: uuid.into(),
            kind: i64::from(SearchTermKind::parse(kind).expect("a kind").code()),
            value: "v".into(),
            touched_at_utc: Some(touched.into()),
        }
    }

    fn found(included: Vec<Vec<Hit>>, excluded: Vec<Vec<Hit>>) -> Found {
        Found {
            included,
            excluded,
            grid_commit: None,
        }
    }

    #[test]
    fn rows_rank_by_their_best_kind_then_newest() {
        let ranked = rank(&found(
            vec![vec![
                hit("r-name", "name", "2026-03"),
                hit("r-old", "from", "2026-01"),
                hit("r-new", "from", "2026-02"),
                hit("r-id", "id", "2025-01"),
                hit("r-id", "name", "2026-04"),
            ]],
            Vec::new(),
        ));
        let order: Vec<&str> = ranked.iter().map(|(u, _)| u.as_str()).collect();
        assert_eq!(order, ["r-id", "r-new", "r-old", "r-name"]);
        assert_eq!(ranked[0].1, (5.0, "id: v".to_string()));
    }

    #[test]
    fn rows_must_match_every_included_word_and_no_excluded_one() {
        let ranked = rank(&found(
            vec![
                vec![
                    hit("a", "from", "1"),
                    hit("b", "from", "1"),
                    hit("c", "from", "1"),
                ],
                vec![hit("b", "container", "1"), hit("c", "title", "1")],
            ],
            vec![vec![hit("c", "name", "1")]],
        ));
        let order: Vec<&str> = ranked.iter().map(|(u, _)| u.as_str()).collect();
        assert_eq!(order, ["b"]);
    }
}
