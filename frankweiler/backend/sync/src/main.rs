//! `frankweiler-sync` — incremental ETL orchestrator.
//!
//! Drives Translate → Load → Dump → Archive across every configured
//! provider. Today this is the same fixtures-mode pipeline the old
//! `frankweiler-build-ingested` ran for the Bazel genrule: walk
//! pre-staged event-store JSONL, render markdown + sidecars, load into
//! an ephemeral Dolt sql-server, dump SQL, tar the rendered tree.
//!
//! The CLI is designed to grow into a real sync runner without breaking
//! the genrule contract:
//!
//!   * `--playback-root` will route the extract phase through
//!     `frankweiler_etl::http` playback fixtures (currently rejected —
//!     synthesizers not landed yet).
//!   * `--deterministic` is the genrule's mode: fixed timestamps,
//!     hermetic tar, sorted dump. Already the default behaviour here;
//!     the flag exists so callers (Bazel) can assert intent.
//!
//! Outputs in `--out`:
//!   * `dump.sql` — `dolt dump --result-format sql` of every table.
//!   * `qmd.tar` — hermetic tar of `rendered_md/` (mtime/uid/gid zeroed).

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Parser;
use frankweiler_core::config::DoltConfig;
use frankweiler_core::dolt_server::DoltServer;
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
    about = "Incremental ETL: translate fixtures, load into Dolt, dump SQL + tar markdown"
)]
struct Args {
    /// Directory holding the shared (github/gitlab/notion) fixture trees.
    #[arg(long)]
    shared_fixtures: PathBuf,

    /// Output directory; receives `dump.sql` and `qmd.tar`.
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

    /// HTTP playback fixture root. When set, the extract phase will
    /// route every `latchkey_curl` call through this tree instead of
    /// hitting the network. Today there is no extract phase wired in
    /// (we drive Translate directly off pre-staged event-store JSONL),
    /// so the flag is accepted and stored for the live runner but
    /// otherwise unused. The synthesizers in `--synthesize-playback-root`
    /// are what *write* this tree.
    #[arg(long)]
    playback_root: Option<PathBuf>,

    /// Run every provider's HTTP fixture **synthesizer** against the
    /// input fixture trees and write playback responses into this dir.
    /// Independent of the regular Translate/Load pipeline — when this
    /// flag is set, the binary writes fixtures and exits without
    /// touching Dolt or producing `dump.sql` / `qmd.tar`.
    #[arg(long)]
    synthesize_playback_root: Option<PathBuf>,
}

fn free_port() -> Result<u16> {
    let l = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(l.local_addr()?.port())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _ = args.deterministic;
    let _ = &args.playback_root;

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

    translate_anthropic(&anthropic.join("anthropic_api"), &root)?;
    translate_chatgpt(&chatgpt.join("chatgpt_api"), &root)?;
    translate_slack(&slack.join("slack_api"), &root)?;
    translate_github(&shared.join("github_api"), &root)?;
    translate_gitlab(&shared.join("gitlab_api"), &root)?;
    translate_notion(&shared.join("notion_web"), &root)?;

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

    let qmd_tar = out.join("qmd.tar");
    tar_rendered_md(&root, &qmd_tar)?;

    eprintln!("[frankweiler-sync] wrote {}", dump_sql.display());
    eprintln!("[frankweiler-sync] wrote {}", qmd_tar.display());
    Ok(())
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

fn tar_rendered_md(root: &Path, dest: &Path) -> Result<()> {
    let rendered = root.join("rendered_md");
    let file = fs::File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    let mut tar = tar::Builder::new(file);
    if !rendered.is_dir() {
        tar.finish()?;
        return Ok(());
    }
    let mut entries: Vec<PathBuf> = walkdir::WalkDir::new(&rendered)
        .into_iter()
        .filter_map(|e| e.ok())
        .map(|e| e.into_path())
        .collect();
    entries.sort();
    for p in entries {
        let rel = p
            .strip_prefix(root)
            .with_context(|| format!("strip_prefix {}", p.display()))?;
        let arcname = format!("qmd/{}", rel.to_string_lossy());
        let meta = fs::symlink_metadata(&p)?;
        if meta.file_type().is_dir() {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            header.set_cksum();
            tar.append_data(&mut header, arcname, std::io::empty())?;
        } else if meta.file_type().is_file() {
            let data = fs::read(&p)?;
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(data.len() as u64);
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            header.set_cksum();
            tar.append_data(&mut header, arcname, data.as_slice())?;
        }
    }
    tar.finish()?;
    Ok(())
}
