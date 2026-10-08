//! `datalib-step probe <source_type> [--list <list>]` — ask a provider
//! which account a set of credentials reaches, and optionally one list a
//! picker offers, without downloading anything.

use anyhow::{Context, Result};
use datalib_probe::{OnProgress, ProbeAsk, ProbeList};

use crate::source_type::SourceType;

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
        other => anyhow::bail!(
            "no probe for source type `{other}`. Probing means asking a live service what an \
             account can reach; only `calendar`, `contacts`, `email`, `claude`, `chatgpt`, \
             `garmin` and `slack` implement it so far."
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
        let params = crate::source::read_params(params_file)?;
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
