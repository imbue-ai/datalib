//! `frankweiler-sync` — incremental ETL orchestrator.
//!
//! Drives Translate → Load → Dump → Archive across every configured
//! provider. Today this is the same fixtures-mode pipeline the old
//! `frankweiler-build-ingested` ran for the Bazel genrule: walk
//! pre-staged event-store JSONL, render markdown + sidecars, load into
//! an ephemeral Dolt sql-server, dump SQL, and emit the rendered tree
//! and QMD index as plain files.
//!
//! The CLI is designed to grow into a real sync runner without breaking
//! the genrule contract:
//!
//!   * `--playback-root` routes the extract phase through
//!     `frankweiler_etl::http` playback fixtures: every provider's
//!     `extract::fetch` runs against the synthesized tree into a
//!     workspace `extracted/` dir, then Translate reads from there.
//!   * `--deterministic` is the genrule's mode: fixed timestamps,
//!     hermetic tar, sorted dump. Already the default behaviour here;
//!     the flag exists so callers (Bazel) can assert intent.
//!
//! Outputs in `--out`:
//!   * `dump.sql`         — `dolt dump --result-format sql` of every table.
//!   * `rendered_md/`     — the rendered conversation markdown tree.
//!   * `qmd_index.sqlite` — BM25 + embedding index built by `qmd_indexer`
//!     over the markdown tree (skipped with `--skip-qmd-index`).
//!
//! Tar packaging of `rendered_md/` and `qmd_index.sqlite` for Bazel
//! distribution is a concern of the calling genrule, not of this binary —
//! see `tests/fixtures/BUILD.bazel`.

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use frankweiler_core::config::DoltConfig;
use frankweiler_core::dolt_server::DoltServer;
use frankweiler_etl::http::{HttpResponse, PLAYBACK_ENV};
use frankweiler_etl::load::{init_schema, load_all};
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_anthropic::synthesize::AnthropicSynth;
use frankweiler_etl_chatgpt::synthesize::ChatgptSynth;
use frankweiler_etl_github::synthesize::GithubSynth;
use frankweiler_etl_gitlab::synthesize::GitlabSynth;
use frankweiler_etl_notion::synthesize::NotionSynth;
use frankweiler_etl_slack::synthesize::SlackSynth;
use sqlx::mysql::MySqlPoolOptions;
use tempfile::TempDir;

#[derive(Debug, Parser)]
#[command(
    name = "frankweiler-sync",
    about = "Incremental ETL: translate fixtures, load into Dolt, emit dump.sql + rendered_md/ + qmd_index.sqlite"
)]
struct Args {
    /// Directory holding the shared (github/gitlab/notion) fixture trees.
    #[arg(long)]
    shared_fixtures: PathBuf,

    /// Output directory; receives `dump.sql`, the `rendered_md/` tree,
    /// and (unless `--skip-qmd-index`) `qmd_index.sqlite`.
    #[arg(long)]
    out: PathBuf,

    /// Fixed timestamp threaded through downstream renderers when they
    /// need a "now"; required for deterministic builds. Format is
    /// ISO-8601 / RFC-3339.
    #[arg(long)]
    now: String,

    /// Parent of `slack_api/` event-store JSONL.
    #[arg(long)]
    slack_fixtures: PathBuf,

    /// Parent of `chatgpt_api/` event-store JSONL.
    #[arg(long)]
    chatgpt_fixtures: PathBuf,

    /// Parent of `anthropic_api/` event-store JSONL.
    #[arg(long)]
    anthropic_fixtures: PathBuf,

    /// Run in deterministic mode: fixed `--now`, sorted dump, hermetic
    /// tar. Today this is the only supported mode; the flag exists for
    /// forward-compat and to let Bazel assert intent.
    #[arg(long, default_value_t = true)]
    deterministic: bool,

    /// HTTP playback fixture root. When set, runs each provider's
    /// `extract::fetch` against this tree (via `FRANKWEILER_HTTP_PLAYBACK`)
    /// into a workspace `extracted/` directory, and the Translate phase
    /// reads from there instead of from the `--*-fixtures` event-stores.
    /// The `--*-fixtures` flags remain required (anthropic uses
    /// `anthropic_api/users.json` from its tree as `export_dir` so the
    /// account UUID flows through normalization), but their event-store
    /// contents are otherwise unused in this mode.
    #[arg(long)]
    playback_root: Option<PathBuf>,

    /// Run every provider's HTTP fixture **synthesizer** against the
    /// input fixture trees and write playback responses into this dir.
    /// Independent of the regular Translate/Load pipeline — when this
    /// flag is set, the binary writes fixtures and exits without
    /// touching Dolt or producing dump.sql / rendered_md/.
    #[arg(long)]
    synthesize_playback_root: Option<PathBuf>,

    /// Skip the QMD index build. Useful for CI environments without
    /// Node.js or when iterating on the ETL pipeline.
    #[arg(long, default_value_t = false)]
    skip_qmd_index: bool,

    /// Where `qmd` should cache its ~300MB embedding model. Defaults to
    /// `~/.cache/qmd-models`. Symlinked into the workspace so the model
    /// blob stays outside the data root.
    #[arg(long)]
    qmd_models_dir: Option<PathBuf>,
}

fn free_port() -> Result<u16> {
    let l = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(l.local_addr()?.port())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _ = args.deterministic;

    if let Some(playback_out) = &args.synthesize_playback_root {
        return run_synthesize(&args, playback_out);
    }

    let shared = args
        .shared_fixtures
        .canonicalize()
        .with_context(|| format!("shared fixtures: {}", args.shared_fixtures.display()))?;
    let slack = args
        .slack_fixtures
        .canonicalize()
        .with_context(|| format!("slack fixtures: {}", args.slack_fixtures.display()))?;
    let chatgpt = args
        .chatgpt_fixtures
        .canonicalize()
        .with_context(|| format!("chatgpt fixtures: {}", args.chatgpt_fixtures.display()))?;
    let anthropic = args
        .anthropic_fixtures
        .canonicalize()
        .with_context(|| format!("anthropic fixtures: {}", args.anthropic_fixtures.display()))?;
    fs::create_dir_all(&args.out).with_context(|| format!("create out: {}", args.out.display()))?;
    let out = args.out.canonicalize()?;

    let workspace = TempDir::new().context("create scratch workspace")?;
    let root = workspace.path().to_path_buf();
    let rendered_md = root.join("rendered_md");
    fs::create_dir_all(&rendered_md)?;

    eprintln!("[frankweiler-sync] root = {}", root.display());

    let extract_inputs = if let Some(playback_root) = args.playback_root.as_ref() {
        let pb = playback_root
            .canonicalize()
            .with_context(|| format!("playback root: {}", playback_root.display()))?;
        run_extract_phase(&pb, &root, &anthropic).await?
    } else {
        ExtractInputs {
            anthropic_api: anthropic.join("anthropic_api"),
            chatgpt_api: chatgpt.join("chatgpt_api"),
            slack_api: slack.join("slack_api"),
            github_api: shared.join("github_api"),
            gitlab_api: shared.join("gitlab_api"),
            notion_web: shared.join("notion_web"),
        }
    };

    translate_anthropic(&extract_inputs.anthropic_api, &root)?;
    translate_chatgpt(&extract_inputs.chatgpt_api, &root)?;
    translate_slack(&extract_inputs.slack_api, &root)?;
    translate_github(&extract_inputs.github_api, &root)?;
    translate_gitlab(&extract_inputs.gitlab_api, &root)?;
    translate_notion(&extract_inputs.notion_web, &root)?;

    let dolt_repo_dir = root.join("dolt_repo");
    fs::create_dir_all(&dolt_repo_dir)?;
    let port = free_port()?;
    let dolt_cfg = DoltConfig {
        host: "127.0.0.1".to_string(),
        port,
        user: "root".to_string(),
        repo_dirname: "dolt_repo".to_string(),
        binary: None,
    };
    eprintln!("[frankweiler-sync] dolt sql-server port = {port}");
    let server = DoltServer::ensure(&dolt_repo_dir, &dolt_cfg).context("dolt sql-server")?;
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

    let dump_sql = out.join("dump.sql");
    dolt_dump(&dolt_repo_dir, &dump_sql)?;

    drop(server);

    let rendered_dest = out.join("rendered_md");
    copy_tree(&root.join("rendered_md"), &rendered_dest)?;

    eprintln!("[frankweiler-sync] wrote {}", dump_sql.display());
    eprintln!("[frankweiler-sync] wrote {}/", rendered_dest.display());

    if !args.skip_qmd_index {
        let qmd_index_out = out.join("qmd_index.sqlite");
        build_qmd_index(&root, args.qmd_models_dir.as_deref(), &qmd_index_out)?;
        eprintln!("[frankweiler-sync] wrote {}", qmd_index_out.display());
    } else {
        eprintln!("[frankweiler-sync] qmd index: skipped (--skip-qmd-index)");
    }
    Ok(())
}

/// Mirror `src` into `dest` as a plain directory tree. We don't tar at
/// this layer — the genrule that wraps `frankweiler-sync` is responsible
/// for hermetic archive packaging.
fn copy_tree(src: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        fs::remove_dir_all(dest).with_context(|| format!("clear existing {}", dest.display()))?;
    }
    if !src.exists() {
        fs::create_dir_all(dest)?;
        return Ok(());
    }
    for entry in walkdir::WalkDir::new(src).sort_by_file_name() {
        let entry = entry?;
        let rel = entry
            .path()
            .strip_prefix(src)
            .with_context(|| format!("strip_prefix {}", entry.path().display()))?;
        let target = dest.join(rel);
        let ft = entry.file_type();
        if ft.is_dir() {
            fs::create_dir_all(&target).with_context(|| format!("mkdir {}", target.display()))?;
        } else if ft.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &target).with_context(|| {
                format!("copy {} -> {}", entry.path().display(), target.display())
            })?;
        }
    }
    Ok(())
}

/// Build the QMD index over the workspace's `rendered_md/` tree and copy
/// the resulting sqlite db to `dest`. Runs `qmd` via `npx` under the hood;
/// requires Node.js to be available on PATH (or `NPX_BIN` set).
fn build_qmd_index(root: &Path, models_dir: Option<&Path>, dest: &Path) -> Result<()> {
    let mut opts = frankweiler_qmd_indexer::IndexOptions::new(root);
    if let Some(d) = models_dir {
        opts.models_dir = d.to_path_buf();
    }
    let index_path = frankweiler_qmd_indexer::run_index(&opts).context("qmd index build")?;
    fs::copy(&index_path, dest).with_context(|| {
        format!(
            "copy qmd index {} -> {}",
            index_path.display(),
            dest.display()
        )
    })?;
    Ok(())
}

/// Per-provider extract output directories — the path each `translate_*`
/// step reads from. In playback mode these live under the scratch
/// workspace; otherwise they point at the user-supplied `--*-fixtures`
/// event-store trees.
struct ExtractInputs {
    anthropic_api: PathBuf,
    chatgpt_api: PathBuf,
    slack_api: PathBuf,
    github_api: PathBuf,
    gitlab_api: PathBuf,
    notion_web: PathBuf,
}

/// Drive each provider's `extract::fetch` against a playback fixture
/// tree, writing event-store JSONL (or snapshots) into the workspace.
///
/// Sets the process-wide `FRANKWEILER_HTTP_PLAYBACK` env var so the
/// shared HTTP transport returns synthesized responses instead of
/// hitting the network. The anthropic step needs `users.json` to recover
/// the account UUID for normalization; we pass the user-supplied
/// `--anthropic-fixtures` tree as `export_dir` to satisfy that.
///
/// Notion has no listing endpoint, so seeds are derived from the
/// playback fixtures themselves: every `<playback>/notion/*.json` whose
/// response body parses as a Notion page becomes a `subtree_pages` seed.
async fn run_extract_phase(
    playback_root: &Path,
    workspace: &Path,
    anthropic_fixtures: &Path,
) -> Result<ExtractInputs> {
    std::env::set_var(PLAYBACK_ENV, playback_root);
    eprintln!(
        "[frankweiler-sync] playback root = {}",
        playback_root.display()
    );

    let extracted = workspace.join("extracted");
    let inputs = ExtractInputs {
        anthropic_api: extracted.join("anthropic_api"),
        chatgpt_api: extracted.join("chatgpt_api"),
        slack_api: extracted.join("slack_api"),
        github_api: extracted.join("github_api"),
        gitlab_api: extracted.join("gitlab_api"),
        notion_web: extracted.join("notion_web"),
    };
    for d in [
        &inputs.anthropic_api,
        &inputs.chatgpt_api,
        &inputs.slack_api,
        &inputs.github_api,
        &inputs.gitlab_api,
        &inputs.notion_web,
    ] {
        fs::create_dir_all(d).with_context(|| format!("create {}", d.display()))?;
    }

    eprintln!("[frankweiler-sync] extract: anthropic");
    frankweiler_etl_anthropic::extract::fetch(frankweiler_etl_anthropic::extract::FetchOptions {
        out_dir: inputs.anthropic_api.clone(),
        export_dir: Some(anthropic_fixtures.join("anthropic_api")),
        // Force every export conversation to be refetched from playback
        // so the merged output carries the synthesized detail body
        // (with chat_messages) rather than just the export skeleton.
        overlap: usize::MAX,
        sleep_between: Duration::ZERO,
        conv_uuid: None,
        ..Default::default()
    })
    .await
    .context("anthropic extract")?;

    eprintln!("[frankweiler-sync] extract: chatgpt");
    frankweiler_etl_chatgpt::extract::fetch(frankweiler_etl_chatgpt::extract::FetchOptions {
        out_dir: inputs.chatgpt_api.clone(),
        max_pages: None,
        limit: None,
        sleep_between: Duration::ZERO,
        conv_uuid: None,
        ..Default::default()
    })
    .await
    .context("chatgpt extract")?;

    eprintln!("[frankweiler-sync] extract: slack");
    frankweiler_etl_slack::extract::fetch(frankweiler_etl_slack::extract::FetchOptions {
        out_dir: inputs.slack_api.clone(),
        channels: None,
        since: frankweiler_etl_slack::extract::DEFAULT_SINCE.to_string(),
        refresh_window_days: 0,
        members_only: false,
        media: false,
        ..Default::default()
    })
    .await
    .context("slack extract")?;

    eprintln!("[frankweiler-sync] extract: github");
    frankweiler_etl_github::extract::fetch(frankweiler_etl_github::extract::FetchOptions {
        out_dir: inputs.github_api.clone(),
        full_sync: true,
        refresh_window_days: 0,
        sleep_between: Duration::ZERO,
        ..frankweiler_etl_github::extract::FetchOptions::default()
    })
    .await
    .context("github extract")?;

    eprintln!("[frankweiler-sync] extract: gitlab");
    frankweiler_etl_gitlab::extract::fetch(frankweiler_etl_gitlab::extract::FetchOptions {
        out_dir: inputs.gitlab_api.clone(),
        full_sync: true,
        refresh_window_days: 0,
        sleep_between: Duration::ZERO,
        ..frankweiler_etl_gitlab::extract::FetchOptions::default()
    })
    .await
    .context("gitlab extract")?;

    let notion_seeds = derive_notion_seeds(&playback_root.join("notion"))
        .context("derive notion seeds from playback fixtures")?;
    eprintln!(
        "[frankweiler-sync] extract: notion ({} seed page(s))",
        notion_seeds.len()
    );
    frankweiler_etl_notion::extract::fetch(frankweiler_etl_notion::extract::FetchOptions {
        out_dir: inputs.notion_web.clone(),
        subtree_pages: notion_seeds,
        sleep_between: Duration::ZERO,
        ..frankweiler_etl_notion::extract::FetchOptions::default()
    })
    .await
    .context("notion extract")?;

    Ok(inputs)
}

/// Walk `<playback>/notion/*.json`, decode each as an `HttpResponse`,
/// parse the body as JSON, and collect every `id` whose `object` field
/// is `"page"`. That set of UUIDs is what notion's extract walks via
/// BFS — its `visited` HashSet handles dedup so feeding all known
/// pages (not just roots) is harmless.
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

fn translate_anthropic(fixture: &Path, root: &Path) -> Result<()> {
    use frankweiler_etl_anthropic::translate::{parse::parse_export, render::render_all};
    eprintln!("[frankweiler-sync] anthropic: {}", fixture.display());
    let parsed =
        parse_export(fixture).with_context(|| format!("anthropic parse {}", fixture.display()))?;
    render_all(&parsed, root).context("anthropic render_all")?;
    Ok(())
}

fn translate_chatgpt(fixture: &Path, root: &Path) -> Result<()> {
    use frankweiler_etl_chatgpt::translate::{parse::parse_api_dir, render::render_all};
    eprintln!("[frankweiler-sync] chatgpt: {}", fixture.display());
    let parsed =
        parse_api_dir(fixture).with_context(|| format!("chatgpt parse {}", fixture.display()))?;
    render_all(&parsed, root).context("chatgpt render_all")?;
    Ok(())
}

fn translate_slack(fixture: &Path, root: &Path) -> Result<()> {
    use frankweiler_etl_slack::translate::{render::render_all, translate_raw_dir};
    eprintln!("[frankweiler-sync] slack: {}", fixture.display());
    let t = translate_raw_dir(fixture)
        .with_context(|| format!("slack translate_raw_dir {}", fixture.display()))?;
    render_all(&t, root, |_| {}).context("slack render_all")?;
    Ok(())
}

fn translate_github(fixture: &Path, root: &Path) -> Result<()> {
    use frankweiler_etl_github::translate::{parse_api_dir, render_github};
    eprintln!("[frankweiler-sync] github: {}", fixture.display());
    let parsed =
        parse_api_dir(fixture).with_context(|| format!("github parse {}", fixture.display()))?;
    render_github(&parsed, root).context("render_github")?;
    Ok(())
}

fn translate_gitlab(fixture: &Path, root: &Path) -> Result<()> {
    use frankweiler_etl_gitlab::translate::{parse_api_dir, render_gitlab};
    eprintln!("[frankweiler-sync] gitlab: {}", fixture.display());
    let parsed =
        parse_api_dir(fixture).with_context(|| format!("gitlab parse {}", fixture.display()))?;
    render_gitlab(&parsed, root).context("render_gitlab")?;
    Ok(())
}

fn run_synthesize(args: &Args, out: &Path) -> Result<()> {
    fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    let synths: Vec<Box<dyn Synthesizer>> = vec![
        Box::new(AnthropicSynth::new(
            args.anthropic_fixtures.join("anthropic_api"),
        )),
        Box::new(ChatgptSynth::new(args.chatgpt_fixtures.join("chatgpt_api"))),
        Box::new(SlackSynth::new(args.slack_fixtures.join("slack_api"))),
        Box::new(GithubSynth::new(args.shared_fixtures.join("github_api"))),
        Box::new(GitlabSynth::new(args.shared_fixtures.join("gitlab_api"))),
        Box::new(NotionSynth::new(args.shared_fixtures.join("notion_web"))),
    ];
    for s in &synths {
        let report = s
            .synthesize(out)
            .with_context(|| format!("synthesize {}", s.name()))?;
        eprintln!(
            "[frankweiler-sync] synthesize {}: {} fixtures",
            s.name(),
            report.fixtures_written
        );
    }
    Ok(())
}

fn translate_notion(fixture: &Path, root: &Path) -> Result<()> {
    use frankweiler_etl_notion::translate::{parse_api_dir, render::render_notion_official};
    eprintln!("[frankweiler-sync] notion: {}", fixture.display());
    let parsed =
        parse_api_dir(fixture).with_context(|| format!("notion parse {}", fixture.display()))?;
    render_notion_official(&parsed, root).context("render_notion_official")?;
    Ok(())
}

fn dolt_dump(repo_dir: &Path, dump_sql: &Path) -> Result<()> {
    eprintln!("[frankweiler-sync] dolt dump -> {}", dump_sql.display());
    let dolt = frankweiler_core::dolt_server::resolve_dolt_binary(None)
        .context("resolve dolt binary for dump")?;
    let status = Command::new(&dolt)
        .arg("dump")
        .arg("--result-format")
        .arg("sql")
        .arg("--no-batch")
        .arg("--file-name")
        .arg(dump_sql)
        .current_dir(repo_dir)
        .status()
        .context("spawn dolt dump")?;
    if !status.success() {
        bail!("dolt dump failed: {status}");
    }
    Ok(())
}
