//! The mutes store's one writer (`problem_mutes/README.md`): `POST
//! /problems/mute`, and what `/problems` reads to show a row as muted and
//! to keep or leave out the muted ones (`muted:`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use datalib_columns::{Chip, ChipKind};
use datalib_problem_mutes::{MuteRow, MutedHow};
use datalib_unified_document_index::problems::Ids;
use serde::Deserialize;
use sqlx::sqlite::SqlitePool;

use super::Index;

/// The store, opened on the first mute — or at the first read, when an
/// earlier one made it — and held, with its writer lock, from then on.
/// A root nobody muted anything in never gets the file.
#[derive(Clone)]
pub struct Mutes {
    path: Arc<PathBuf>,
    pool: Arc<tokio::sync::Mutex<Option<SqlitePool>>>,
}

impl Mutes {
    pub fn new(data_root: &Path) -> Self {
        Mutes {
            path: Arc::new(datalib_problem_mutes::store_path(data_root)),
            pool: Arc::default(),
        }
    }

    /// The pool, opening the store if `create` or if it is already
    /// there; `None` when it is not and need not be.
    async fn pool(&self, create: bool) -> Result<Option<SqlitePool>> {
        let mut held = self.pool.lock().await;
        if let Some(pool) = held.as_ref() {
            return Ok(Some(pool.clone()));
        }
        if !create && !self.path.exists() {
            return Ok(None);
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("make {}", dir.display()))?;
        }
        let pool = datalib_etl::doltlite_raw::open_curated(
            &self.path,
            datalib_problem_mutes::DDL,
            datalib_store_meta::StoreKind::ProblemMutes,
            &[],
        )
        .await
        .with_context(|| format!("open {}", self.path.display()))?;
        *held = Some(pool.clone());
        Ok(Some(pool))
    }

    pub async fn all(&self) -> Result<HashMap<String, MuteRow>> {
        match self.pool(false).await? {
            Some(pool) => datalib_problem_mutes::read_all(&pool).await,
            None => Ok(HashMap::new()),
        }
    }

    /// Mutes or unmutes `ids` in one commit, which publishes it.
    pub async fn set(&self, ids: &[String], muted: bool, agent: Option<&str>) -> Result<()> {
        let pool = self
            .pool(true)
            .await?
            .context("the mutes store did not open")?;
        let n = ids.len();
        let s = if n == 1 { "" } else { "s" };
        if muted {
            let how = if agent.is_some() {
                MutedHow::Agent
            } else {
                MutedHow::Manual
            };
            let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
            datalib_problem_mutes::mute(&pool, ids, how, agent, &datalib_time::split_stamp(&now))
                .await?;
            datalib_etl::doltlite_raw::commit_run(&pool, &format!("mute {n} problem{s}")).await?;
        } else {
            datalib_problem_mutes::unmute(&pool, ids).await?;
            datalib_etl::doltlite_raw::commit_run(&pool, &format!("unmute {n} problem{s}")).await?;
        }
        Ok(())
    }
}

/// What a `muted:` term asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutedFilter {
    /// `muted:yes`: only the muted rows.
    Only,
    /// `muted:no` or `-muted:yes`: every row but the muted ones.
    Hide,
}

pub const KEY: &str = "muted";
pub const WORDS: &[&str] = &["yes", "no"];

/// `q` with its `muted:` terms taken out, and what they asked for. The
/// index has no column for a mute, so the term is read here; every other
/// token is handed on as written. A value other than yes or no, or two
/// terms that disagree, is refused as a misspelled word is.
pub fn split_muted(q: &str) -> Result<(String, Option<MutedFilter>), String> {
    let mut rest = Vec::new();
    let mut asked: Option<MutedFilter> = None;
    for token in datalib_query::tokenize(q) {
        let term = match datalib_query::parse(&token).into_iter().next() {
            Some(datalib_query::Token::Term(t)) if t.key == KEY => t,
            _ => {
                rest.push(token);
                continue;
            }
        };
        let yes = match term.value.as_str() {
            "yes" => true,
            "no" => false,
            other => return Err(format!("`{KEY}:` takes one of yes, no, not `{other}`")),
        };
        let filter = if yes != term.negate {
            MutedFilter::Only
        } else {
            MutedFilter::Hide
        };
        if asked.is_some_and(|a| a != filter) {
            return Err(format!(
                "`{KEY}:yes` and `{KEY}:no` together keep nothing; use one"
            ));
        }
        asked = Some(filter);
    }
    Ok((rest.join(" "), asked))
}

/// The ids a list keeps for `filter`, given what is muted.
pub fn ids_for(filter: Option<MutedFilter>, mutes: &HashMap<String, MuteRow>) -> Ids {
    let muted = || {
        let mut ids: Vec<String> = mutes.keys().cloned().collect();
        ids.sort();
        ids
    };
    match filter {
        None => Ids::Any,
        Some(MutedFilter::Only) => Ids::Only(muted()),
        Some(MutedFilter::Hide) => Ids::Except(muted()),
    }
}

/// A muted row's chip: who muted it and when, on hover.
pub fn chip(mute: &MuteRow) -> Chip {
    let who = match (mute.muted_how, mute.agent.as_deref()) {
        (MutedHow::Agent, Some(agent)) => format!("the agent {agent}"),
        (MutedHow::Agent, None) => "an agent".to_string(),
        (MutedHow::Manual, _) => "you".to_string(),
    };
    let when = datalib_time::join_stamp(&mute.muted_at_utc, mute.tz_offset.as_deref());
    Chip {
        kind: ChipKind::Metric,
        text: "MUTED".into(),
        title: format!(
            "Muted by {who} at {when}: it counts toward nothing until it changes or is unmuted"
        ),
    }
}

#[derive(Debug, Deserialize)]
pub struct MuteBody {
    pub problem_uuids: Vec<String>,
    /// `false` unmutes.
    pub muted: bool,
    /// The agent muting on a person's behalf; absent for the person.
    pub agent: Option<String>,
}

/// The most ids one request names: more than a grid selects, and short of
/// a body that is a mistake.
const MAX_IDS: usize = 100_000;

/// `POST /problems/mute`: `{problem_uuids, muted, agent?}`.
pub async fn handler(
    State(s): State<Index>,
    Json(body): Json<MuteBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if body.problem_uuids.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "name at least one problem in `problem_uuids`".into(),
        ));
    }
    if body.problem_uuids.len() > MAX_IDS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("at most {MAX_IDS} problems at a time"),
        ));
    }
    let agent = body
        .agent
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty());
    s.mutes
        .set(&body.problem_uuids, body.muted, agent)
        .await
        .map_err(|e| {
            tracing::error!(error = %format!("{e:#}"), "could not change the mutes");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not change the mutes: {e:#}"),
            )
        })?;
    Ok(Json(serde_json::json!({
        "muted": body.muted,
        "problem_uuids": body.problem_uuids,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_muted_term_is_read_here_and_the_rest_handed_on() {
        assert_eq!(
            split_muted("source:enterprise muted:no \"warp core\""),
            Ok((
                "source:enterprise \"warp core\"".to_string(),
                Some(MutedFilter::Hide)
            ))
        );
        assert_eq!(
            split_muted("-muted:yes"),
            Ok((String::new(), Some(MutedFilter::Hide)))
        );
        assert_eq!(
            split_muted("muted:yes severity:error"),
            Ok(("severity:error".to_string(), Some(MutedFilter::Only)))
        );
        assert_eq!(
            split_muted("-muted:no"),
            Ok((String::new(), Some(MutedFilter::Only)))
        );
        assert_eq!(split_muted("source:x"), Ok(("source:x".to_string(), None)));
        assert!(split_muted("muted:maybe").unwrap_err().contains("yes, no"));
        assert!(split_muted("muted:yes muted:no").is_err());
    }
}
