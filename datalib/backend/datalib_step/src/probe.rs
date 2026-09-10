//! `datalib-step probe <source_type>` — ask a provider what a set of
//! credentials can reach, without downloading anything.

use anyhow::{Context, Result};

pub async fn run(source_type: &str, params: &serde_json::Value) -> Result<serde_json::Value> {
    match source_type {
        "email" => {
            let config: datalib_etl_email_config::EmailConfig =
                serde_json::from_value(params.clone())
                    .context("parse --params as an email download config")?;
            let report = datalib_etl_email::probe::probe(&config).await?;
            Ok(serde_json::to_value(report)?)
        }
        "claude" => {
            let config: datalib_etl_claude_config::ClaudeConfig =
                serde_json::from_value(params.clone())
                    .context("parse --params as a claude download config")?;
            let report = datalib_etl_claude::probe::probe(&config).await?;
            Ok(serde_json::to_value(report)?)
        }
        other => anyhow::bail!(
            "no probe for source type {other:?}. Probing means asking a live service what an \
             account can reach; only `email` and `claude` implement it so far."
        ),
    }
}

/// The `probe` subcommand end to end: parse `--params`, run the
/// provider's probe, and write the answer where the caller reads it.
/// Never returns on failure.
#[allow(clippy::disallowed_macros)]
pub async fn run_cli(source_type: &str, params_flag: Option<&str>) -> ! {
    let report = async {
        let params = crate::source::parse_params(params_flag)?;
        run(source_type, &params).await
    }
    .await;
    match report {
        Ok(report) => {
            println!("{report}");
            std::process::exit(0)
        }
        Err(e) => {
            for cause in e.chain() {
                eprintln!("error: {cause}");
            }
            std::process::exit(1)
        }
    }
}
