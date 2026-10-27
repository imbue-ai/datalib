//! `render_occurrences`: a calendar source's occurrences in a window of
//! days around the run's now, one row each
//! (`datalib_etl_calendar_render::occurrences`). This is the step
//! around them: the params, the window, the store and its seal.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use datalib_etl::doltlite_raw;
use datalib_etl_calendar_common::occurrences::Window;
use datalib_etl_calendar_config::CalendarOccurrencesConfig;
use datalib_etl_calendar_render::occurrences::{self, store};
use datalib_schema::problems::{Severity, METRIC};

use crate::events::{Emitter, OutputClaim};
use crate::source::StepEnv;
use crate::source_type::SourceType;

/// The step's params, or why a sync would refuse them.
pub fn params(
    env_type: Option<&str>,
    params: serde_json::Value,
) -> Result<CalendarOccurrencesConfig> {
    let source_type = env_type.and_then(SourceType::parse);
    anyhow::ensure!(
        source_type.is_some_and(SourceType::renders_occurrences),
        "`render_occurrences` lists a calendar's occurrences, but this group's type is {}; \
         take the step out of the config",
        env_type.map_or("unset".to_string(), |t| format!("`{t}`"))
    );
    serde_json::from_value(params).context("render_occurrences params")
}

/// From midnight UTC `days_back` days before `now`'s date to the end of
/// the day `days_ahead` after it. Whole days, so every run on one day
/// lists the same occurrences.
pub fn window(now: DateTime<Utc>, cfg: &CalendarOccurrencesConfig) -> Window {
    let today = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc();
    Window {
        from: today - Duration::days(i64::from(cfg.days_back)),
        until: today + Duration::days(i64::from(cfg.days_ahead) + 1),
    }
}

pub async fn run(
    env: &StepEnv,
    data_root: &Path,
    now: &str,
    params_json: serde_json::Value,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let cfg = params(env.group_type.as_deref(), params_json)?;
    let now = DateTime::parse_from_rfc3339(now)
        .with_context(|| format!("the run's now, {now:?}"))?
        .with_timezone(&Utc);
    let window = window(now, &cfg);
    let raw = data_root.join(env.raw_store_rel());
    let Some(listed) = occurrences::read(&raw, &window)? else {
        tracing::info!(raw = %raw.display(), "no raw store yet; nothing to list");
        return Ok(Vec::new());
    };
    for u in &listed.unlisted {
        tracing::warn!(event = %u.event_row, reason = %u.reason, "an event's occurrences could not be listed");
    }
    let progress = emitter.progress();
    for (severity, n) in [
        (Severity::Error, 0),
        (Severity::Warning, listed.unlisted.len()),
    ] {
        progress.metric(METRIC, &[severity.metric_label()], n as i64);
    }

    let db = datalib_etl::layout::render_occurrences_db(data_root, &env.group);
    let pool = store::open(&db).await?;
    let sealed = async {
        store::replace(&pool, &listed, &window).await?;
        doltlite_raw::commit_run(
            &pool,
            &format!(
                "render_occurrences: {} occurrences from {} to {}, {} events unlisted",
                listed.rows.len(),
                window.from.to_rfc3339(),
                window.until.to_rfc3339(),
                listed.unlisted.len()
            ),
        )
        .await?;
        doltlite_raw::head_commit(&pool).await
    }
    .await;
    pool.close().await;
    let head = sealed?.context("the occurrences store has no commit after its seal")?;
    progress.metric("last_pass_occurrences", &[], listed.rows.len() as i64);
    tracing::info!(
        occurrences = listed.rows.len(),
        unlisted = listed.unlisted.len(),
        "the occurrences are current"
    );
    Ok(vec![OutputClaim {
        path: env.step.clone(),
        version: head,
        rows: None,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_is_whole_utc_days_around_now() {
        let now = DateTime::parse_from_rfc3339("2026-10-26T15:09:00-07:00")
            .unwrap()
            .with_timezone(&Utc);
        let w = window(
            now,
            &CalendarOccurrencesConfig {
                days_back: 30,
                days_ahead: 400,
            },
        );
        assert_eq!(w.from.to_rfc3339(), "2026-09-26T00:00:00+00:00");
        assert_eq!(w.until.to_rfc3339(), "2027-12-01T00:00:00+00:00");
    }

    #[test]
    fn params_default_and_refuse_what_they_do_not_know() {
        let cfg = params(Some("calendar"), serde_json::json!({})).unwrap();
        assert_eq!((cfg.days_back, cfg.days_ahead), (30, 400));
        assert!(params(Some("calendar"), serde_json::json!({"horizon": 9})).is_err());
        let err = params(Some("slack"), serde_json::json!({})).unwrap_err();
        assert!(format!("{err:#}").contains("`slack`"), "{err:#}");
    }
}
