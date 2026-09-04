//! `datalib-step probe <source_type>` — ask a provider what a set of
//! credentials can reach, without downloading anything.
//!
//! **This is not a pipeline step.** It is a utility the UI shells out
//! to, like `synthesize`: no data root, no outputs, no NDJSON event
//! stream. It writes one JSON object to stdout and exits — success on
//! stdout, failure on stderr with a non-zero status — because the one
//! caller is a program (`datalib-http`'s `POST /api/probe`) that has to
//! forward the answer to a browser.
//!
//! Two things depend on that plainness, so don't "fix" it into the step
//! contract: a probe runs against a config that has never synced (there
//! may be no tree for it on disk at all), and it runs while someone is
//! still filling in a form, so the answer has to come back in seconds
//! and be parseable without an event reader.
//!
//! Only `email` has one today. Adding another is a match arm plus the
//! provider-side function it calls; the wizard turns the button on with
//! `canProbe` in `ui/src/config/catalog.ts`.

use anyhow::{Context, Result};

/// Run the probe for `source_type` and return its report as JSON.
///
/// `params` is the provider's **download** config subtree — the same
/// thing `datalib-step download <type>` takes. Render-phase probing
/// goes through here too, with the download params of the step being
/// rendered: a render step's own params carry no credentials, and the
/// labels a render filter can name are the ones the account has.
pub async fn run(source_type: &str, params: &serde_json::Value) -> Result<serde_json::Value> {
    match source_type {
        "email" => {
            let config: datalib_etl_email_config::EmailConfig =
                serde_json::from_value(params.clone())
                    .context("parse --params as an email download config")?;
            let report = datalib_etl_email::probe::probe(&config).await?;
            Ok(serde_json::to_value(report)?)
        }
        other => anyhow::bail!(
            "no probe for source type {other:?}. Probing means asking a live service what an \
             account can reach; only `email` implements it so far."
        ),
    }
}

/// The `probe` subcommand end to end: parse `--params`, run the
/// provider's probe, and write the answer where the caller reads it.
/// Never returns on failure.
///
/// `println!`/`eprintln!` are banned workspace-wide because they
/// corrupt the progress display of a step that draws bars. This
/// command draws none — it *is* a stdout sink, which is the exemption
/// the lint's own note describes — so the allow sits here, on the one
/// function that needs it, rather than on the binary.
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
