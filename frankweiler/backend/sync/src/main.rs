//! `frankweiler-sync` — config-driven ETL orchestrator.
//!
//! Drives Extract → Render-and-index-md → Load → Archive across every
//! enabled source in the user's `~/.config/frankweiler/config.yaml`.
//! Each source is dispatched on its `type:` discriminator; sources with
//! a `sync:` block (the "managed" ones) get their downloader invoked,
//! the others are render-and-index-md-only against pre-staged `input_path`.
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
//!     against pre-staged `input_path`s (the doltlite DBs for each
//!     source). Useful for iterating on translate/load without re-hitting
//!     the network, and as an escape hatch when one source's fetch is
//!     broken or unreasonably slow — you still get whatever data is
//!     already on disk correctly rendered and indexed. Incremental: docs
//!     whose `source_fingerprint` matches `documents.source_fingerprint`
//!     are skipped without re-rendering.
//!   * default: extract live from every managed source's provider API,
//!     translate, load into a scratch Dolt repo, emit `dolt_repo/` +
//!     the configured Dolt repo at `<data_root>/dolt_db/`, write the
//!     rendered markdown tree to `<data_root>/rendered_md/`, and (unless
//!     `qmd.skip`) build the qmd index at `<data_root>/qmd/index.sqlite`.
//!     SQL dumping (if needed) is downstream — e.g. a Bazel genrule that
//!     consumes `dolt_db/` and runs `dolt dump`.
//!
//! Both extract and translate run concurrently across managed sources
//! when `sync.parallel: true` (the default). Each translate task writes
//! through its own `apply_one` callback; the shared index doltlite is
//! a WAL-mode sqlx pool, so per-doc indexer writes from different
//! sources serialize at the SQLite level but pool acquisition stays
//! non-blocking. The on-disk `rendered_md/` tree is sharded by source
//! so there's no contention there.

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use frankweiler_core::config::{load_config, Config, SourceConfig};
use frankweiler_etl::http::PLAYBACK_ENV;
use frankweiler_etl::load::{
    apply_one, init_schema, load_cursors, load_fingerprints, RenderedMarkdown,
};
use frankweiler_etl::progress::{FanOut, Progress, TracingSink};
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_anthropic::synthesize::AnthropicSynth;
use frankweiler_etl_beeper::synthesize::BeeperSynth;
use frankweiler_etl_chatgpt::synthesize::ChatgptSynth;
use frankweiler_etl_github::synthesize::GithubSynth;
use frankweiler_etl_gitlab::synthesize::GitlabSynth;
use frankweiler_etl_notion::synthesize::NotionSynth;
use frankweiler_etl_slack::synthesize::SlackSynth;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::task::JoinSet;

mod progress;
mod render_and_index_md;
mod summary;
use crate::progress::{make_bar, make_multi, IndicatifSink};
use crate::summary::{ErrorKind, PhaseOutcome, Status, SyncSummary};
// Use `frankweiler_obs::status_line!` for status lines that fire while
// progress bars are on screen — it routes through the shared
// `MultiProgress::println` to suspend bar draws across the write.
use frankweiler_obs::status_line;

// `FRANKWEILER_VERSION` is the output of `git describe --tags --always
// --dirty` at build time, set by either
//   - Bazel: rustc_env.txt resolves {STABLE_GIT_DESCRIBE} from
//     tools/workspace_status.sh — *only when stamping is on* (i.e.
//     `--config=release`). Day-to-day dev builds have stamping off
//     so the action cache doesn't invalidate on every commit; in
//     that case rules_rust passes the literal placeholder
//     `{STABLE_GIT_DESCRIBE}` through to rustc.
//   - cargo: build.rs runs the same `git describe` (falls back to
//     "unknown" outside a git checkout).
// `FRANKWEILER_VERSION_RESOLVED` strips the unsubstituted-placeholder
// state down to "dev" so the user-facing `--version` output reads
// either a real `git describe` string, "unknown", or "dev". Exact-tag
// commits render as "v0.1.2"; mid-development commits as
// "v0.1.2-3-gabc123d".
const FRANKWEILER_VERSION_RESOLVED: &str = {
    let raw = env!("FRANKWEILER_VERSION");
    if !raw.is_empty() && raw.as_bytes()[0] == b'{' {
        "dev"
    } else {
        raw
    }
};

#[derive(Debug, Parser)]
#[command(
    name = "frankweiler-sync",
    version = FRANKWEILER_VERSION_RESOLVED,
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
    /// `input_path`s (the doltlite DBs already on disk for each source).
    /// Useful when iterating on translate/load without re-hitting the
    /// network, and as an escape hatch when one source's fetch is broken
    /// or taking too long — you still get whatever data is already on
    /// disk correctly rendered and indexed. Translate is incremental:
    /// docs whose `source_fingerprint` matches the indexer's record are
    /// left untouched, so repeated runs with this flag are near-free.
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

    /// Wipe every enabled source's per-entity tables (and their
    /// `_bookkeeping` sidecars) before the run, and re-download every
    /// entity row from upstream. The resulting `dolt diff` between
    /// the pre-reset and post-reset commits then shows only
    /// upstream-content changes (because the bookkeeping sidecars
    /// are not part of the data diff), which is how we verify our
    /// PK design is stable across re-fetches.
    ///
    /// **`blob_refs` is preserved** so attachments whose bytes are
    /// already in the per-source CAS file are skip-checked instead of
    /// re-fetched on the wire. Use `--refetch-blobs` to invalidate
    /// the blob cache index when you actually want the bytes re-pulled.
    ///
    /// Whole-table bookkeeping (sync_runs, sync_scope_state) is
    /// preserved — that's audit log + resume cursor, not row content.
    #[arg(long)]
    reset_and_redownload: bool,

    /// Wipe `blob_refs` + `blob_refs_bookkeeping` for every enabled
    /// source before the run, so each attachment is re-fetched on the
    /// wire even when its bytes are already in the sibling CAS file.
    /// The CAS itself is never truncated — re-fetched bytes hash to
    /// the same blake3 and `INSERT OR IGNORE` is a no-op, so this
    /// costs network IO but not disk.
    ///
    /// Orthogonal to `--reset-and-redownload`: pass both for a full
    /// reset; pass `--reset-and-redownload` alone for the common
    /// "check for entity gaps without burning bandwidth on blobs"
    /// case.
    #[arg(long)]
    refetch_blobs: bool,

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
            // Subscriber didn't come up — tracing isn't an option, and
            // there's no MultiProgress to write through yet. Plain
            // stderr is the only sink we have.
            #[allow(clippy::disallowed_macros)]
            {
                eprintln!("[frankweiler-sync] tracing init failed: {e}");
            }
            None
        }
    };

    let summary = Arc::new(Mutex::new(SyncSummary::new()));
    let start = Instant::now();
    // Shared handle the `run()` body fills in as it makes progress —
    // the SIGINT handler reads from it to issue best-effort commits
    // against whatever doltlite databases happen to have uncommitted
    // writes when the user Ctrl-Cs.
    let ctrlc = Arc::new(Mutex::new(CtrlcState::default()));

    // Ctrl-C: best-effort flush of the summary AND best-effort dolt
    // commit before exit. We can't join the running task graph from
    // here cleanly, so mid-flight extract work is abandoned — but any
    // rows already on disk get a commit, so re-running picks up where
    // the interrupt happened with a clean dolt_log.
    let s_sig = summary.clone();
    let c_sig = ctrlc.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            status_line!("[frankweiler-sync] caught Ctrl-C; committing partial state…");
            interrupt_commit_all(&c_sig).await;
            // Re-snapshot each source's files (post-commit) and record
            // what changed, even on an interrupted run.
            let reports = interrupt_build_reports(&c_sig).await;
            let mut s = s_sig.lock().unwrap();
            s.interrupted = true;
            s.interrupted_extract_reports = reports;
            s.finalize(start);
            match s.write() {
                Ok(Some(p)) => {
                    status_line!("[frankweiler-sync] summary: {}", summary::pretty_path(&p))
                }
                Ok(None) => status_line!("[frankweiler-sync] summary: <no data_root yet>"),
                Err(e) => status_line!("[frankweiler-sync] failed to write summary: {e}"),
            }
            std::process::exit(130);
        }
    });

    let fatal: Option<anyhow::Error> = run(&summary, &ctrlc).await.err();

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
    for outcome in s.extract.iter().chain(s.render_and_index_md.iter()) {
        if outcome.status == Status::Error {
            status_line!(
                "\n[{}] {} ({}): {}",
                outcome.error_kind.map(|k| k.as_str()).unwrap_or("error"),
                outcome.name,
                outcome.type_str,
                outcome.error.as_deref().unwrap_or(""),
            );
            if outcome.error_kind == Some(ErrorKind::Auth) {
                status_line!("--- auth hint ---");
                status_line!("{}", auth_hint_for(&outcome.type_str));
            }
        }
    }
    if let Some(e) = fatal.as_ref() {
        render_error(e);
    }

    match s.write() {
        Ok(Some(p)) => status_line!("\n[frankweiler-sync] summary: {}", summary::pretty_path(&p)),
        Ok(None) => status_line!("\n[frankweiler-sync] summary: <not written; no data_root>"),
        Err(e) => status_line!("\n[frankweiler-sync] failed to write summary: {e}"),
    }

    let any_phase_err = s.extract.iter().any(|o| o.status == Status::Error)
        || s.render_and_index_md
            .iter()
            .any(|o| o.status == Status::Error)
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

/// Shared state populated by the main `run()` body and read by the
/// SIGINT handler. Each field is independently `None` while the
/// corresponding resource isn't ready yet.
#[derive(Default)]
struct CtrlcState {
    /// Open pool to the index doltlite_db. Populated after
    /// `open_index_pool`. The handler issues one commit against this
    /// pool to capture whatever rows the translate phase already wrote.
    index_pool: Option<sqlx::sqlite::SqlitePool>,
    /// Per-source live pools to the extract doltlite_dbs. Populated
    /// at the start of `run_extract_phase` (before any download begins)
    /// and never reopened. The SIGINT handler commits against these
    /// pools directly — no `commit_run_at_path` reopen race because
    /// these are the *same* connections the extract workers are using.
    extract_pools: Vec<ExtractPoolEntry>,
    /// Per-source report contexts (paths, before-snapshots, live
    /// counters), registered alongside `extract_pools`. On Ctrl-C —
    /// after the pools are committed — the handler re-snapshots each
    /// source's files and assembles a report from these, so an
    /// interrupted run still records what changed.
    extract_reports: Vec<ReportCtx>,
    /// Opaque interrupt-commit hooks registered by Program-A
    /// `DataProcessor`-based sources (today: email). The handler fires each
    /// on Ctrl-C; unlike `extract_pools` it carries no pool and no doltlite
    /// knowledge — the source's `Checkpoint` impl owns the commit. Shared by
    /// `Arc` into every migrated source's `RunCtx`.
    checkpoints: Arc<frankweiler_etl::processor::CheckpointSink>,
}

/// Everything needed to assemble a source's [`ExtractReport`] after the
/// fact: the entity-db path (the CAS path is derived), the before-extract
/// snapshots, and the live counters. Held by both `run_extract_phase`
/// (for the clean-completion path) and [`CtrlcState`] (for the interrupt
/// path).
#[derive(Clone)]
struct ReportCtx {
    name: String,
    entity_path: PathBuf,
    before_events: frankweiler_etl::extract_metrics::DbSnapshot,
    before_blobs: frankweiler_etl::extract_metrics::DbSnapshot,
    metrics: Arc<frankweiler_etl::extract_metrics::ExtractMetrics>,
    diagnostics: Arc<frankweiler_obs::diagnostics::Diagnostics>,
}

impl ReportCtx {
    /// Re-snapshot the source's files (the "after" endpoint) and fold in
    /// the live counters and captured diagnostics. Safe once the source has
    /// committed + released its writer; the snapshot uses an independent
    /// read-only connection.
    async fn assemble(&self) -> frankweiler_etl::extract_metrics::ExtractReport {
        frankweiler_etl::extract_metrics::assemble_report(
            &self.entity_path,
            &self.before_events,
            &self.before_blobs,
            &self.metrics,
            &self.diagnostics,
        )
        .await
    }
}

/// Entry in [`CtrlcState::extract_pools`] — the source's display name
/// and the open pool. We log the name on commit so a user reading the
/// interrupt summary can tell which source the commit hash belongs to.
#[derive(Clone)]
struct ExtractPoolEntry {
    name: String,
    pool: sqlx::sqlite::SqlitePool,
}

/// Commit every doltlite database the run has touched. Called from the
/// SIGINT handler; each individual failure is downgraded to a stderr
/// warning so one stuck file doesn't block the others.
async fn interrupt_commit_all(state: &Arc<Mutex<CtrlcState>>) {
    let (pool_opt, extract_pools, checkpoints) = {
        let s = state.lock().unwrap();
        (
            s.index_pool.clone(),
            s.extract_pools.clone(),
            s.checkpoints.snapshot(),
        )
    };
    if let Some(pool) = pool_opt {
        let msg = "frankweiler-sync: interrupted (Ctrl-C); committing partial state".to_string();
        match frankweiler_etl::doltlite_raw::commit_run(&pool, &msg).await {
            Ok(Some(h)) => status_line!("[frankweiler-sync] interrupt index commit: {h}"),
            Ok(None) => {}
            Err(e) => status_line!("[frankweiler-sync] interrupt index commit failed: {e:#}"),
        }
    }
    for entry in extract_pools {
        let msg = format!("extract {}: interrupted (Ctrl-C)", entry.name);
        match frankweiler_etl::doltlite_raw::commit_run(&entry.pool, &msg).await {
            Ok(Some(h)) => status_line!(
                "[frankweiler-sync] interrupt extract commit {}: {h}",
                entry.name
            ),
            Ok(None) => {}
            Err(e) => status_line!(
                "[frankweiler-sync] interrupt extract commit failed for {}: {e:#}",
                entry.name
            ),
        }
    }
    // Program-A processor-based sources (email): fire each registered opaque
    // interrupt-commit hook. The orchestrator knows only that each persists
    // partial state — the `dolt_commit` is encapsulated in the source's
    // `Checkpoint` impl.
    for entry in checkpoints {
        match entry.hook.checkpoint().await {
            Ok(()) => status_line!("[frankweiler-sync] interrupt checkpoint {}: ok", entry.name),
            Err(e) => status_line!(
                "[frankweiler-sync] interrupt checkpoint failed for {}: {e:#}",
                entry.name
            ),
        }
    }
}

/// Build per-source extract reports on a Ctrl-C interrupt. Call *after*
/// [`interrupt_commit_all`] so the after-snapshots see committed rows.
/// Logs a one-line summary per source as it goes and returns the
/// `(name, report)` pairs for the JSON summary. Best-effort: empty
/// reports (file-tree-backed sources, sources that never started) are
/// dropped.
async fn interrupt_build_reports(
    state: &Arc<Mutex<CtrlcState>>,
) -> Vec<(String, frankweiler_etl::extract_metrics::ExtractReport)> {
    let ctxs = { state.lock().unwrap().extract_reports.clone() };
    let mut out = Vec::new();
    for ctx in ctxs {
        let report = ctx.assemble().await;
        if report.is_empty() {
            continue;
        }
        status_line!(
            "[frankweiler-sync] extract {} (interrupted): {}",
            ctx.name,
            report.summary_line()
        );
        out.push((ctx.name.clone(), report));
    }
    out
}

/// Walk the anyhow error chain top-to-bottom (so the user reads
/// "extract foo (type=bar)" → "fetch /me" → "HTTP 403 …" in order) and,
/// when the failure looks auth-related, append source-specific
/// instructions for fixing latchkey credentials.
fn render_error(e: &anyhow::Error) {
    status_line!("\n[frankweiler-sync] FAILED");
    for (i, cause) in e.chain().enumerate() {
        let prefix = if i == 0 { "error:" } else { "  caused by:" };
        status_line!("{prefix} {cause}");
    }
    let chain_text: String = e
        .chain()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    if looks_like_auth_failure(&chain_text) {
        if let Some(provider) = extract_provider_type(&chain_text) {
            status_line!("\n--- auth hint ---");
            status_line!("{}", auth_hint_for(provider));
        } else {
            status_line!("\n--- auth hint ---");
            status_line!("{GENERIC_AUTH_HINT}");
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
        "type=email",
        "type=beeper",
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
        "email" => "\
Email source: JMAP (Fastmail / generic) auth missing or expired.

  1. Create an API token at https://app.fastmail.com/settings/security/tokens
     with the 'Read-only access to mail' scope; copy it to the clipboard.
  2. Register the two host services and attach the token to both
     (Fastmail serves blob bytes from a separate host):
       latchkey services register fastmail \\
           --base-api-url=\"https://api.fastmail.com/\"
       latchkey services register fastmail-content \\
           --base-api-url=\"https://www.fastmailusercontent.com/\"
       latchkey auth set fastmail         -H \"Authorization: Bearer $(pbpaste)\"
       latchkey auth set fastmail-content -H \"Authorization: Bearer $(pbpaste)\"
  3. Smoke-test:
       latchkey curl -sSL https://api.fastmail.com/.well-known/jmap \\
           | jq .primaryAccounts

See frankweiler/backend/etl/providers/jmap/EXTRACT.md for details."
            .into(),
        "beeper" => "\
beeper extract reads Beeper Texts' on-disk SQLite. No auth dance.

  1. Make sure Beeper Texts is installed and has run at least once
     so its data dir exists. Default path:
       ~/Library/Application Support/BeeperTexts/index.db
     (Pass --beeper-data-dir or set `beeper_data_dir:` in the source's
     sync block to override.)
  2. Confirm read access (Application Support is NOT Full Disk Access
     protected, so this should just work):
       sqlite3 ~/Library/Application\\ Support/BeeperTexts/index.db \\
           \"SELECT COUNT(*) FROM threads;\"

See frankweiler/backend/etl/providers/beeper/EXTRACT.md for details."
            .into(),
        _ => GENERIC_AUTH_HINT.into(),
    }
}

async fn run(summary: &Arc<Mutex<SyncSummary>>, ctrlc: &Arc<Mutex<CtrlcState>>) -> Result<()> {
    let args = Args::parse();
    let _ = args.deterministic;

    // Sample the system clock once if `--now` was omitted, so every
    // phase sees the same timestamp for the duration of the run.
    // Local TZ + offset (e.g. `2026-05-28T14:23:45-07:00`) so the
    // timestamp is meaningful when a human reads it.
    let now = args
        .now
        .unwrap_or_else(|| frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs());

    let cfg = load_config(args.config.as_deref()).context("load config")?;
    status_line!(
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
    status_line!("[frankweiler-sync] data_root = {}", root.display());

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

    // Per-source extract pools are registered with `ctrlc` lazily,
    // by `run_extract_phase` as it opens each source's doltlite_db
    // *before* starting that source's download. That way:
    //   * a Ctrl-C before any open hasn't started yet sees an empty
    //     list and commits nothing (correct — nothing to commit);
    //   * a Ctrl-C mid-extract commits against the *same* pool the
    //     worker is using, so there's no second-connection lock
    //     conflict on the dolt_commit;
    //   * a source whose open *fails* never gets registered, so the
    //     interrupt path doesn't try to commit something that isn't
    //     there.

    // ── Extract ────────────────────────────────────────────────────
    if args.skip_extract {
        status_line!(
            "[frankweiler-sync] extract: skipped (--skip-extract); using staged input_paths"
        );
        let mut s = summary.lock().unwrap();
        for src in cfg.enabled_sources() {
            s.extract.push(PhaseOutcome {
                name: src.name().to_string(),
                type_str: src.type_str().to_string(),
                status: Status::Skipped,
                error: None,
                error_kind: None,
                stats: Some("skipped (--skip-extract)".into()),
                report: None,
            });
        }
    } else {
        let pb = if let Some(playback_root) = args.playback_root.as_ref() {
            let pb = playback_root
                .canonicalize()
                .with_context(|| format!("playback root: {}", playback_root.display()))?;
            std::env::set_var(PLAYBACK_ENV, &pb);
            status_line!("[frankweiler-sync] playback root = {}", pb.display());
            Some(pb)
        } else {
            status_line!("[frankweiler-sync] extract: live (hitting provider APIs)");
            None
        };
        let control = frankweiler_etl::control::ExtractControl {
            reset_and_redownload: args.reset_and_redownload,
            refetch_blobs: args.refetch_blobs,
        };
        if control.reset_and_redownload {
            status_line!(
                "[frankweiler-sync] extract: --reset-and-redownload — wiping every source's \
                 entity tables before fetch (blob_refs preserved, see --refetch-blobs)"
            );
        }
        if control.refetch_blobs {
            status_line!(
                "[frankweiler-sync] extract: --refetch-blobs — wiping every source's \
                 blob_refs before fetch; CAS bytes survive but every attachment re-downloads"
            );
        }
        let outcomes = run_extract_phase(&cfg, pb.as_deref(), &now, &control, ctrlc).await;
        summary.lock().unwrap().extract.extend(outcomes);
    }

    // ── Open index pool + load prior fingerprints ─────────────────
    // The pool is opened *before* translate now: render's commit
    // callback writes into it per doc, so render+index land
    // atomically. A previous version of this file opened the pool
    // only at load time, but with render+load merged we need it
    // here.
    let index_pool = open_index_pool(&cfg).await?;
    init_schema(&index_pool).await?;
    ctrlc.lock().unwrap().index_pool = Some(index_pool.clone());
    let prior_fingerprints = load_fingerprints(&index_pool)
        .await
        .context("load prior fingerprints")?;
    let prior_cursors = load_cursors(&index_pool)
        .await
        .context("load prior cursors")?;
    status_line!(
        "[frankweiler-sync] prior fingerprints: {} docs ({} with cheap-probe cursor)",
        prior_fingerprints.len(),
        prior_cursors.len(),
    );

    // ── Render-and-index-md (= render + per-doc load) ──────────────
    // Render-and-index-md only runs against sources whose extract succeeded (or
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
    // One MultiProgress for the whole translate phase; one bar per
    // source. Sources run concurrently in `spawn_blocking` tasks
    // (mirrors the extract phase), so bars animate together rather
    // than in turn.
    let render_and_index_md_multi = make_multi();
    let load_totals = Arc::new(Mutex::new(summary::LoadOutcome {
        markdowns_loaded: 0,
        markdowns_total: 0,
        rows_inserted: 0,
        error: None,
        commit_hash: None,
        write_lock: None,
    }));

    let prior_fingerprints = Arc::new(prior_fingerprints);
    let prior_cursors = Arc::new(prior_cursors);
    let cfg_arc = Arc::new(cfg.clone());
    let root_arc: Arc<PathBuf> = Arc::new(root.clone());
    let now_arc: Arc<String> = Arc::new(now.clone());
    // One shared write-serialization lock for every per-source worker.
    // It owns a clone of the index pool, serializes concurrent writers
    // so doltlite never sees more than one writer at a time, and (via
    // begin_transaction below) batches every per-doc DELETE/INSERTs/
    // upsert into ONE big `BEGIN ... COMMIT`. Doltlite charges ~50ms
    // per auto-committed statement bundle for the prolly-tree manifest
    // mutation; batching collapses that into a single per-run commit.
    // The lock also records wait/hold timings, which surface in the
    // sync_summary.json's `load.write_lock` block.
    let write_lock = frankweiler_etl::load::WriteLock::new_arc(index_pool.clone());
    // All-or-nothing semantics: if any source's render path errors
    // out, we rollback every write the run had accumulated so far
    // — the grid_rows/markdowns tables stay exactly as they were
    // before this run started. Successful completion runs COMMIT
    // after every worker has joined and BEFORE the dolt_commit.
    write_lock
        .begin_transaction()
        .await
        .context("WriteLock::begin_transaction for translate phase")?;

    let mut set: JoinSet<(String, String, Result<String>)> = JoinSet::new();
    for src in cfg.enabled_sources() {
        let name = src.name().to_string();
        let type_str = src.type_str().to_string();
        if extract_failed.contains(&name) {
            summary
                .lock()
                .unwrap()
                .render_and_index_md
                .push(PhaseOutcome {
                    name,
                    type_str,
                    status: Status::Skipped,
                    error: None,
                    error_kind: None,
                    stats: Some("skipped (extract failed)".into()),
                    report: None,
                });
            continue;
        }
        let bar = make_bar(&render_and_index_md_multi, name.clone());
        let mp = render_and_index_md_multi.clone();
        let src_owned = src.clone();
        let cfg_t = cfg_arc.clone();
        let root_t = root_arc.clone();
        // Separate clone for the on_doc_complete closure so the outer
        // `render_and_index_md_source` call can still borrow `root_t`.
        let root_for_cb = root_arc.clone();
        let now_t = now_arc.clone();
        let pfp = prior_fingerprints.clone();
        let pc = prior_cursors.clone();
        let lt = load_totals.clone();
        let wl = write_lock.clone();
        set.spawn_blocking(move || {
            let sinks: Vec<std::sync::Arc<dyn frankweiler_etl::progress::ProgressSink>> = vec![
                std::sync::Arc::new(IndicatifSink::new(bar, mp)),
                std::sync::Arc::new(TracingSink::new(name.clone())),
            ];
            let progress = Progress::new(std::sync::Arc::new(FanOut::new(sinks)));

            // Per-doc commit callback. We're inside `spawn_blocking`
            // (a dedicated blocking thread), so we can call
            // `Handle::current().block_on(...)` directly — no
            // `block_in_place` needed, since this thread isn't a
            // worker thread.
            let name_for_cb = name.clone();
            let mut on_doc_complete = move |mut doc: RenderedMarkdown| -> Result<()> {
                // Render doesn't always have the user-facing config
                // name (notion/github/gitlab pass empty); fill it in
                // here so documents.source_name is consistent.
                if doc.source_name.is_empty() {
                    doc.source_name = name_for_cb.clone();
                }
                let rows_inserted = doc.rows.len();
                let lock_ref = wl.as_ref();
                let root_path = root_for_cb.as_path();
                let now_str = now_t.as_str();
                tokio::runtime::Handle::current().block_on(async move {
                    apply_one(lock_ref, root_path, &doc, Some(now_str)).await
                })?;
                let mut t = lt.lock().unwrap();
                t.markdowns_loaded += 1;
                t.markdowns_total += 1;
                t.rows_inserted += rows_inserted;
                Ok(())
            };

            let res = render_and_index_md_source(
                &src_owned,
                &cfg_t,
                root_t.as_path(),
                &progress,
                &pfp,
                &pc,
                &mut on_doc_complete,
            )
            .map(|_| "ok".to_string());
            progress.finish("done");
            (name, type_str, res)
        });
    }

    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((name, type_str, res)) => {
                summary
                    .lock()
                    .unwrap()
                    .render_and_index_md
                    .push(summary::outcome_from(&name, &type_str, res));
            }
            Err(e) => {
                // Task panicked — record a generic outcome so the
                // panic shows up in the summary instead of being
                // swallowed.
                let err = anyhow::anyhow!("translate task panicked: {e}");
                summary
                    .lock()
                    .unwrap()
                    .render_and_index_md
                    .push(PhaseOutcome::err("<unknown>", "unknown", &err));
            }
        }
    }

    // Sort outcomes by config-declaration order so the summary stays
    // stable across runs (parallel completion order is nondeterministic).
    let cfg_order: std::collections::HashMap<String, usize> = cfg
        .enabled_sources()
        .enumerate()
        .map(|(i, s)| (s.name().to_string(), i))
        .collect();
    let fallback_pos = cfg_order.len();
    summary
        .lock()
        .unwrap()
        .render_and_index_md
        .sort_by_key(|o| {
            cfg_order
                .get(o.name.as_str())
                .copied()
                .unwrap_or(fallback_pos)
        });

    // All-or-nothing semantics: COMMIT the big batch if every source
    // succeeded; ROLLBACK otherwise so the index DB is left exactly
    // as it was before this run started. We check `translate`
    // outcomes here (extract failures already filter into Skipped
    // translate outcomes above, which are NOT errors). A ROLLBACK
    // failure is logged but not propagated — the open transaction
    // will be closed when the held connection drops.
    let any_render_and_index_md_error = summary
        .lock()
        .unwrap()
        .render_and_index_md
        .iter()
        .any(|o| o.status == Status::Error);
    if any_render_and_index_md_error {
        status_line!("[frankweiler-sync] translate had errors; rolling back the index-DB batch");
        if let Err(e) = write_lock.rollback_transaction().await {
            status_line!("[frankweiler-sync] WriteLock::rollback_transaction failed: {e:#}");
        }
        // Zero out the load totals so the summary reflects what's
        // actually in the index DB (rolled back → nothing).
        let mut t = load_totals.lock().unwrap();
        t.markdowns_loaded = 0;
        t.rows_inserted = 0;
        t.error = Some("translate phase had errors; batch rolled back".into());
    } else {
        write_lock
            .commit_transaction()
            .await
            .context("WriteLock::commit_transaction for translate phase")?;
    }

    // Stash write-lock contention metrics into the load outcome so
    // they end up in sync_summary_*.json. Two numbers to watch:
    //
    //   * `avg_hold_ms` — average time inside `apply_markdown`'s
    //     DELETE/INSERTs/upsert. Per-doc write cost against doltlite;
    //     a small number means writes are cheap, a big number means
    //     each doc is genuinely expensive and a batched-transaction
    //     refactor would help.
    //
    //   * `avg_wait_ms` — average time each `apply_one` call was
    //     queued behind another writer. Large values mean the parallel
    //     render side is producing docs faster than serialized writes
    //     can absorb. If `avg_wait_ms >> avg_hold_ms` the lock itself
    //     is the bottleneck, not the underlying write.
    {
        let mut t = load_totals.lock().unwrap();
        t.write_lock = Some(summary::WriteLockStats::from_metrics(write_lock.metrics()));
    }

    // One commit per run for the index DB. We snapshot the load totals
    // into the commit message so `dolt log` carries the same row-count
    // info the JSON summary has — no need to cross-reference. Failure
    // is logged but not fatal: the data is still on disk, dolt_log just
    // won't have an entry for this run.
    {
        let totals = load_totals.lock().unwrap().clone();
        let extract_names: Vec<String> = summary
            .lock()
            .unwrap()
            .extract
            .iter()
            .filter(|o| o.status == Status::Ok)
            .map(|o| o.name.clone())
            .collect();
        let msg = format!(
            "frankweiler-sync: markdowns_loaded={} markdowns_total={} rows_inserted={} sources=[{}]",
            totals.markdowns_loaded,
            totals.markdowns_total,
            totals.rows_inserted,
            extract_names.join(","),
        );
        match frankweiler_etl::doltlite_raw::commit_run(&index_pool, &msg).await {
            Ok(hash) => {
                if let Some(h) = hash.as_deref() {
                    status_line!("[frankweiler-sync] index commit: {h}");
                }
                load_totals.lock().unwrap().commit_hash = hash;
            }
            Err(e) => {
                status_line!("[frankweiler-sync] index commit failed: {e:#}");
            }
        }
    }

    summary.lock().unwrap().load = Some(load_totals.lock().unwrap().clone());

    // Under stock SQLite (WAL mode) we used to TRUNCATE-checkpoint the
    // WAL here so the on-disk `.db` was self-contained for downstream
    // genrules. On doltlite there's no WAL sidecar — the chunk store
    // is the source of truth and writes land in the main file as
    // they happen. The pragma is a no-op (doltlite rejects
    // `wal_checkpoint` as "not configurable") so we just drop the
    // pool and trust that all writes (including the dolt_commit we
    // just issued) are durable.
    drop(index_pool);
    status_line!("[frankweiler-sync] wrote {}", cfg.dolt_db_path().display());
    status_line!(
        "[frankweiler-sync] wrote {}/",
        root.join("rendered_md").display()
    );

    // ── QMD index ──────────────────────────────────────────────────
    if !cfg.qmd.skip {
        match build_qmd_index(&root, cfg.qmd.models_dir.as_deref()) {
            Ok(outcome) => {
                status_line!("[frankweiler-sync] wrote {}", outcome.index_path.display());
                let mut s = summary.lock().unwrap();
                s.qmd_index = Some(PhaseOutcome::ok(
                    "qmd",
                    "qmd",
                    outcome.index_path.display().to_string(),
                ));
                s.qmd_status = outcome.status_output;
            }
            Err(e) => {
                status_line!("[frankweiler-sync] qmd index FAILED: {e:#}");
                summary.lock().unwrap().qmd_index = Some(PhaseOutcome::err("qmd", "qmd", &e));
            }
        }
    } else {
        status_line!("[frankweiler-sync] qmd index: skipped (qmd.skip=true)");
    }
    Ok(())
}

/// Open the index doltlite at `<data_root>/<dolt.db_filename>`. Created
/// if missing. WAL mode so `apply_one` from the translate-side
/// per-doc callback can write concurrently with reads; the caller is
/// responsible for the closing TRUNCATE checkpoint so the on-disk
/// `.db` is self-contained (downstream genrules copy only the .db,
/// not the -wal sidecar).
async fn open_index_pool(cfg: &Config) -> Result<sqlx::sqlite::SqlitePool> {
    let db_path = cfg.dolt_db_path();
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)?;
    }
    status_line!("[frankweiler-sync] doltlite db = {}", db_path.display());
    // doltlite manages its own chunk-store; `PRAGMA journal_mode = …`
    // is rejected as "not configurable on doltlite-format databases",
    // so we leave it at default. The WAL_CHECKPOINT call we used to
    // do after the translate phase to flush sidecars is now a no-op
    // for doltlite — see the comment at that callsite.
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
        .create_if_missing(true);
    // Pool size 1: doltlite's HEAD pointer + working tree are
    // per-connection, so multiple pool connections produce silent
    // dolt_log dropouts and `commit conflict` errors on interleaved
    // writes. See `frankweiler_etl::doltlite_raw` module docs for
    // the full story (and the dolt-team-confirmed advice).
    //
    // Implication for the SIGINT handler: while the translate
    // writer holds the connection, a Ctrl-C can't acquire the pool
    // to issue its best-effort `dolt_commit` against the
    // pre-translate working set. That's fine — sqlx drops the
    // writer's connection on shutdown, doltlite rolls back the
    // in-flight transaction, and the next run picks up cleanly
    // (every translate write is idempotent). If you ever need
    // an actually-async-safe interrupt commit, open a separate
    // one-shot connection from the SIGINT path rather than
    // widening the pool.
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("open doltlite at {}", db_path.display()))
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
    control: &frankweiler_etl::control::ExtractControl,
    ctrlc: &Arc<Mutex<CtrlcState>>,
) -> Vec<PhaseOutcome> {
    let mut outcomes: Vec<PhaseOutcome> = Vec::new();
    let mut plans: Vec<ExtractPlan> = Vec::new();
    for s in cfg.enabled_sources() {
        let Some(plan_res) = ExtractPlan::for_source(s, cfg, playback_root, now, control) else {
            continue;
        };
        match plan_res {
            Ok(plan) => plans.push(plan),
            Err(e) => outcomes.push(PhaseOutcome::err(s.name(), s.type_str(), &e)),
        }
    }

    // Snapshot every source's raw-store files BEFORE any writer opens,
    // so each source's report has an honest before/after baseline for
    // bytes and per-table row counts. Keyed by source name; the same
    // contexts are registered with the SIGINT state so an interrupted
    // run can still produce reports. Sources with no doltlite store
    // (file-tree-backed) snapshot as empty and get dropped later when
    // the report turns out empty.
    let mut report_ctxs: std::collections::HashMap<String, ReportCtx> =
        std::collections::HashMap::new();
    for plan in &plans {
        let entity_path = frankweiler_etl::doltlite_raw::db_path_for(&plan.out_dir);
        let (before_events, before_blobs) =
            frankweiler_etl::extract_metrics::snapshot_source(&entity_path).await;
        let ctx = ReportCtx {
            name: plan.name.clone(),
            entity_path,
            before_events,
            before_blobs,
            metrics: plan.metrics.clone(),
            diagnostics: plan.diagnostics.clone(),
        };
        report_ctxs.insert(plan.name.clone(), ctx.clone());
        ctrlc.lock().unwrap().extract_reports.push(ctx);
    }

    // Every source is a Program-A `DataProcessor` set that owns its store: the
    // orchestrator opens no pool and registers no `ExtractPoolEntry`. Each
    // source shares the SIGINT `CheckpointSink` and registers its own opaque
    // `Checkpoint` when it opens its store inside `run`.
    let mut opened: Vec<ExtractPlan> = Vec::with_capacity(plans.len());
    for mut plan in plans {
        plan.checkpoints = ctrlc.lock().unwrap().checkpoints.clone();
        opened.push(plan);
    }
    let mut plans = opened;

    // Each provider creates whatever on-disk layout it needs:
    //   - doltlite-backed (anthropic, chatgpt, notion, slack) write to
    //     `<data_root>/raw/<name>/entities.doltlite_db`; `doltlite_raw::open`
    //     creates the file's parent dir automatically.
    //   - file-tree-backed (github, gitlab) call `create_dir_all` on
    //     their out_dir as their first extract step.
    // We used to pre-create `<data_root>/raw/<name>/` here for everyone,
    // which left empty leftover dirs for the doltlite-backed providers.
    //
    // One MultiProgress for the whole extract phase; one bar per plan
    // fanned out to a TracingSink so structured consumers see the same
    // stream.
    let multi = make_multi();
    for plan in &mut plans {
        let bar = make_bar(&multi, plan.name.clone());
        let sinks: Vec<std::sync::Arc<dyn frankweiler_etl::progress::ProgressSink>> = vec![
            std::sync::Arc::new(IndicatifSink::new(bar, multi.clone())),
            std::sync::Arc::new(TracingSink::new(plan.name.clone())),
        ];
        let fanout: std::sync::Arc<dyn frankweiler_etl::progress::ProgressSink> =
            std::sync::Arc::new(FanOut::new(sinks));
        // Let live counter updates re-render the bar's `api=… rows[…]`
        // suffix, then wrap so the provider's own `set_message` carries
        // that suffix too. Children (per-unit inner bars) stay unwrapped.
        plan.metrics.attach_bar(fanout.clone());
        let sink: std::sync::Arc<dyn frankweiler_etl::progress::ProgressSink> = std::sync::Arc::new(
            frankweiler_etl::extract_metrics::MetricsSink::new(fanout, plan.metrics.clone()),
        );
        plan.progress = Progress::new(sink);
    }

    if cfg.sync.parallel {
        let mut set: JoinSet<(String, &'static str, Result<String>)> = JoinSet::new();
        for plan in plans {
            let name = plan.name.clone();
            let type_str = plan.type_str;
            set.spawn(async move {
                tracing::info!(source = %name, kind = type_str, "extract: start");
                let r = plan
                    .run()
                    .await
                    .with_context(|| format!("extract {name} (type={type_str})"));
                match &r {
                    Ok(s) => tracing::info!(source = %name, summary = %s, "extract: done"),
                    Err(e) => tracing::error!(
                        source = %name,
                        error = %format!("{e:#}"),
                        "extract: FAIL"
                    ),
                };
                (name, type_str, r)
            });
        }
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((name, type_str, r)) => {
                    let outcome = summary::outcome_from(&name, type_str, r);
                    outcomes.push(finalize_extract_outcome(outcome, &report_ctxs).await);
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
            tracing::info!(source = %name, kind = type_str, "extract: start");
            let r = plan
                .run()
                .await
                .with_context(|| format!("extract {name} (type={type_str})"));
            match &r {
                Ok(s) => tracing::info!(source = %name, summary = %s, "extract: done"),
                Err(e) => tracing::error!(
                    source = %name,
                    error = %format!("{e:#}"),
                    "extract: FAIL"
                ),
            };
            let outcome = summary::outcome_from(&name, type_str, r);
            outcomes.push(finalize_extract_outcome(outcome, &report_ctxs).await);
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

/// Attach the general "what changed" report to a just-finished source's
/// outcome. Runs at join time — the source has already committed, closed
/// its writer, and cleared its progress bar — so the after-snapshot is
/// lock-safe and the INFO line lands "as the bar goes away". A no-op for
/// sources with no registered context (e.g. a `<unknown>` panic record).
async fn finalize_extract_outcome(
    outcome: PhaseOutcome,
    report_ctxs: &std::collections::HashMap<String, ReportCtx>,
) -> PhaseOutcome {
    let Some(ctx) = report_ctxs.get(&outcome.name) else {
        return outcome;
    };
    let report = ctx.assemble().await;
    if !report.is_empty() {
        tracing::info!(
            source = %outcome.name,
            summary = %report.summary_line(),
            "extract: report"
        );
    }
    outcome.with_report(report)
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
    /// The source's Program-A `DataProcessor`s for the extract wave. They own
    /// their store (open/commit/checkpoint); the orchestrator opens no pool and
    /// issues no commit.
    processors: Vec<Box<dyn frankweiler_etl::processor::DataProcessor>>,
    /// Shared interrupt-commit sink (from [`CtrlcState`]) the processors
    /// register their [`Checkpoint`](frankweiler_etl::processor::Checkpoint)
    /// hooks into. A standalone placeholder until `run_extract_phase` swaps in
    /// the real shared one.
    checkpoints: Arc<frankweiler_etl::processor::CheckpointSink>,
    /// Cross-provider knobs (e.g. `--reset-and-redownload`). Flows
    /// from the CLI through `ExtractPlan::for_source` into each
    /// provider's `FetchOptions.control`.
    control: frankweiler_etl::control::ExtractControl,
    /// Live "what changed" counters for this source, installed as the
    /// ambient extract-metrics context for the duration of `run()` so the
    /// shared write/HTTP chokepoints record into it. Shared (via the
    /// `Arc`) with the orchestrator's report-assembly and the Ctrl-C
    /// handler.
    metrics: Arc<frankweiler_etl::extract_metrics::ExtractMetrics>,
    /// Resolved (global ⊕ per-source) rate-limit give-up bounds. Installed
    /// as the ambient [`frankweiler_etl::retry::RetryGuard`] for the
    /// duration of `run()` so the shared HTTP chokepoint enforces them for
    /// every provider, without provider-side code.
    extract_params: frankweiler_core::config::ExtractParams,
    /// Per-source WARN/ERROR capture buffer, installed as the ambient
    /// diagnostics context for the duration of `run()` so the global
    /// [`frankweiler_obs::diagnostics`] layer collects every warning/error
    /// (wire + provider-internal) into it. Shared (via the `Arc`) with the
    /// report-assembly and Ctrl-C paths, exactly like `metrics`.
    diagnostics: Arc<frankweiler_obs::diagnostics::Diagnostics>,
}

fn build_source_plan(
    src: &SourceConfig,
    cfg: &Config,
    playback_root: Option<&Path>,
) -> Option<Result<frankweiler_etl::processor::SourcePlan>> {
    let shared = src.resolved_shared(cfg);
    let common = frankweiler_etl::processor::PlanCommon {
        name: src.name().to_string(),
        raw_path: src.resolved_raw_path(&cfg.data_root),
        input_path: src.resolved_input_path(&cfg.data_root),
        blob_size_limit_bytes: shared.blob_size_limit_bytes,
        playback_root: playback_root.map(|p| p.to_path_buf()),
        event_tape_enabled: shared.event_tape.unwrap_or_default().enabled,
        max_sequential_failures: shared.extract_params.max_sequential_failures(),
    };
    let stanza = match serde_yaml::to_value(src).context("serialize source stanza") {
        Ok(s) => s,
        Err(e) => return Some(Err(e)),
    };
    let plan = match src.type_str() {
        "email" => serde_yaml::from_value::<frankweiler_etl_email_config::EmailConfig>(stanza)
            .context("parse email config")
            .and_then(|c| frankweiler_etl_email::processor::plan(common, c)),
        "claude_api" | "claude_export" => {
            serde_yaml::from_value::<frankweiler_etl_anthropic_config::AnthropicConfig>(stanza)
                .context("parse anthropic config")
                .and_then(|c| frankweiler_etl_anthropic::processor::plan(common, c))
        }
        "chatgpt_api" => {
            serde_yaml::from_value::<frankweiler_etl_chatgpt_config::ChatgptConfig>(stanza)
                .context("parse chatgpt config")
                .and_then(|c| frankweiler_etl_chatgpt::processor::plan(common, c))
        }
        "github_api" => {
            serde_yaml::from_value::<frankweiler_etl_github_config::GithubConfig>(stanza)
                .context("parse github config")
                .and_then(|c| frankweiler_etl_github::processor::plan(common, c))
        }
        "gitlab_api" => {
            serde_yaml::from_value::<frankweiler_etl_gitlab_config::GitlabConfig>(stanza)
                .context("parse gitlab config")
                .and_then(|c| frankweiler_etl_gitlab::processor::plan(common, c))
        }
        "sms_backup_restore" => serde_yaml::from_value::<
            frankweiler_etl_sms_backup_restore_config::SmsBackupRestoreConfig,
        >(stanza)
        .context("parse sms_backup_restore config")
        .and_then(|c| frankweiler_etl_sms_backup_restore::processor::plan(common, c)),
        "google_takeout" => serde_yaml::from_value::<
            frankweiler_etl_google_takeout_config::GoogleTakeoutConfig,
        >(stanza)
        .context("parse google_takeout config")
        .and_then(|c| frankweiler_etl_google_takeout::processor::plan(common, c)),
        "carddav" => {
            serde_yaml::from_value::<frankweiler_etl_carddav_config::CarddavConfig>(stanza)
                .context("parse carddav config")
                .and_then(|c| frankweiler_etl_contacts::processor::plan(common, c))
        }
        "beeper" => serde_yaml::from_value::<frankweiler_etl_beeper_config::BeeperConfig>(stanza)
            .context("parse beeper config")
            .and_then(|c| frankweiler_etl_beeper::processor::plan(common, c)),
        "signal_backup" => {
            serde_yaml::from_value::<frankweiler_etl_signal_config::SignalConfig>(stanza)
                .context("parse signal config")
                .and_then(|c| frankweiler_etl_signal::processor::plan(common, c))
        }
        "yolink" => serde_yaml::from_value::<frankweiler_etl_yolink_config::YolinkConfig>(stanza)
            .context("parse yolink config")
            .and_then(|c| frankweiler_etl_yolink::processor::plan(common, c)),
        "slack_api" => serde_yaml::from_value::<frankweiler_etl_slack_config::SlackConfig>(stanza)
            .context("parse slack config")
            .and_then(|c| frankweiler_etl_slack::processor::plan(common, c)),
        "perseus" => {
            serde_yaml::from_value::<frankweiler_etl_perseus_config::PerseusConfig>(stanza)
                .context("parse perseus config")
                .and_then(|c| frankweiler_etl_perseus::processor::plan(common, c))
        }
        "linkedin" => {
            serde_yaml::from_value::<frankweiler_etl_linkedin_config::LinkedinConfig>(stanza)
                .context("parse linkedin config")
                .and_then(|c| frankweiler_etl_linkedin::processor::plan(common, c))
        }
        "whatsapp_backup" => {
            serde_yaml::from_value::<frankweiler_etl_whatsapp_config::WhatsappConfig>(stanza)
                .context("parse whatsapp config")
                .and_then(|c| frankweiler_etl_whatsapp::processor::plan(common, c))
        }
        "notion_api" => {
            serde_yaml::from_value::<frankweiler_etl_notion_config::NotionConfig>(stanza)
                .context("parse notion config")
                .and_then(|c| frankweiler_etl_notion::processor::plan(common, c))
        }
        // Every provider is migrated; nothing falls through to ExtractKind.
        _ => return None,
    };
    Some(plan)
}

/// Wrap a source's extract processors in an [`ExtractPlan`] carrying the common
/// per-source machinery (progress, metrics, diagnostics). The store itself —
/// pool, DDL, commit, interrupt `Checkpoint` — is owned by the processors.
fn extract_plan_from_processors(
    src: &SourceConfig,
    cfg: &Config,
    now: &str,
    control: &frankweiler_etl::control::ExtractControl,
    processors: Vec<Box<dyn frankweiler_etl::processor::DataProcessor>>,
) -> ExtractPlan {
    ExtractPlan {
        name: src.name().to_string(),
        type_str: src.type_str(),
        out_dir: src.resolved_raw_path(&cfg.data_root),
        now: now.to_string(),
        progress: Progress::noop(),
        processors,
        // Placeholder; `run_extract_phase` swaps in the shared sink from
        // `CtrlcState` before the processors run.
        checkpoints: Arc::new(frankweiler_etl::processor::CheckpointSink::new()),
        control: control.clone(),
        metrics: frankweiler_etl::extract_metrics::ExtractMetrics::new(),
        extract_params: src.resolved_shared(cfg).extract_params,
        diagnostics: frankweiler_obs::diagnostics::Diagnostics::new(),
    }
}

impl ExtractPlan {
    /// `None` when the source is translate-only (no `sync:` block).
    fn for_source(
        src: &SourceConfig,
        cfg: &Config,
        playback_root: Option<&Path>,
        now: &str,
        control: &frankweiler_etl::control::ExtractControl,
    ) -> Option<Result<Self>> {
        if !src.is_managed() {
            return None;
        }
        // Every provider builds its own `DataProcessor`s (owning their store)
        // via `build_source_plan`; there is no `ExtractKind` path any more.
        build_source_plan(src, cfg, playback_root).map(|res| {
            res.map(|sp| extract_plan_from_processors(src, cfg, now, control, sp.extract))
        })
    }

    /// Returns a one-line per-source summary on success. Provider-specific
    /// shape — what makes it onto stderr is whatever's interesting for
    /// that source (slack media outcomes including `error` counts, claude
    /// fetched/skipped/errors, etc).
    ///
    /// Installs this source's [`ExtractMetrics`] as the ambient task-local
    /// context so the shared write/HTTP chokepoints record into it for the
    /// whole download. Everything is awaited on this one task, so the
    /// context is visible to every chokepoint the provider reaches.
    async fn run(self) -> Result<String> {
        let metrics = self.metrics.clone();
        let diagnostics = self.diagnostics.clone();
        // Install all three ambient contexts for the whole source extract on
        // this one task: the metrics counters, the rate-limit give-up guard,
        // and the WARN/ERROR diagnostics buffer. The shared HTTP chokepoint
        // resolves the guard via `retry::current_or_default()`; the global
        // tracing layer funnels warnings/errors into the diagnostics buffer —
        // both enforced for every provider with no provider-side code.
        let guard = frankweiler_etl::retry::RetryGuard::from_params(&self.extract_params);
        frankweiler_obs::diagnostics::scope(
            diagnostics,
            frankweiler_etl::retry::scope(
                guard,
                frankweiler_etl::extract_metrics::scope(metrics, self.run_processors()),
            ),
        )
        .await
    }

    /// Run a Program-A processor-based source's extract wave. Each processor
    /// owns its store and registers its own `Checkpoint`; the orchestrator
    /// supplies only the storage-agnostic [`RunCtx`]. No pool, no commit, no
    /// `dolt_commit` here — that all lives inside the processor.
    async fn run_processors(self) -> Result<String> {
        // `root` is unused by extract processors (they derive their own store
        // path from config); pass the source's out_dir to satisfy the ctx.
        // `prior_fingerprints` is a translate-wave concern — empty here.
        let empty_fingerprints = std::collections::HashMap::new();
        let mut summaries = Vec::with_capacity(self.processors.len());
        for proc in &self.processors {
            let ctx = frankweiler_etl::processor::RunCtx::for_extract(
                &self.name,
                &self.out_dir,
                &self.now,
                &self.progress,
                &self.control,
                &empty_fingerprints,
                self.checkpoints.as_ref(),
            );
            let summary = proc
                .run(&ctx)
                .await
                .with_context(|| format!("processor {}", proc.id()))?;
            summaries.push(summary);
        }
        self.progress.finish_and_clear();
        Ok(summaries.join(" | "))
    }
}

// ─────────────────────────────────────────────────────────────────────
// Render-and-index-md phase
// ─────────────────────────────────────────────────────────────────────

/// Render-and-index-md: turn one source's `input_path` into the
/// workspace's `rendered_md/` + sidecar tree. ClaudeExport shares the
/// anthropic renderer since the on-disk shape is the same.
fn render_and_index_md_source(
    src: &SourceConfig,
    cfg: &Config,
    root: &Path,
    progress: &Progress,
    prior_fingerprints: &std::collections::HashMap<String, String>,
    _prior_cursors: &std::collections::HashMap<String, String>,
    on_doc_complete: &mut render_and_index_md::OnDoc,
) -> Result<()> {
    // Migrated providers render through their translate `DataProcessor`s
    // (provider owns its config + render path); the rest stay on the
    // opaque-stanza `renderer_for` registry. Same `build_source_plan` seam the
    // extract phase uses, so a provider is migrated in exactly one place.
    if let Some(res) = build_source_plan(src, cfg, None) {
        let source_plan = res?;
        return render_processor_translate(
            src.name(),
            &source_plan.translate,
            root,
            progress,
            prior_fingerprints,
            on_doc_complete,
        );
    }
    let raw_dir = src.resolved_raw_path(&cfg.data_root);
    let name = src.name();
    // The render registry is config-opaque: it dispatches on the `type`
    // string and parses any knobs out of this stanza itself. We hand it
    // the source serialized back to YAML; once the config loader carries
    // raw stanzas natively this round-trip goes away.
    let stanza = serde_yaml::to_value(src).context("serialize source config stanza")?;
    let renderer = render_and_index_md::renderer_for(src.type_str(), &stanza)?;
    status_line!(
        "[render_and_index_md] {name} ({}): {}",
        src.type_str(),
        raw_dir.display()
    );
    let ctx = render_and_index_md::RenderCtx {
        root,
        name,
        raw_path: &raw_dir,
        progress,
        prior_fingerprints,
    };
    renderer.run(&ctx, on_doc_complete)
}

/// Drive a migrated source's translate wave (its translate `DataProcessor`s),
/// fusing Load through `on_doc_complete` exactly like the render registry.
/// Called from a `spawn_blocking` thread.
fn render_processor_translate(
    name: &str,
    processors: &[Box<dyn frankweiler_etl::processor::DataProcessor>],
    root: &Path,
    progress: &Progress,
    prior_fingerprints: &std::collections::HashMap<String, String>,
    on_doc_complete: &mut render_and_index_md::OnDoc,
) -> Result<()> {
    // Translate processors don't persist a raw store, so they register no
    // checkpoints and read no extract control; supply throwaway values to
    // satisfy the (extract-shaped) `RunCtx`.
    let checkpoints = frankweiler_etl::processor::CheckpointSink::new();
    let control = frankweiler_etl::control::ExtractControl::default();
    let now = String::new();
    for proc in processors {
        // `ctx` reborrows `on_doc_complete` for this iteration and drops at the
        // end of it, returning the unique borrow before the next processor.
        let ctx = frankweiler_etl::processor::RunCtx::for_translate(
            name,
            root,
            &now,
            progress,
            &control,
            prior_fingerprints,
            &checkpoints,
            on_doc_complete,
        );
        // Drive the translate future with `futures`' executor, NOT tokio's:
        // the processor's body is synchronous render work, and the fused-Load
        // callback (`emit_doc` → the orchestrator's `on_doc_complete`) does its
        // own `tokio::block_on(apply_one)`. Using tokio here too would nest two
        // tokio runtimes on this `spawn_blocking` thread and panic; `futures`'
        // executor enters no tokio context, so the callback's `block_on` stays
        // the only one — exactly as the old synchronous renderer behaved.
        futures::executor::block_on(proc.run(&ctx))?;
    }
    Ok(())
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
            SourceConfig::Beeper { .. } => Box::new(BeeperSynth::new(input.clone())),
            SourceConfig::Carddav { .. } => {
                // No synthesizer yet — the carddav translate path is
                // a follow-up. Skip synth quietly so a config that
                // mixes carddav with synth-supported sources doesn't
                // error out.
                status_line!(
                    "[synth] {} (carddav): skipped (no synthesizer yet)",
                    src.name()
                );
                continue;
            }
            SourceConfig::Email { .. } => {
                // No synthesizer yet — JMAP playback fixtures are a
                // follow-up. Skip quietly.
                status_line!(
                    "[synth] {} (email): skipped (no synthesizer yet)",
                    src.name()
                );
                continue;
            }
            SourceConfig::Linkedin { fetch_photos, .. } => {
                // File-backed for the CSV walk; the only HTTP it makes is
                // the optional connection-photo fetch. Synthesize those
                // fixtures iff that's enabled, else there's nothing to
                // play back.
                if *fetch_photos {
                    Box::new(frankweiler_etl_linkedin::synthesize::LinkedinSynth::new(
                        input.clone(),
                    ))
                } else {
                    status_line!(
                        "[synth] {} (linkedin): skipped (photo fetch off)",
                        src.name()
                    );
                    continue;
                }
            }
            SourceConfig::GoogleTakeout { .. } => {
                // File-backed (no HTTP to play back); synth is a no-op.
                status_line!(
                    "[synth] {} (google_takeout): skipped (file-backed, no extract HTTP)",
                    src.name()
                );
                continue;
            }
            SourceConfig::SmsBackupRestore { .. } => {
                // File-backed (no HTTP to play back); synth is a no-op.
                status_line!(
                    "[synth] {} (sms_backup_restore): skipped (file-backed, no extract HTTP)",
                    src.name()
                );
                continue;
            }
            SourceConfig::Perseus { .. } => {
                // Perseus has no extract phase (no HTTP playback to
                // synthesize against), so synth is a no-op.
                status_line!(
                    "[synth] {} (perseus): skipped (translate-only, no extract)",
                    src.name()
                );
                continue;
            }
            SourceConfig::SignalBackup { .. } => {
                // No playback synthesizer yet — Signal extract is
                // local-file-only, no HTTP to play back.
                status_line!(
                    "[synth] {} (signal_backup): skipped (no synthesizer yet)",
                    src.name()
                );
                continue;
            }
            SourceConfig::Yolink { .. } => {
                // No playback synthesizer for yolink yet — would need
                // to capture per-window CSV bodies into a fixture
                // tree. Skip quietly so a mixed config doesn't error.
                status_line!(
                    "[synth] {} (yolink): skipped (no synthesizer yet)",
                    src.name()
                );
                continue;
            }
            SourceConfig::WhatsAppBackup { .. } => {
                // WhatsApp extract is local-file-only (decrypt + mirror);
                // no HTTP playback. Skip synth the same way Signal does.
                status_line!(
                    "[synth] {} (whatsapp_backup): skipped (no synthesizer)",
                    src.name()
                );
                continue;
            }
        };
        let report = synth
            .synthesize(out)
            .with_context(|| format!("synthesize {} ({})", src.name(), src.type_str()))?;
        status_line!(
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

fn build_qmd_index(
    root: &Path,
    models_dir: Option<&Path>,
) -> Result<frankweiler_qmd_indexer::IndexOutcome> {
    let mut opts = frankweiler_qmd_indexer::IndexOptions::new(root);
    if let Some(d) = models_dir {
        opts.models_dir = d.to_path_buf();
    }
    frankweiler_qmd_indexer::run_index(&opts).context("qmd index build")
}

#[cfg(test)]
// Test diagnostics; cargo-test captures stderr. No MP in scope.
#[allow(clippy::disallowed_macros)]
mod interrupt_tests {
    //! Tests for the SIGINT-handler commit path. We can't easily send
    //! SIGINT to ourselves mid-`#[tokio::test]` and observe what
    //! [`interrupt_commit_all`] did — async-signal-safe tokio teardown
    //! is its own rabbit hole. Instead we drive the function directly
    //! against a `CtrlcState` we populate by hand, which is exactly
    //! the same state the real SIGINT handler would see (because
    //! [`run`] writes into the same `Arc<Mutex<CtrlcState>>` that
    //! handler reads). The commit-landing assertions are the
    //! load-bearing piece — the actual signal plumbing is just glue.
    use super::*;
    use frankweiler_etl::doltlite_raw as dr;
    use serde_json::json;
    use tempfile::tempdir;

    async fn has_dolt(pool: &sqlx::sqlite::SqlitePool) -> bool {
        dr::has_dolt_extensions(pool).await
    }

    /// Populate `CtrlcState` with one index pool + one pre-opened
    /// extract pool, call [`interrupt_commit_all`], then verify:
    ///   * the index pool got exactly one new commit
    ///   * the extract pool got exactly one new commit, against the
    ///     same connection the SIGINT path uses (no reopen).
    ///
    /// Mirrors the production state the SIGINT handler sees at any
    /// point after `run_extract_phase` has begun opening pools: each
    /// source that opened successfully is in `extract_pools`; sources
    /// that failed to open were never registered (so the interrupt
    /// path doesn't see them at all — there's no
    /// "never-materialized" entry to defensively skip).
    #[tokio::test]
    async fn interrupt_commit_all_commits_index_and_extract_dbs() {
        let d = tempdir().unwrap();
        let index_db = d.path().join("backend_index.doltlite_db");
        let extract_db = d.path().join("raw").join("source_a.doltlite_db");

        // Sanity-prime the index DB so dolt_log has a head to count from.
        // Use the same DDL the real index DB carries — empty extra DDL
        // is fine; we just need the file to exist and be doltlite-format.
        let index_pool = dr::open(&index_db, &[]).await.unwrap();

        if !has_dolt(&index_pool).await {
            eprintln!(
                "[interrupt_tests] stock libsqlite3 — full assertion skipped. \
                 Run under bazel (which links doltlite) for the load-bearing check."
            );
            return;
        }

        // Both pools need per-session committer identity. doltlite
        // doesn't persist this, so the real sync binary configures
        // it once per connection at sync start — we'd do the same in
        // production. Here we configure right before the interrupt.
        for q in [
            "SELECT dolt_config('user.name', 'frankweiler-interrupt-test')",
            "SELECT dolt_config('user.email', 'interrupt@frankweiler.local')",
        ] {
            sqlx::query(q).execute(&index_pool).await.unwrap();
        }

        // Open the extract DB and stage a row so the interrupt commit
        // has something to commit (otherwise dolt would say "nothing
        // to commit" and skip the new log entry). The pool stays open
        // for the duration of the test — production mirror: each
        // source's pool is held across its whole extract run.
        let extract_pool = dr::open(&extract_db, &[]).await.unwrap();
        for q in [
            "SELECT dolt_config('user.name', 'frankweiler-interrupt-test')",
            "SELECT dolt_config('user.email', 'interrupt@frankweiler.local')",
        ] {
            sqlx::query(q).execute(&extract_pool).await.unwrap();
        }
        let _ = dr::start_run(&extract_pool, &json!({"phase": "extract"}))
            .await
            .unwrap();

        // Stage an uncommitted change on the index DB too, so the
        // interrupt path has something to commit there.
        sqlx::query("CREATE TABLE IF NOT EXISTS canary (id INTEGER PRIMARY KEY, note TEXT)")
            .execute(&index_pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO canary (note) VALUES ('staged-before-interrupt')")
            .execute(&index_pool)
            .await
            .unwrap();

        // Snapshot dolt_log counts BEFORE the interrupt so we can
        // assert exactly one new entry lands per DB.
        let index_log_before: i64 = sqlx::query_scalar("SELECT count(*) FROM dolt_log()")
            .fetch_one(&index_pool)
            .await
            .unwrap();
        // Snapshot via the held pool — same connection that will
        // issue the interrupt commit, so its post-commit view will
        // see exactly the right delta.
        let extract_log_before: i64 = sqlx::query_scalar("SELECT count(*) FROM dolt_log()")
            .fetch_one(&extract_pool)
            .await
            .unwrap();

        // Build the shared state EXACTLY as the run() body would:
        // index pool live + extract_pools populated with the live
        // pre-opened per-source pools (registered as `run_extract_phase`
        // opens them).
        let state = Arc::new(Mutex::new(CtrlcState {
            index_pool: Some(index_pool.clone()),
            extract_pools: vec![ExtractPoolEntry {
                name: "source_a".to_string(),
                pool: extract_pool.clone(),
            }],
            extract_reports: Vec::new(),
            ..Default::default()
        }));

        // Invoke the same function the SIGINT handler invokes.
        interrupt_commit_all(&state).await;

        // ── Verify ────────────────────────────────────────────────
        // Index DB: exactly one new dolt_log entry, with the
        // interrupt-stamped message. We count via a FRESH pool because
        // doltlite's per-connection view doesn't see commits issued
        // from a different connection inside the same pool — the
        // original index_pool we held would report a stale count.
        index_pool.close().await;
        let verify_index = dr::open(&index_db, &[]).await.unwrap();
        let index_log_after: i64 = sqlx::query_scalar("SELECT count(*) FROM dolt_log()")
            .fetch_one(&verify_index)
            .await
            .unwrap();
        assert_eq!(
            index_log_after - index_log_before,
            1,
            "expected exactly one new index commit from interrupt"
        );
        let index_head_msg: String =
            sqlx::query_scalar("SELECT message FROM dolt_log() ORDER BY date DESC LIMIT 1")
                .fetch_one(&verify_index)
                .await
                .unwrap();
        assert!(
            index_head_msg.contains("interrupted (Ctrl-C)"),
            "index interrupt commit message wrong: {index_head_msg}"
        );

        // Extract DB: count via the same held pool the commit ran
        // through. That connection sees its own commit immediately.
        let extract_log_after: i64 = sqlx::query_scalar("SELECT count(*) FROM dolt_log()")
            .fetch_one(&extract_pool)
            .await
            .unwrap();
        assert_eq!(
            extract_log_after - extract_log_before,
            1,
            "expected exactly one new extract commit from interrupt"
        );
        let extract_head_msg: String =
            sqlx::query_scalar("SELECT message FROM dolt_log() ORDER BY date DESC LIMIT 1")
                .fetch_one(&extract_pool)
                .await
                .unwrap();
        assert!(
            extract_head_msg.contains("interrupted (Ctrl-C)")
                && extract_head_msg.contains("source_a"),
            "extract interrupt commit message wrong: {extract_head_msg}"
        );
        extract_pool.close().await;

        verify_index.close().await;
    }
}
