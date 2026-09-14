//! `datalib-step login <source_type>` — sign in to a service whose
//! credential datalib holds itself, outside latchkey. Interactive: it
//! prompts on the terminal, so nothing in the pipeline calls it.

use anyhow::Result;

use crate::source_type::SourceType;

pub async fn run(
    source_type: SourceType,
    token_dir: Option<&str>,
    email: Option<&str>,
    domain: &str,
) -> Result<String> {
    match source_type {
        SourceType::Garmin => {
            let dir = datalib_etl_garmin::auth::expand_token_dir(token_dir);
            datalib_etl_garmin::login::login_interactive(&dir, domain, email).await?;
            Ok(format!(
                "signed in; tokens written under {}. The OAuth1 token lasts about a year; \
                 the ingest step refreshes the bearer from it on its own.",
                dir.display()
            ))
        }
        other => anyhow::bail!(
            "no login for source type `{other}`. Every other live source authenticates through \
             latchkey (`latchkey auth browser <service>` or `latchkey auth set <service> …`); \
             only `garmin` keeps a credential of its own."
        ),
    }
}

/// The `login` subcommand end to end. Never returns.
#[allow(clippy::disallowed_macros)]
pub async fn run_cli(
    source_type: &str,
    token_dir: Option<&str>,
    email: Option<&str>,
    domain: &str,
) -> ! {
    let outcome = async {
        let source_type = SourceType::parse(source_type).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown source type {source_type:?}; known types: {}",
                SourceType::known_list()
            )
        })?;
        run(source_type, token_dir, email, domain).await
    }
    .await;
    match outcome {
        Ok(msg) => {
            println!("{msg}");
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
