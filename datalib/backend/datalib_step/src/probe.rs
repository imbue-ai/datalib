//! `datalib-step probe <source_type> [--method <m>] [--list <list>]` —
//! ask a provider which account a set of credentials reaches, and
//! optionally one list a picker offers, without downloading anything.

use anyhow::{Context, Result};
use datalib_probe::{OnProgress, ProbeAsk, ProbeList};
use datalib_source_common::Reach;

use crate::source_type::SourceType;

/// The params with the way in a probe asks through: `method` when one is
/// named, else the one the params hold, else the source's only live one.
/// A source with several live ways in and none named is refused, naming
/// them.
pub fn with_method(
    source_type: SourceType,
    params: &serde_json::Value,
    method: Option<&str>,
) -> Result<serde_json::Value> {
    let live: Vec<&str> = crate::methods::ingest_methods(source_type)
        .iter()
        .filter(|m| m.reach == Reach::Origin)
        .map(|m| m.path)
        .collect();
    let held: Vec<&str> = crate::methods::held(params, crate::methods::ingest_methods(source_type))
        .iter()
        .filter(|m| m.reach == Reach::Origin)
        .map(|m| m.path)
        .collect();
    let named = |list: &[&str]| {
        list.iter()
            .map(|p| format!("`{p}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let pick = match (method, held.as_slice(), live.as_slice()) {
        (Some(m), _, _) if !live.contains(&m) => anyhow::bail!(
            "a {source_type} source has no live way in called `{m}`; it has {}",
            named(&live)
        ),
        (Some(m), [h], _) if *h != m => {
            anyhow::bail!("`--method {m}`, but the params file selects `{h}`; name one of them")
        }
        (Some(m), _, _) => m,
        (None, [_, ..], _) => return Ok(params.clone()),
        (None, [], [only]) => only,
        (None, [], _) => anyhow::bail!(
            "a {source_type} source can be reached {} ways ({}); name one with \
             `--method`, e.g. `datalib-step probe {source_type} --method {}`",
            live.len(),
            named(&live),
            live.first().copied().unwrap_or_default()
        ),
    };
    let mut params = match params {
        serde_json::Value::Object(_) => params.clone(),
        _ => serde_json::json!({}),
    };
    if let Some(table) = params.as_object_mut() {
        table.entry(pick).or_insert_with(|| serde_json::json!({}));
    }
    Ok(params)
}

pub async fn run(
    source_type: SourceType,
    params: &serde_json::Value,
    ask: ProbeAsk,
    progress: OnProgress<'_>,
) -> Result<serde_json::Value> {
    let mut params = params.clone();
    crate::methods::drop_inert_params(&mut params);
    match source_type {
        SourceType::Email => {
            let config: datalib_etl_email_config::EmailConfig =
                serde_json::from_value(params.clone())
                    .context("parse the params as an email download config")?;
            let report = datalib_etl_email::probe::probe(&config, ask).await?;
            Ok(serde_json::to_value(report)?)
        }
        SourceType::Claude => {
            let config: datalib_etl_claude_config::ClaudeConfig =
                serde_json::from_value(params.clone())
                    .context("parse the params as a claude download config")?;
            let report = datalib_etl_claude::probe::probe(&config, ask, progress).await?;
            Ok(serde_json::to_value(report)?)
        }
        SourceType::Chatgpt => {
            let config: datalib_etl_chatgpt_config::ChatgptConfig =
                serde_json::from_value(params.clone())
                    .context("parse the params as a chatgpt download config")?;
            let report = datalib_etl_chatgpt::probe::probe(&config, ask, progress).await?;
            Ok(serde_json::to_value(report)?)
        }
        SourceType::Calendar => {
            let config: datalib_etl_calendar_config::CalendarConfig =
                serde_json::from_value(params.clone())
                    .context("parse the params as a calendar download config")?;
            let report = datalib_etl_calendar::probe::probe(&config, ask).await?;
            Ok(serde_json::to_value(report)?)
        }
        SourceType::Contacts => {
            let config: datalib_etl_contacts_config::ContactsConfig =
                serde_json::from_value(params.clone())
                    .context("parse the params as a contacts download config")?;
            let report = datalib_etl_contacts::probe::probe(&config, ask).await?;
            Ok(serde_json::to_value(report)?)
        }
        SourceType::Slack => {
            let config: datalib_etl_slack_config::SlackConfig =
                serde_json::from_value(params.clone())
                    .context("parse the params as a slack download config")?;
            let report = datalib_etl_slack::probe::probe(&config, ask, progress).await?;
            Ok(serde_json::to_value(report)?)
        }
        SourceType::Garmin => {
            let config: datalib_etl_garmin_config::GarminConfig =
                serde_json::from_value(params.clone())
                    .context("parse the params as a garmin download config")?;
            let report = datalib_etl_garmin::probe::probe(&config, ask).await?;
            Ok(serde_json::to_value(report)?)
        }
        SourceType::Linear => {
            let config: datalib_etl_linear_config::LinearConfig =
                serde_json::from_value(params.clone())
                    .context("parse the params as a linear download config")?;
            let report = datalib_etl_linear::probe::probe(&config, ask, progress).await?;
            Ok(serde_json::to_value(report)?)
        }
        SourceType::Zoom => {
            let config: datalib_etl_zoom_config::ZoomConfig =
                serde_json::from_value(params.clone())
                    .context("parse the params as a zoom download config")?;
            let report = datalib_etl_zoom::probe::probe(&config, ask).await?;
            Ok(serde_json::to_value(report)?)
        }
        other => anyhow::bail!(
            "no probe for source type `{other}`. Probing means asking a live service what an \
             account can reach; only `calendar`, `contacts`, `email`, `claude`, `chatgpt`, \
             `garmin`, `linear`, `slack` and `zoom` implement it so far."
        ),
    }
}

/// How a probe treats a failed request: it reports it. A check is
/// something a person is waiting on and can press again, so the sync's
/// patience — backing off for minutes through a 503 or a dropped
/// network — would only hold the dialog up.
fn one_attempt() -> std::sync::Arc<datalib_etl_web::retry::RetryGuard> {
    datalib_etl_web::retry::RetryGuard::new(
        std::time::Duration::from_secs(30),
        1,
        std::time::Duration::from_secs(1),
        std::time::Duration::from_secs(1),
        datalib_etl::stop::StopFlag::default(),
    )
}

/// The `probe` subcommand end to end: read the params file, run the
/// provider's probe, and write the answer where the caller reads it —
/// the report on stdout, or on failure `{"failure": {issue, detail}}`
/// there and the error chain on stderr. Never returns.
#[allow(clippy::disallowed_macros)]
pub async fn run_cli(
    source_type: &str,
    method: Option<&str>,
    list: Option<&str>,
    params_file: Option<&std::path::Path>,
) -> ! {
    let report = async {
        let source_type = SourceType::parse(source_type).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown source type {source_type:?}; known types: {}",
                SourceType::known_list()
            )
        })?;
        let ask = match list {
            None => ProbeAsk::Account,
            Some(name) => ProbeAsk::List(ProbeList::parse(name).ok_or_else(|| {
                anyhow::anyhow!("no list called {name:?} — a probe lists one of a picker's lists")
            })?),
        };
        let params = with_method(
            source_type,
            &crate::source::read_params(params_file)?,
            method,
        )?;
        let progress = |p: datalib_probe::ProbeProgress| eprintln!("{}", p.line());
        datalib_etl_web::retry::scope(one_attempt(), run(source_type, &params, ask, &progress))
            .await
    }
    .await;
    match report {
        Ok(report) => {
            println!("{report}");
            std::process::exit(0)
        }
        Err(e) => {
            let chain: Vec<String> = e.chain().map(|c| c.to_string()).collect();
            for cause in &chain {
                eprintln!("error: {cause}");
            }
            let gateway = std::env::var_os("LATCHKEY_GATEWAY").is_some_and(|g| !g.is_empty());
            let failure = datalib_probe::issue::Failure::from_text(chain.join("\n"), gateway);
            println!("{}", serde_json::json!({ "failure": failure }));
            std::process::exit(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `datalib-step probe slack`, as the help writes it, failed: the
    /// params named no way in. Slack has one, so that is the one.
    #[test]
    fn a_source_with_one_live_way_in_needs_no_params() {
        assert_eq!(
            with_method(SourceType::Slack, &json!({}), None).unwrap(),
            json!({ "api": {} })
        );
    }

    /// Email has two; the probe has to be told which, and says how.
    #[test]
    fn a_source_with_two_asks_which() {
        let err = with_method(SourceType::Email, &json!({}), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("`gmail`") && err.contains("`jmap`"), "{err}");
        assert!(err.contains("--method"), "{err}");
        assert_eq!(
            with_method(SourceType::Email, &json!({}), Some("gmail")).unwrap(),
            json!({ "gmail": {} })
        );
    }

    /// What the params file says stands; `--method` only adds a table.
    #[test]
    fn the_params_file_keeps_its_settings() {
        let params = json!({ "gmail": { "user_id": "me" }, "only_extract_labels": ["Inbox"] });
        assert_eq!(
            with_method(SourceType::Email, &params, None).unwrap(),
            params
        );
        assert_eq!(
            with_method(SourceType::Email, &params, Some("gmail")).unwrap(),
            params
        );
        let err = with_method(SourceType::Email, &params, Some("jmap"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("selects `gmail`"), "{err}");
    }

    /// A file-reading way in has no connection to test.
    #[test]
    fn refuses_a_method_that_reads_files() {
        let err = with_method(SourceType::Email, &json!({}), Some("mbox"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no live way in called `mbox`"), "{err}");
    }
}
