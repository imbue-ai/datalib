//! A configured entry the upstream does not have.
//!
//! Distinct from the per-item transient failures `FetchSummary` counts as
//! `errors`. Those are 5xx, timeouts and blips: retrying fixes them, so
//! they are counted and forgotten. One of these never resolves on its own
//! — a label that does not exist will not exist next run either — so it is
//! reported every run until someone corrects the config.
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

/// One `warn!` per problem, in a shape every provider shares so a reader
/// grepping `download_problem` finds all of them.
pub fn report(problems: &[DownloadProblem]) {
    for p in problems {
        tracing::warn!(
            event = "download_problem",
            setting = %p.setting,
            value = %p.value,
            reason = p.reason.as_str(),
            detail = %p.detail,
            "a configured entry does not exist upstream; continuing without it",
        );
    }
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
