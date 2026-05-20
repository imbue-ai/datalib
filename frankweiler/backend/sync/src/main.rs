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
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use frankweiler_core::config::{
    load_config, ChatgptApiSync, ClaudeApiSync, Config, GithubApiSync, GitlabApiSync,
    NotionApiSync, SlackApiSync, SourceConfig,
};
use frankweiler_core::dolt_server::DoltServer;
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
use sqlx::mysql::MySqlPoolOptions;
use tokio::task::JoinSet;

mod progress;
use crate::progress::{make_bar, make_multi, IndicatifSink};

#[derive(Debug, Parser)]
#[command(
    name = "frankweiler-sync",
    about = "Config-driven ETL: extract every enabled source, translate, load into Dolt at <data_root>/dolt_db/ + rendered_md/ + qmd/index.sqlite"
)]
struct Args {
    /// Path to the YAML config. Defaults to `$FRANKWEILER_CONFIG` or
    /// `~/.config/frankweiler/config.yaml`. See `frankweiler_core::config`.
    #[arg(long, env = "FRANKWEILER_CONFIG")]
    config: Option<PathBuf>,

    /// Fixed "now" timestamp threaded through downstream renderers and
    /// the dolt load. ISO-8601 / RFC-3339; required for deterministic
    /// builds and for the Bazel genrule.
    #[arg(long)]
    now: String,

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
}

fn free_port() -> Result<u16> {
    let l = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(l.local_addr()?.port())
}

#[tokio::main]
async fn main() {
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "full");
    }
    match run().await {
        Ok(()) => {}
        Err(e) => {
            render_error(&e);
            std::process::exit(1);
        }
    }
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
    s.contains("HTTP 401")
        || s.contains("HTTP 403")
        || s.contains("cf-mitigated")
        || s.contains("challenge")
        || s.contains("Unauthorized")
        || s.contains("Forbidden")
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
        "chatgpt_api" => "\
chatgpt access token expired or missing.

  1. Open https://chatgpt.com in a logged-in browser, then in DevTools
     console run:
       const r = await fetch('/api/auth/session');
       const j = await r.json();
       console.log(`latchkey auth set chatgpt -H \"Authorization: Bearer ${j.accessToken}\"`);
  2. Paste the printed `latchkey auth set …` command into your shell.
  3. Smoke-test:
       LATCHKEY_CURL=$(pwd)/frankweiler/backend/target/debug/latchkey-curl-shim \\
         latchkey curl -s https://chatgpt.com/backend-api/me | head -c 200
     Expect a JSON object with your account id. If you still see a
     Cloudflare challenge, also paste `Cookie: cf_clearance=…` into the
     same `latchkey auth set chatgpt` call.

See frankweiler/backend/etl/providers/chatgpt/EXTRACT.md for details."
            .into(),
        "claude_api" => "\
anthropic sessionKey expired or missing.

  1. Open https://claude.ai logged in. In DevTools → Application →
     Cookies → claude.ai, copy the `sessionKey` value.
  2. Run:
       latchkey auth set claude-ai -H 'Cookie: sessionKey=sk-ant-…'
  3. Smoke-test:
       LATCHKEY_CURL=$(pwd)/frankweiler/backend/target/debug/latchkey-curl-shim \\
         latchkey curl -s https://claude.ai/api/organizations | head -c 200

See frankweiler/backend/etl/providers/anthropic/EXTRACT.md for details."
            .into(),
        "slack_api" => "\
slack token expired or missing.

  1. Grab a user-scope OAuth token (xoxc/xoxp/xoxd).
  2. Run:
       latchkey auth set slack -H 'Authorization: Bearer xoxc-…'
  3. Smoke-test:
       latchkey curl -s https://slack.com/api/auth.test | head -c 200

See frankweiler/backend/etl/providers/slack/EXTRACT.md for details."
            .into(),
        "github_api" => "\
github PAT expired or missing.

  1. Create a fine-grained PAT at https://github.com/settings/tokens
     with `repo` + `read:user` scopes.
  2. Run:
       latchkey auth set github -H 'Authorization: Bearer github_pat_…'
  3. Smoke-test:
       latchkey curl -s https://api.github.com/user | head -c 200

See frankweiler/backend/etl/providers/github/EXTRACT.md for details."
            .into(),
        "gitlab_api" => "\
gitlab token expired or missing.

  1. Create a personal token at https://gitlab.com/-/profile/personal_access_tokens
     with `read_api` scope.
  2. Run:
       latchkey auth set gitlab -H 'Authorization: Bearer glpat-…'
  3. Smoke-test:
       latchkey curl -s https://gitlab.com/api/v4/user | head -c 200

See frankweiler/backend/etl/providers/gitlab/EXTRACT.md for details."
            .into(),
        "notion_api" => "\
notion integration token expired or missing.

  1. Create an internal integration at https://www.notion.so/profile/integrations
     and copy the secret.
  2. Run:
       latchkey auth set notion -H 'Authorization: Bearer secret_…'
  3. Smoke-test:
       latchkey curl -s -X POST https://api.notion.com/v1/search \\
         -H 'Notion-Version: 2022-06-28' -H 'Content-Type: application/json' \\
         -d '{}' | head -c 200

See frankweiler/backend/etl/providers/notion/EXTRACT.md for details."
            .into(),
        _ => GENERIC_AUTH_HINT.into(),
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();
    let _ = args.deterministic;

    let cfg = load_config(args.config.as_deref()).context("load config")?;
    eprintln!(
        "[frankweiler-sync] config: data_root={}, {} source(s)",
        cfg.data_root.display(),
        cfg.sources.len()
    );

    if let Some(playback_out) = &args.synthesize_playback_root {
        return run_synthesize(&cfg, playback_out);
    }

    fs::create_dir_all(&cfg.data_root)
        .with_context(|| format!("create data_root: {}", cfg.data_root.display()))?;
    let root = cfg.data_root.canonicalize()?;
    fs::create_dir_all(root.join("rendered_md"))?;
    eprintln!("[frankweiler-sync] data_root = {}", root.display());

    if args.skip_extract {
        eprintln!("[frankweiler-sync] extract: skipped (--skip-extract); using staged input_paths");
    } else if let Some(playback_root) = args.playback_root.as_ref() {
        let pb = playback_root
            .canonicalize()
            .with_context(|| format!("playback root: {}", playback_root.display()))?;
        std::env::set_var(PLAYBACK_ENV, &pb);
        eprintln!("[frankweiler-sync] playback root = {}", pb.display());
        run_extract_phase(&cfg, Some(&pb), &args.now).await?;
    } else {
        eprintln!("[frankweiler-sync] extract: live (hitting provider APIs)");
        run_extract_phase(&cfg, None, &args.now).await?;
    }

    for src in cfg.enabled_sources() {
        translate_source(src, &cfg, &root)?;
    }

    let dolt_db_dir = cfg.dolt_db_path();
    fs::create_dir_all(&dolt_db_dir)?;
    let port = free_port()?;
    let mut dolt_cfg = cfg.dolt.clone();
    dolt_cfg.port = port;
    eprintln!("[frankweiler-sync] dolt sql-server port = {port}");
    let server = DoltServer::ensure(&dolt_db_dir, &dolt_cfg).context("dolt sql-server")?;
    let url = server.mysql_url();
    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .with_context(|| format!("connect dolt at {url}"))?;
    init_schema(&pool).await?;
    let summary = load_all(&pool, &root, |_| {}, Some(&args.now))
        .await
        .context("load_all")?;
    eprintln!(
        "[frankweiler-sync] loaded documents={}/{} rows={}",
        summary.documents_loaded, summary.documents_total, summary.rows_inserted
    );
    drop(pool);
    drop(server);

    eprintln!("[frankweiler-sync] wrote {}/", dolt_db_dir.display());
    eprintln!(
        "[frankweiler-sync] wrote {}/",
        root.join("rendered_md").display()
    );

    if !cfg.qmd.skip {
        build_qmd_index(&root, cfg.qmd.models_dir.as_deref())?;
        eprintln!(
            "[frankweiler-sync] wrote {}",
            root.join("qmd/index.sqlite").display()
        );
    } else {
        eprintln!("[frankweiler-sync] qmd index: skipped (qmd.skip=true)");
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// Extract phase
// ─────────────────────────────────────────────────────────────────────

/// Drive every managed source's `extract::fetch` against the playback
/// tree. Each source writes into its resolved `input_path`. Runs
/// concurrently when `cfg.sync.parallel`.
async fn run_extract_phase(cfg: &Config, playback_root: Option<&Path>, now: &str) -> Result<()> {
    let mut plans: Vec<ExtractPlan> = cfg
        .enabled_sources()
        .filter_map(|s| ExtractPlan::for_source(s, cfg, playback_root, now))
        .collect::<Result<Vec<_>>>()?;

    for p in &plans {
        fs::create_dir_all(&p.out_dir)
            .with_context(|| format!("create {}", p.out_dir.display()))?;
    }

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
        let mut set: JoinSet<Result<()>> = JoinSet::new();
        for plan in plans {
            let name = plan.name.clone();
            let type_str = plan.type_str;
            set.spawn(async move {
                eprintln!("[extract] start {name} ({type_str})");
                plan.run()
                    .await
                    .with_context(|| format!("extract {name} (type={type_str})"))?;
                eprintln!("[extract] done  {name}");
                Ok(())
            });
        }
        while let Some(joined) = set.join_next().await {
            joined.context("extract task panicked")??;
        }
    } else {
        for plan in plans {
            let name = plan.name.clone();
            let type_str = plan.type_str;
            eprintln!("[extract] {name} ({type_str})");
            plan.run()
                .await
                .with_context(|| format!("extract {name} (type={type_str})"))?;
        }
    }
    Ok(())
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

    async fn run(self) -> Result<()> {
        let progress = self.progress.clone();
        let result = match self.kind {
            ExtractKind::Anthropic { sync } => {
                frankweiler_etl_anthropic::extract::fetch(
                    frankweiler_etl_anthropic::extract::FetchOptions {
                        out_dir: self.out_dir.clone(),
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
                .map(|_| ())
            }
            ExtractKind::Chatgpt { sync } => frankweiler_etl_chatgpt::extract::fetch(
                frankweiler_etl_chatgpt::extract::FetchOptions {
                    out_dir: self.out_dir.clone(),
                    max_pages: sync.max_pages.map(|v| v as usize),
                    limit: sync.limit.map(|v| v as usize),
                    sleep_between: Duration::ZERO,
                    conv_uuids: sync.conv_uuids.clone(),
                    fetched_at: Some(self.now.clone()),
                    progress: progress.clone(),
                },
            )
            .await
            .map(|_| ()),
            ExtractKind::Slack { sync } => frankweiler_etl_slack::extract::fetch(
                frankweiler_etl_slack::extract::FetchOptions {
                    out_dir: self.out_dir.clone(),
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
            .map(|_| ()),
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
            .map(|_| ()),
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
            .map(|_| ()),
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
                        out_dir: self.out_dir.clone(),
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
                .map(|_| ())
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
            render_all(&parsed, root)
                .context("anthropic render_all")
                .map(|_| ())
        }
        SourceConfig::ChatgptApi { .. } => {
            use frankweiler_etl_chatgpt::translate::{parse::parse_api_dir, render::render_all};
            let parsed = parse_api_dir(&fixture)
                .with_context(|| format!("chatgpt parse {}", fixture.display()))?;
            render_all(&parsed, root)
                .context("chatgpt render_all")
                .map(|_| ())
        }
        SourceConfig::SlackApi { .. } => {
            use frankweiler_etl_slack::translate::{render::render_all, translate_raw_dir};
            let t = translate_raw_dir(&fixture)
                .with_context(|| format!("slack translate_raw_dir {}", fixture.display()))?;
            render_all(&t, root, |_| {})
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
