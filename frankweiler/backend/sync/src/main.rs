//! `frankweiler-sync` — config-driven ETL orchestrator.
//!
//! Drives Extract → Translate → Load → Archive across every
//! enabled source in the user's `~/.config/frankweiler/config.yaml`.
//! Each source is dispatched on its `type:` discriminator; sources with
//! a `sync:` block (the "managed" ones) get their downloader invoked,
//! the others are translate-only against pre-staged `input_path`.
//!
//! ```sh
//! FRANKWEILER_CONFIG=$(pwd)/configs/thad_dev.yaml \
//!   bazelisk run //frankweiler/backend/sync:frankweiler_sync_bin -- \
//!     --now "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
//! ```
//!
//! Modes:
//!
//!   * `--synthesize-playback-root <DIR>`: walk each source's
//!     `input_path` (interpreted in this mode as a checked-in raw fixture
//!     tree) and write HTTP playback fixtures to `<DIR>`. Independent of
//!     translate/load — exits after synth. Used by the Bazel genrule to
//!     turn checked-in JSONL into replay tapes.
//!   * `--playback-root <DIR>`: redirect every provider's HTTP transport
//!     to `<DIR>` (via `FRANKWEILER_HTTP_PLAYBACK`) and run extract for
//!     each managed source into its resolved `input_path`. Used by the
//!     hermetic Bazel genrule.
//!   * `--skip-extract`: skip the extract phase entirely and translate
//!     against pre-staged `input_path`s. Useful for iterating on
//!     translate/load without re-hitting the network.
//!   * default: extract live from every managed source's provider API,
//!     translate, load into a scratch Dolt repo, emit `dolt_repo/` +
//!     the configured Dolt repo at `<data_root>/dolt_db/`, write the
//!     rendered markdown tree to `<data_root>/rendered_md/`, and (unless
//!     `qmd.skip`) build the qmd index at `<data_root>/qmd/index.sqlite`.
//!     SQL dumping (if needed) is downstream — e.g. a Bazel genrule that
//!     consumes `dolt_db/` and runs `dolt dump`.
//!
//! Extract runs concurrently across managed sources when
//! `sync.parallel: true` (the default); translate/load remain sequential
//! since they share a single Dolt repo and `rendered_md/` tree.

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use frankweiler_core::config::{
    load_config, ChatgptApiSync, ClaudeApiSync, Config, GithubApiSync, GitlabApiSync,
    NotionApiSync, SlackApiSync, SourceConfig,
};
use frankweiler_etl::http::{HttpResponse, PLAYBACK_ENV};
use frankweiler_etl::load::{init_schema, load_all};
use frankweiler_etl::progress::{FanOut, Progress, TracingSink};
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_anthropic::synthesize::AnthropicSynth;
use frankweiler_etl_chatgpt::synthesize::ChatgptSynth;
use frankweiler_etl_github::synthesize::GithubSynth;
use frankweiler_etl_gitlab::synthesize::GitlabSynth;
use frankweiler_etl_notion::synthesize::NotionSynth;
use frankweiler_etl_slack::synthesize::SlackSynth;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::task::JoinSet;

mod progress;
mod summary;
use crate::progress::{make_bar, make_multi, IndicatifSink};
use crate::summary::{ErrorKind, PhaseOutcome, Status, SyncSummary};

// `FRANKWEILER_VERSION` is the output of `git describe --tags --always
// --dirty` at build time, set by either
//   - Bazel: rustc_env.txt resolves {STABLE_GIT_DESCRIBE} from
//     tools/workspace_status.sh; or
//   - cargo: build.rs runs the same `git describe` (falls back to
//     "unknown" outside a git checkout).
// Both paths guarantee the env is set, so `env!` (compile-time) works
// without needing `option_env!` + a fallback const. Exact-tag commits
// render as "v0.1.2"; mid-development commits as "v0.1.2-3-gabc123d".

#[derive(Debug, Parser)]
#[command(
    name = "frankweiler-sync",
    version = env!("FRANKWEILER_VERSION"),
    about = "Config-driven ETL: extract every enabled source, translate, load into Dolt at <data_root>/dolt_db/ + rendered_md/ + qmd/index.sqlite"
)]
struct Args {
    /// Path to the YAML config. Defaults to `$FRANKWEILER_CONFIG` or
    /// `~/.config/frankweiler/config.yaml`. See `frankweiler_core::config`.
    #[arg(long, env = "FRANKWEILER_CONFIG")]
    config: Option<PathBuf>,

    /// Fixed "now" timestamp threaded through downstream renderers and
    /// the dolt load. ISO-8601 / RFC-3339; required for deterministic
    /// builds and for the Bazel genrule. If omitted, the local system
    /// clock is sampled once at startup (with local TZ offset) and used
    /// for the whole run.
    #[arg(long)]
    now: Option<String>,

    /// Run extract against this HTTP playback fixture tree instead of
    /// the network. Required for hermetic Bazel builds; outside of those
    /// the worker hits the real provider APIs.
    #[arg(long)]
    playback_root: Option<PathBuf>,

    /// Skip the extract phase and translate against pre-staged
    /// `input_path`s. Useful for iterating on translate/load without
    /// re-hitting the network.
    #[arg(long)]
    skip_extract: bool,

    /// Synth-only mode: build HTTP playback fixtures for every source
    /// (reading from each source's `input_path`) and exit. Doesn't load
    /// or dump.
    #[arg(long)]
    synthesize_playback_root: Option<PathBuf>,

    /// Forward-compat assertion. Today the binary is always deterministic
    /// given a fixed `--now`.
    #[arg(long, default_value_t = true)]
    deterministic: bool,

    #[command(flatten)]
    obs: frankweiler_obs::ObsArgs,
}

#[tokio::main]
async fn main() {
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "full");
    }
    // Parse args first so --log-level / --otlp-endpoint take effect.
    // Re-parsed inside `run`; clap is cheap and lets us keep the existing
    // `run(&summary)` signature instead of threading Args through.
    let early_args = <Args as clap::Parser>::parse();
    let _obs_guard = match frankweiler_obs::init(&early_args.obs, "frankweiler-sync") {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("[frankweiler-sync] tracing init failed: {e}");
            None
        }
    };

    let summary = Arc::new(Mutex::new(SyncSummary::new()));
    let start = Instant::now();

    // Ctrl-C: best-effort flush of the summary before exit. We can't
    // join the running task graph from here cleanly, so we accept that
    // mid-flight extract work is abandoned. The summary still captures
    // every source that finished (or failed) before the signal.
    let s_sig = summary.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n[frankweiler-sync] caught Ctrl-C; writing partial summary…");
            let mut s = s_sig.lock().unwrap();
            s.interrupted = true;
            s.finalize(start);
            match s.write() {
                Ok(Some(p)) => {
                    eprintln!("[frankweiler-sync] summary: {}", summary::pretty_path(&p))
                }
                Ok(None) => eprintln!("[frankweiler-sync] summary: <no data_root yet>"),
                Err(e) => eprintln!("[frankweiler-sync] failed to write summary: {e}"),
            }
            std::process::exit(130);
        }
    });

    let fatal: Option<anyhow::Error> = run(&summary).await.err();

    let mut s = summary.lock().unwrap();
    if let Some(e) = fatal.as_ref() {
        s.fatal_error = Some(
            e.chain()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(": "),
        );
    }
    s.finalize(start);

    // Print per-error auth hints based on the collected outcomes
    // (rather than only on a single bubbled-up error). The user reads
    // these alongside the JSON summary path.
    for outcome in s.extract.iter().chain(s.translate.iter()) {
        if outcome.status == Status::Error {
            eprintln!(
                "\n[{}] {} ({}): {}",
                outcome.error_kind.map(|k| k.as_str()).unwrap_or("error"),
                outcome.name,
                outcome.type_str,
                outcome.error.as_deref().unwrap_or(""),
            );
            if outcome.error_kind == Some(ErrorKind::Auth) {
                eprintln!("--- auth hint ---");
                eprintln!("{}", auth_hint_for(&outcome.type_str));
            }
        }
    }
    if let Some(e) = fatal.as_ref() {
        render_error(e);
    }

    match s.write() {
        Ok(Some(p)) => eprintln!("\n[frankweiler-sync] summary: {}", summary::pretty_path(&p)),
        Ok(None) => eprintln!("\n[frankweiler-sync] summary: <not written; no data_root>"),
        Err(e) => eprintln!("\n[frankweiler-sync] failed to write summary: {e}"),
    }

    let any_phase_err = s.extract.iter().any(|o| o.status == Status::Error)
        || s.translate.iter().any(|o| o.status == Status::Error)
        || s.load.as_ref().is_some_and(|l| l.error.is_some())
        || s.qmd_index
            .as_ref()
            .is_some_and(|o| o.status == Status::Error);
    let code = if fatal.is_some() {
        1
    } else if any_phase_err {
        2
    } else {
        0
    };
    std::process::exit(code);
}

/// Walk the anyhow error chain top-to-bottom (so the user reads
/// "extract foo (type=bar)" → "fetch /me" → "HTTP 403 …" in order) and,
/// when the failure looks auth-related, append source-specific
/// instructions for fixing latchkey credentials.
fn render_error(e: &anyhow::Error) {
    eprintln!("\n[frankweiler-sync] FAILED");
    for (i, cause) in e.chain().enumerate() {
        let prefix = if i == 0 { "error:" } else { "  caused by:" };
        eprintln!("{prefix} {cause}");
    }
    let chain_text: String = e
        .chain()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    if looks_like_auth_failure(&chain_text) {
        if let Some(provider) = extract_provider_type(&chain_text) {
            eprintln!("\n--- auth hint ---");
            eprintln!("{}", auth_hint_for(provider));
        } else {
            eprintln!("\n--- auth hint ---");
            eprintln!("{GENERIC_AUTH_HINT}");
        }
    }
}

fn looks_like_auth_failure(s: &str) -> bool {
    // Note: `cf-mitigated=None` is the literal Debug rendering of an
    // absent header (we format the response with `{:?}` on
    // `Option<&str>`), so a substring match on "cf-mitigated" alone
    // triggers on every non-200 chatgpt response — including transient
    // HTTP 500s — and pushes the user toward a useless re-auth dance.
    // Only treat cf-mitigated as auth-related when it's actually set
    // (i.e. `Some(...)`), or when we got a true 401/403 status.
    s.contains("HTTP 401")
        || s.contains("HTTP 403")
        || s.contains("Unauthorized")
        || s.contains("Forbidden")
        || s.contains("cf-mitigated=Some(")
}

fn extract_provider_type(s: &str) -> Option<&'static str> {
    // The `with_context` strings include "(type=<type_str>)" — pull
    // that out so we can print the right hint.
    for marker in [
        "type=claude_api",
        "type=chatgpt_api",
        "type=slack_api",
        "type=github_api",
        "type=gitlab_api",
        "type=notion_api",
    ] {
        if s.contains(marker) {
            return Some(&marker["type=".len()..]);
        }
    }
    None
}

const GENERIC_AUTH_HINT: &str = "Provider returned an auth-failure status. \
This usually means latchkey credentials are missing or expired. \
See <provider>/EXTRACT.md for setup. Confirm the in-tree curl shim is \
built (`cargo build -p frankweiler-etl --bin latchkey-curl-shim`), or \
set $FRANKWEILER_CURL_SHIM / $LATCHKEY_CURL explicitly, and that \
`latchkey auth list` shows entries.";

fn auth_hint_for(provider: &str) -> String {
    match provider {
        // All hints route the secret through the macOS clipboard so it
        // never lands in shell history: a one-liner copies the token to
        // the pasteboard, then the printed `latchkey auth set …` command
        // expands `$(pbpaste)` at exec time. zsh/bash record the literal
        // `$(pbpaste)`, not the resolved value.
        "chatgpt_api" => "\
chatgpt access token expired or missing.

  1. Open https://chatgpt.com in a logged-in browser, then in DevTools
     console run (clipboard write needs page focus, so it waits for a
     click on the page):
       const r = await fetch('/api/auth/session');
       const j = await r.json();
       addEventListener('click', async () => {
         await navigator.clipboard.writeText(j.accessToken);
         console.log('  latchkey auth set chatgpt -H \"Authorization: Bearer $(pbpaste)\"');
       }, { once: true });
     Then click anywhere on the chatgpt page; the console prints the
     command to run.
  2. Paste the printed `latchkey auth set …` line into your shell and
     run it. zsh/bash record the literal `$(pbpaste)`, not the resolved
     token, so the secret never lands in shell history.
  3. Smoke-test:
       latchkey curl -s https://chatgpt.com/backend-api/me | head -c 200
     Expect a JSON object with your account id. If you still see a
     Cloudflare challenge, copy `cf_clearance` from DevTools → Application
     → Cookies → chatgpt.com and add a second `-H \"Cookie: cf_clearance=$(pbpaste)\"`
     to the `latchkey auth set chatgpt` call.

See frankweiler/backend/etl/providers/chatgpt/EXTRACT.md for details."
            .into(),
        "claude_api" => "\
anthropic sessionKey expired or missing.

  1. Open https://claude.ai logged in. In DevTools → Application →
     Cookies → claude.ai, copy the `sessionKey` value to the clipboard.
  2. Run (uses `$(pbpaste)` so the token isn't recorded in shell history):
       latchkey auth set claude-ai -H \"Cookie: sessionKey=$(pbpaste)\"
  3. Smoke-test:
       latchkey curl -s https://claude.ai/api/organizations | head -c 200

See frankweiler/backend/etl/providers/anthropic/EXTRACT.md for details."
            .into(),
        "slack_api" => "\
slack token expired or missing.

  1. Grab a user-scope OAuth token (xoxc/xoxp/xoxd) and copy it to the
     clipboard.
  2. Run (uses `$(pbpaste)` so the token isn't recorded in shell history):
       latchkey auth set slack -H \"Authorization: Bearer $(pbpaste)\"
  3. Smoke-test:
       latchkey curl -s https://slack.com/api/auth.test | head -c 200

See frankweiler/backend/etl/providers/slack/EXTRACT.md for details."
            .into(),
        "github_api" => "\
github PAT expired or missing.

  1. Create a fine-grained PAT at https://github.com/settings/tokens
     with `repo` + `read:user` scopes; copy it to the clipboard.
  2. Run (uses `$(pbpaste)` so the token isn't recorded in shell history):
       latchkey auth set github -H \"Authorization: Bearer $(pbpaste)\"
  3. Smoke-test:
       latchkey curl -s https://api.github.com/user | head -c 200

See frankweiler/backend/etl/providers/github/EXTRACT.md for details."
            .into(),
        "gitlab_api" => "\
gitlab token expired or missing.

  1. Create a personal token at https://gitlab.com/-/profile/personal_access_tokens
     with `read_api` scope; copy it to the clipboard.
  2. Run (uses `$(pbpaste)` so the token isn't recorded in shell history):
       latchkey auth set gitlab -H \"Authorization: Bearer $(pbpaste)\"
  3. Smoke-test:
       latchkey curl -s https://gitlab.com/api/v4/user | head -c 200

See frankweiler/backend/etl/providers/gitlab/EXTRACT.md for details."
            .into(),
        "notion_api" => "\
notion integration token expired or missing.

  1. Create an internal integration at https://www.notion.so/profile/integrations
     and copy the secret to the clipboard.
  2. Run (uses `$(pbpaste)` so the token isn't recorded in shell history):
       latchkey auth set notion -H \"Authorization: Bearer $(pbpaste)\"
  3. Smoke-test:
       latchkey curl -s -X POST https://api.notion.com/v1/search \\
         -H 'Notion-Version: 2022-06-28' -H 'Content-Type: application/json' \\
         -d '{}' | head -c 200

See frankweiler/backend/etl/providers/notion/EXTRACT.md for details."
            .into(),
        _ => GENERIC_AUTH_HINT.into(),
    }
}

async fn run(summary: &Arc<Mutex<SyncSummary>>) -> Result<()> {
    let args = Args::parse();
    let _ = args.deterministic;

    // Sample the system clock once if `--now` was omitted, so every
    // phase sees the same timestamp for the duration of the run.
    // Local TZ + offset (e.g. `2026-05-28T14:23:45-07:00`) so the
    // timestamp is meaningful when a human reads it.
    let now = args.now.unwrap_or_else(|| {
        chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
    });

    let cfg = load_config(args.config.as_deref()).context("load config")?;
    eprintln!(
        "[frankweiler-sync] config: data_root={}, {} source(s)",
        cfg.data_root.display(),
        cfg.sources.len()
    );

    if let Some(playback_out) = &args.synthesize_playback_root {
        // Synth-only doesn't write into data_root, so don't bother
        // staging the summary file there. Run and exit.
        return run_synthesize(&cfg, playback_out);
    }

    fs::create_dir_all(&cfg.data_root)
        .with_context(|| format!("create data_root: {}", cfg.data_root.display()))?;
    let root = cfg.data_root.canonicalize()?;
    fs::create_dir_all(root.join("rendered_md"))?;
    eprintln!("[frankweiler-sync] data_root = {}", root.display());

    {
        let mut s = summary.lock().unwrap();
        s.data_root = Some(root.clone());
        // Stamp the run's `--now` into the filename so successive runs
        // don't clobber each other and CI can attach the JSON as a
        // build artifact keyed by run timestamp. `:` is legal on Unix
        // filesystems but trips up Windows; replace with `-` for
        // portability.
        let safe_now = now.replace(':', "-");
        s.output_path = Some(root.join(format!("sync_summary_{safe_now}.json")));
    }

    // ── Extract ────────────────────────────────────────────────────
    if args.skip_extract {
        eprintln!("[frankweiler-sync] extract: skipped (--skip-extract); using staged input_paths");
        let mut s = summary.lock().unwrap();
        for src in cfg.enabled_sources() {
            s.extract.push(PhaseOutcome {
                name: src.name().to_string(),
                type_str: src.type_str().to_string(),
                status: Status::Skipped,
                error: None,
                error_kind: None,
                stats: Some("skipped (--skip-extract)".into()),
            });
        }
    } else {
        let pb = if let Some(playback_root) = args.playback_root.as_ref() {
            let pb = playback_root
                .canonicalize()
                .with_context(|| format!("playback root: {}", playback_root.display()))?;
            std::env::set_var(PLAYBACK_ENV, &pb);
            eprintln!("[frankweiler-sync] playback root = {}", pb.display());
            Some(pb)
        } else {
            eprintln!("[frankweiler-sync] extract: live (hitting provider APIs)");
            None
        };
        let outcomes = run_extract_phase(&cfg, pb.as_deref(), &now).await;
        summary.lock().unwrap().extract.extend(outcomes);
    }

    // ── Translate ──────────────────────────────────────────────────
    // Translate only runs against sources whose extract succeeded (or
    // was skipped via --skip-extract). A source whose extract errored
    // out probably has missing/partial fixtures on disk, so attempting
    // to translate it just produces a second, downstream failure
    // confusing the summary.
    let extract_failed: std::collections::HashSet<String> = summary
        .lock()
        .unwrap()
        .extract
        .iter()
        .filter(|o| o.status == Status::Error)
        .map(|o| o.name.clone())
        .collect();
    for src in cfg.enabled_sources() {
        let name = src.name().to_string();
        let type_str = src.type_str().to_string();
        if extract_failed.contains(&name) {
            summary.lock().unwrap().translate.push(PhaseOutcome {
                name,
                type_str,
                status: Status::Skipped,
                error: None,
                error_kind: None,
                stats: Some("skipped (extract failed)".into()),
            });
            continue;
        }
        let res = translate_source(src, &cfg, &root).map(|_| "ok".to_string());
        summary
            .lock()
            .unwrap()
            .translate
            .push(summary::outcome_from(&name, &type_str, res));
    }

    // ── Load ───────────────────────────────────────────────────────
    let load_res = run_load_phase(&cfg, &root, &now).await;
    match load_res {
        Ok(load_summary) => {
            eprintln!(
                "[frankweiler-sync] loaded documents={}/{} rows={}",
                load_summary.documents_loaded,
                load_summary.documents_total,
                load_summary.rows_inserted
            );
            summary.lock().unwrap().load = Some(summary::LoadOutcome {
                documents_loaded: load_summary.documents_loaded,
                documents_total: load_summary.documents_total,
                rows_inserted: load_summary.rows_inserted,
                error: None,
            });
        }
        Err(e) => {
            eprintln!("[frankweiler-sync] load FAILED: {e:#}");
            summary.lock().unwrap().load = Some(summary::LoadOutcome {
                documents_loaded: 0,
                documents_total: 0,
                rows_inserted: 0,
                error: Some(
                    e.chain()
                        .map(|c| c.to_string())
                        .collect::<Vec<_>>()
                        .join(": "),
                ),
            });
        }
    }

    eprintln!(
        "[frankweiler-sync] wrote {}/",
        root.join("rendered_md").display()
    );

    // ── QMD index ──────────────────────────────────────────────────
    if !cfg.qmd.skip {
        match build_qmd_index(&root, cfg.qmd.models_dir.as_deref()) {
            Ok(()) => {
                eprintln!(
                    "[frankweiler-sync] wrote {}",
                    root.join("qmd/index.sqlite").display()
                );
                summary.lock().unwrap().qmd_index = Some(PhaseOutcome::ok(
                    "qmd",
                    "qmd",
                    root.join("qmd/index.sqlite").display().to_string(),
                ));
            }
            Err(e) => {
                eprintln!("[frankweiler-sync] qmd index FAILED: {e:#}");
                summary.lock().unwrap().qmd_index = Some(PhaseOutcome::err("qmd", "qmd", &e));
            }
        }
    } else {
        eprintln!("[frankweiler-sync] qmd index: skipped (qmd.skip=true)");
    }
    Ok(())
}

/// Open the doltlite file at `<data_root>/<dolt.db_filename>`, init
/// schema, run `load_all`. Pulled out of `run()` so the orchestrator can
/// catch failures here without short-circuiting the summary write.
async fn run_load_phase(
    cfg: &Config,
    root: &Path,
    now: &str,
) -> Result<frankweiler_etl::load::LoadSummary> {
    let db_path = cfg.dolt_db_path();
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)?;
    }
    eprintln!("[frankweiler-sync] doltlite db = {}", db_path.display());
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
    let pool = SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .with_context(|| format!("open doltlite at {}", db_path.display()))?;
    init_schema(&pool).await?;
    let summary = load_all(&pool, root, |_| {}, Some(now))
        .await
        .context("load_all")?;
    // Force-checkpoint the WAL into the main DB before closing the
    // pool. Under WAL mode, all writes go to `<db>.db-wal` and only
    // get merged into the main `.db` file on a checkpoint. sqlx's
    // default close path runs only a PASSIVE checkpoint, which copies
    // bytes but leaves the WAL file populated — so a downstream
    // process that copies just `backend_index.doltlite_db` ends up
    // with an empty file. The genrule that ships
    // `tests/fixtures/ingested/backend_index.doltlite_db`
    // hit exactly this: 4KB-empty `.db`, all data in `.db-wal`,
    // every e2e test asserts zero rows. TRUNCATE checkpoints + zeros
    // the WAL so the `.db` file is self-contained.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&pool)
        .await
        .context("wal_checkpoint at end of load")?;
    drop(pool);
    eprintln!("[frankweiler-sync] wrote {}", db_path.display());
    Ok(summary)
}

// ─────────────────────────────────────────────────────────────────────
// Extract phase
// ─────────────────────────────────────────────────────────────────────

/// Drive every managed source's `extract::fetch` against the playback
/// tree. Each source writes into its resolved `input_path`. Runs
/// concurrently when `cfg.sync.parallel`.
///
/// Keep-going: a per-source error does NOT abort the phase. We return
/// one [`PhaseOutcome`] per managed source; the orchestrator decides
/// what (if anything) to do with the failures. Two exceptions: an
/// `out_dir` mkdir failure (filesystem permissions etc.) and a task
/// panic both surface as the source's own error so they can't take the
/// whole pipeline down.
async fn run_extract_phase(
    cfg: &Config,
    playback_root: Option<&Path>,
    now: &str,
) -> Vec<PhaseOutcome> {
    let mut outcomes: Vec<PhaseOutcome> = Vec::new();
    let mut plans: Vec<ExtractPlan> = Vec::new();
    for s in cfg.enabled_sources() {
        let Some(plan_res) = ExtractPlan::for_source(s, cfg, playback_root, now) else {
            continue;
        };
        match plan_res {
            Ok(plan) => plans.push(plan),
            Err(e) => outcomes.push(PhaseOutcome::err(s.name(), s.type_str(), &e)),
        }
    }

    // Pre-create out_dirs; an mkdir failure becomes a source-level
    // outcome rather than a phase-wide abort.
    plans.retain_mut(|p| {
        if let Err(e) = fs::create_dir_all(&p.out_dir) {
            let err = anyhow::Error::new(e).context(format!("create {}", p.out_dir.display()));
            outcomes.push(PhaseOutcome::err(&p.name, p.type_str, &err));
            false
        } else {
            true
        }
    });

    // One MultiProgress for the whole extract phase; one bar per plan
    // fanned out to a TracingSink so structured consumers see the same
    // stream.
    let multi = make_multi();
    for plan in &mut plans {
        let bar = make_bar(&multi, plan.name.clone());
        let sinks: Vec<std::sync::Arc<dyn frankweiler_etl::progress::ProgressSink>> = vec![
            std::sync::Arc::new(IndicatifSink::new(bar)),
            std::sync::Arc::new(TracingSink::new(plan.name.clone())),
        ];
        plan.progress = Progress::new(std::sync::Arc::new(FanOut::new(sinks)));
    }

    if cfg.sync.parallel {
        let mut set: JoinSet<(String, &'static str, Result<String>)> = JoinSet::new();
        for plan in plans {
            let name = plan.name.clone();
            let type_str = plan.type_str;
            set.spawn(async move {
                eprintln!("[extract] start {name} ({type_str})");
                let r = plan
                    .run()
                    .await
                    .with_context(|| format!("extract {name} (type={type_str})"));
                match &r {
                    Ok(s) => eprintln!("[extract] done  {name}: {s}"),
                    Err(e) => eprintln!("[extract] FAIL  {name}: {e:#}"),
                }
                (name, type_str, r)
            });
        }
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((name, type_str, r)) => {
                    outcomes.push(summary::outcome_from(&name, type_str, r));
                }
                Err(e) => {
                    // Task panicked — we don't know which source. Record
                    // a generic outcome so the panic shows up in the
                    // summary instead of disappearing silently.
                    let err = anyhow::anyhow!("extract task panicked: {e}");
                    outcomes.push(PhaseOutcome::err("<unknown>", "unknown", &err));
                }
            }
        }
    } else {
        for plan in plans {
            let name = plan.name.clone();
            let type_str = plan.type_str;
            eprintln!("[extract] {name} ({type_str})");
            let r = plan
                .run()
                .await
                .with_context(|| format!("extract {name} (type={type_str})"));
            match &r {
                Ok(s) => eprintln!("[extract] done  {name}: {s}"),
                Err(e) => eprintln!("[extract] FAIL  {name}: {e:#}"),
            }
            outcomes.push(summary::outcome_from(&name, type_str, r));
        }
    }

    // Parallel mode collects outcomes in completion order, which is
    // nondeterministic — fast-finishing sources show up before slow
    // ones. Sort by the source-declaration order from the config so
    // the summary (and its snapshot) stays stable across runs.
    // Sources not in the config (e.g. a `<unknown>` panic record)
    // sort to the end in insertion order.
    let cfg_order: std::collections::HashMap<&str, usize> = cfg
        .enabled_sources()
        .enumerate()
        .map(|(i, s)| (s.name(), i))
        .collect();
    let fallback_pos = cfg_order.len();
    outcomes.sort_by_key(|o| {
        cfg_order
            .get(o.name.as_str())
            .copied()
            .unwrap_or(fallback_pos)
    });
    outcomes
}

/// One source's extract closure. Holds owned data so it can be moved
/// into a `tokio::spawn` task. `Arc<dyn Fn ... + Send + Sync>` would
/// work too — we use an enum dispatch for clarity.
struct ExtractPlan {
    name: String,
    type_str: &'static str,
    out_dir: PathBuf,
    now: String,
    progress: Progress,
    kind: ExtractKind,
}

enum ExtractKind {
    Anthropic {
        sync: ClaudeApiSync,
    },
    Chatgpt {
        sync: ChatgptApiSync,
    },
    Slack {
        sync: SlackApiSync,
    },
    Github {
        sync: GithubApiSync,
    },
    Gitlab {
        sync: GitlabApiSync,
    },
    Notion {
        sync: NotionApiSync,
        playback_root: Option<PathBuf>,
    },
}

impl ExtractPlan {
    /// `None` when the source is translate-only (no `sync:` block).
    fn for_source(
        src: &SourceConfig,
        cfg: &Config,
        playback_root: Option<&Path>,
        now: &str,
    ) -> Option<Result<Self>> {
        if !src.is_managed() {
            return None;
        }
        let name = src.name().to_string();
        let type_str = src.type_str();
        let out_dir = src.resolved_input_path(&cfg.data_root);
        let kind = match src {
            SourceConfig::ClaudeApi { sync, .. } => ExtractKind::Anthropic {
                sync: sync.clone().unwrap_or_default(),
            },
            SourceConfig::ChatgptApi { sync, .. } => ExtractKind::Chatgpt {
                sync: sync.clone().unwrap_or_default(),
            },
            SourceConfig::SlackApi { sync, .. } => ExtractKind::Slack {
                sync: sync.clone().unwrap_or_default(),
            },
            SourceConfig::GithubApi { sync, .. } => ExtractKind::Github {
                sync: sync.clone().unwrap_or_default(),
            },
            SourceConfig::GitlabApi { sync, .. } => ExtractKind::Gitlab {
                sync: sync.clone().unwrap_or_default(),
            },
            SourceConfig::NotionApi { sync, .. } => ExtractKind::Notion {
                sync: sync.clone().unwrap_or_default(),
                playback_root: playback_root.map(|p| p.to_path_buf()),
            },
            SourceConfig::ClaudeExport { .. } => return None,
        };
        Some(Ok(Self {
            name,
            type_str,
            out_dir,
            now: now.to_string(),
            progress: Progress::noop(),
            kind,
        }))
    }

    /// Returns a one-line per-source summary on success. Provider-specific
    /// shape — what makes it onto stderr is whatever's interesting for
    /// that source (slack media outcomes including `error` counts, claude
    /// fetched/skipped/errors, etc).
    async fn run(self) -> Result<String> {
        let progress = self.progress.clone();
        let result: Result<String> = match self.kind {
            ExtractKind::Anthropic { sync } => {
                frankweiler_etl_anthropic::extract::fetch(
                    frankweiler_etl_anthropic::extract::FetchOptions {
                        db_path: self.out_dir.clone(),
                        // Auto-resolve: users.json (from the bulk export)
                        // is expected to live alongside the source's
                        // input_path. In playback mode the genrule
                        // pre-seeds it there.
                        export_dir: Some(self.out_dir.clone()),
                        overlap: sync.overlap.map(|v| v as usize).unwrap_or(usize::MAX),
                        sleep_between: Duration::ZERO,
                        conv_uuids: sync.conv_uuids.clone(),
                        progress: progress.clone(),
                    },
                )
                .await
                .map(|s| {
                    format!(
                        "fetched={} skipped={} errors={} forbidden_orgs={} total={} requests={}",
                        s.fetched, s.skipped, s.errors, s.forbidden_orgs, s.total, s.requests,
                    )
                })
            }
            ExtractKind::Chatgpt { sync } => frankweiler_etl_chatgpt::extract::fetch(
                frankweiler_etl_chatgpt::extract::FetchOptions {
                    db_path: self.out_dir.clone(),
                    max_pages: sync.max_pages.map(|v| v as usize),
                    limit: sync.limit.map(|v| v as usize),
                    sleep_between: Duration::ZERO,
                    conv_uuids: sync.conv_uuids.clone(),
                    fetched_at: Some(self.now.clone()),
                    progress: progress.clone(),
                },
            )
            .await
            .map(|s| {
                format!(
                    "fetched={} skipped={} errors={} listing={} requests={}",
                    s.fetched, s.skipped, s.errors, s.listing, s.requests,
                )
            }),
            ExtractKind::Slack { sync } => frankweiler_etl_slack::extract::fetch(
                frankweiler_etl_slack::extract::FetchOptions {
                    db_path: self.out_dir.clone(),
                    channels: sync.channels.clone(),
                    since: sync
                        .since
                        .clone()
                        .unwrap_or_else(|| frankweiler_etl_slack::extract::DEFAULT_SINCE.into()),
                    refresh_window_days: sync.refresh_window_days.unwrap_or(0),
                    members_only: !sync.all_channels && sync.channels.is_none(),
                    media: sync.media,
                    progress: progress.clone(),
                },
            )
            .await
            .map(|s| {
                let media = s
                    .media
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!(
                    "msgs={} replies={} media[{}]",
                    s.messages, s.replies, media
                )
            }),
            ExtractKind::Github { sync } => frankweiler_etl_github::extract::fetch(
                frankweiler_etl_github::extract::FetchOptions {
                    out_dir: self.out_dir.clone(),
                    full_sync: true,
                    refresh_window_days: sync
                        .refresh_window_days
                        .map(|v| v.max(0) as u32)
                        .unwrap_or(0),
                    max_prs: sync.max_prs.map(|v| v as usize),
                    sleep_between: Duration::ZERO,
                    progress: progress.clone(),
                    ..Default::default()
                },
            )
            .await
            .map(|s| {
                format!(
                    "prs(new={}/upd={}) issue_comments(new={}/upd={}) reviews(new={}/upd={}) review_comments(new={}/upd={})",
                    s.new_prs,
                    s.upd_prs,
                    s.new_issue_comments,
                    s.upd_issue_comments,
                    s.new_reviews,
                    s.upd_reviews,
                    s.new_review_comments,
                    s.upd_review_comments,
                )
            }),
            ExtractKind::Gitlab { sync } => frankweiler_etl_gitlab::extract::fetch(
                frankweiler_etl_gitlab::extract::FetchOptions {
                    out_dir: self.out_dir.clone(),
                    full_sync: true,
                    refresh_window_days: sync
                        .refresh_window_days
                        .map(|v| v.max(0) as u32)
                        .unwrap_or(0),
                    max_mrs: sync.max_mrs.map(|v| v as usize),
                    sleep_between: Duration::ZERO,
                    progress: progress.clone(),
                    ..Default::default()
                },
            )
            .await
            .map(|s| {
                format!(
                    "mrs(new={}/upd={}) discussions(new={}/upd={}) requests={}",
                    s.new_mrs, s.upd_mrs, s.new_discussions, s.upd_discussions, s.requests,
                )
            }),
            ExtractKind::Notion {
                sync,
                playback_root,
            } => {
                // Notion has no listing endpoint; in playback mode we
                // derive seeds by scanning the fixture tree for every
                // synthesized page response. Outside playback we honor
                // the configured subtree seeds verbatim.
                let mut seeds: Vec<String> = sync
                    .subtrees
                    .as_ref()
                    .map(|t| t.pages.clone())
                    .unwrap_or_default();
                if let Some(pb) = playback_root.as_ref() {
                    let derived =
                        derive_notion_seeds(&pb.join("notion")).context("derive notion seeds")?;
                    seeds.extend(derived);
                }
                seeds.sort();
                seeds.dedup();
                frankweiler_etl_notion::extract::fetch(
                    frankweiler_etl_notion::extract::FetchOptions {
                        db_path: self.out_dir.clone(),
                        subtree_pages: seeds,
                        inbox: sync.inbox.as_ref().is_some_and(|i| i.enabled),
                        inbox_mirror_referenced: sync
                            .inbox
                            .as_ref()
                            .and_then(|i| i.mirror_referenced_pages)
                            .unwrap_or(true),
                        space: sync.inbox.as_ref().and_then(|i| i.space.clone()),
                        sleep_between: Duration::ZERO,
                        progress: progress.clone(),
                        ..Default::default()
                    },
                )
                .await
                .map(|s| {
                    format!(
                        "pages(new={}/upd={}) blocks(new={}/upd={}) comments(new={}/upd={}) requests(official={}/unofficial={})",
                        s.new_pages,
                        s.upd_pages,
                        s.new_blocks,
                        s.upd_blocks,
                        s.new_comments,
                        s.upd_comments,
                        s.official_requests,
                        s.unofficial_requests,
                    )
                })
            }
        };
        progress.finish("done");
        result
    }
}

/// Walk `<playback>/notion/*.json`, decode each as an `HttpResponse`,
/// parse the body as JSON, and collect every `id` whose `object` is
/// `"page"`. Notion's BFS dedupes naturally so over-seeding is safe.
fn derive_notion_seeds(notion_dir: &Path) -> Result<Vec<String>> {
    let mut seeds = Vec::new();
    if !notion_dir.is_dir() {
        return Ok(seeds);
    }
    for entry in
        fs::read_dir(notion_dir).with_context(|| format!("read_dir {}", notion_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let resp: HttpResponse = match serde_json::from_slice(&bytes) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let body: serde_json::Value = match serde_json::from_slice(&resp.body) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if body.get("object").and_then(|v| v.as_str()) == Some("page") {
            if let Some(id) = body.get("id").and_then(|v| v.as_str()) {
                seeds.push(id.to_string());
            }
        }
    }
    seeds.sort();
    seeds.dedup();
    Ok(seeds)
}

// ─────────────────────────────────────────────────────────────────────
// Translate phase
// ─────────────────────────────────────────────────────────────────────

/// Translate one source's `input_path` into the workspace's
/// `rendered_md/` + sidecar tree. ClaudeExport shares the anthropic
/// translator since the on-disk shape is the same.
fn translate_source(src: &SourceConfig, cfg: &Config, root: &Path) -> Result<()> {
    let fixture = src.resolved_input_path(&cfg.data_root);
    let name = src.name();
    eprintln!(
        "[translate] {name} ({}): {}",
        src.type_str(),
        fixture.display()
    );
    match src {
        SourceConfig::ClaudeApi { .. } | SourceConfig::ClaudeExport { .. } => {
            use frankweiler_etl_anthropic::translate::{parse::parse_export, render::render_all};
            let parsed = parse_export(&fixture)
                .with_context(|| format!("anthropic parse {}", fixture.display()))?;
            render_all(&parsed, root, name)
                .context("anthropic render_all")
                .map(|_| ())
        }
        SourceConfig::ChatgptApi { .. } => {
            use frankweiler_etl_chatgpt::translate::{parse::parse_api_dir, render::render_all};
            let parsed = parse_api_dir(&fixture)
                .with_context(|| format!("chatgpt parse {}", fixture.display()))?;
            render_all(&parsed, root, name)
                .context("chatgpt render_all")
                .map(|_| ())
        }
        SourceConfig::SlackApi { .. } => {
            use frankweiler_etl_slack::translate::{render::render_all, translate_raw_dir};
            let t = translate_raw_dir(&fixture)
                .with_context(|| format!("slack translate_raw_dir {}", fixture.display()))?;
            render_all(&t, root, name, |_| {})
                .context("slack render_all")
                .map(|_| ())
        }
        SourceConfig::GithubApi { .. } => {
            use frankweiler_etl_github::translate::{parse_api_dir, render_github};
            let parsed = parse_api_dir(&fixture)
                .with_context(|| format!("github parse {}", fixture.display()))?;
            render_github(&parsed, root)
                .context("render_github")
                .map(|_| ())
        }
        SourceConfig::GitlabApi { .. } => {
            use frankweiler_etl_gitlab::translate::{parse_api_dir, render_gitlab};
            let parsed = parse_api_dir(&fixture)
                .with_context(|| format!("gitlab parse {}", fixture.display()))?;
            render_gitlab(&parsed, root)
                .context("render_gitlab")
                .map(|_| ())
        }
        SourceConfig::NotionApi { .. } => {
            use frankweiler_etl_notion::translate::{
                parse_api_dir, render::render_notion_official,
            };
            let parsed = parse_api_dir(&fixture)
                .with_context(|| format!("notion parse {}", fixture.display()))?;
            render_notion_official(&parsed, root)
                .context("render_notion_official")
                .map(|_| ())
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Synthesize phase
// ─────────────────────────────────────────────────────────────────────

/// Synth-only mode. Iterates over every enabled source and runs the
/// matching synthesizer, reading from the source's `input_path` (which
/// in this mode points at a checked-in raw fixture tree) and writing
/// HTTP playback responses into `out`.
fn run_synthesize(cfg: &Config, out: &Path) -> Result<()> {
    fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    for src in cfg.enabled_sources() {
        let input = src.resolved_input_path(&cfg.data_root);
        let synth: Box<dyn Synthesizer> = match src {
            SourceConfig::ClaudeApi { .. } | SourceConfig::ClaudeExport { .. } => {
                Box::new(AnthropicSynth::new(input.clone()))
            }
            SourceConfig::ChatgptApi { .. } => Box::new(ChatgptSynth::new(input.clone())),
            SourceConfig::SlackApi { .. } => Box::new(SlackSynth::new(input.clone())),
            SourceConfig::GithubApi { .. } => Box::new(GithubSynth::new(input.clone())),
            SourceConfig::GitlabApi { .. } => Box::new(GitlabSynth::new(input.clone())),
            SourceConfig::NotionApi { .. } => Box::new(NotionSynth::new(input.clone())),
        };
        let report = synth
            .synthesize(out)
            .with_context(|| format!("synthesize {} ({})", src.name(), src.type_str()))?;
        eprintln!(
            "[synth] {} ({}): {} fixtures from {}",
            src.name(),
            src.type_str(),
            report.fixtures_written,
            input.display(),
        );
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────

fn build_qmd_index(root: &Path, models_dir: Option<&Path>) -> Result<()> {
    let mut opts = frankweiler_qmd_indexer::IndexOptions::new(root);
    if let Some(d) = models_dir {
        opts.models_dir = d.to_path_buf();
    }
    frankweiler_qmd_indexer::run_index(&opts).context("qmd index build")?;
    Ok(())
}
