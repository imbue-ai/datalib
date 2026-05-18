//! `frankweiler-build-ingested` — Rust port of `tests/fixtures/build_ingested.py`.
//!
//! Drives every provider's Translate step over the checked-in TNG fixtures,
//! Loads the resulting sidecars into Dolt, then emits two byte-stable
//! artifacts for the Bazel genrule cache:
//!
//!   * `dump.sql` — `dolt dump` of every table in the working set.
//!   * `qmd.tar`  — the rendered `rendered_md/` tree, with mtime/uid/gid
//!                  normalized so the tar is hermetic.
//!
//! Positional args mirror the old Python entrypoint:
//!   1: shared fixture dir holding github_api/, gitlab_api/, notion_web/
//!   2: output dir for dump.sql + qmd.tar (Bazel-supplied)
//!   3: --now value (currently unused — left here for compat with the
//!      Python signature so the genrule plumbing doesn't have to change)
//!   4: slack fixture dir (parent of slack_api/)
//!   5: chatgpt fixture dir (parent of chatgpt_api/)
//!   6: anthropic fixture dir (parent of anthropic_api/)
//!
//! The `claude_export` source (Claude.ai manual JSON export) is
//! intentionally dropped — there is no Rust port of that reader. Every
//! other source from the old Python pipeline is covered by a provider
//! crate.

use std::ffi::OsString;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use frankweiler_core::config::DoltConfig;
use frankweiler_core::dolt_server::DoltServer;
use frankweiler_etl::load::{init_schema, load_all};
use sqlx::mysql::MySqlPoolOptions;
use tempfile::TempDir;

struct Args {
    shared_fixtures: PathBuf,
    out: PathBuf,
    _now: String,
    slack_fixtures: PathBuf,
    chatgpt_fixtures: PathBuf,
    anthropic_fixtures: PathBuf,
}

fn parse_args() -> Result<Args> {
    let raw: Vec<OsString> = std::env::args_os().skip(1).collect();
    if raw.len() < 6 {
        bail!(
            "usage: frankweiler-build-ingested <shared_fixtures> <out> <now> \
             <slack_fixtures> <chatgpt_fixtures> <anthropic_fixtures>"
        );
    }
    Ok(Args {
        shared_fixtures: PathBuf::from(&raw[0]),
        out: PathBuf::from(&raw[1]),
        _now: raw[2].to_string_lossy().into_owned(),
        slack_fixtures: PathBuf::from(&raw[3]),
        chatgpt_fixtures: PathBuf::from(&raw[4]),
        anthropic_fixtures: PathBuf::from(&raw[5]),
    })
}

fn free_port() -> Result<u16> {
    let l = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(l.local_addr()?.port())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
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
    fs::create_dir_all(&args.out)
        .with_context(|| format!("create out: {}", args.out.display()))?;
    let out = args.out.canonicalize()?;

    // Hermetic working dir: a fresh tempdir each invocation, cleaned up on
    // scope exit. Only the two declared genrule outputs (`dump.sql`,
    // `qmd.tar`) get written into `out`; everything else (Dolt repo,
    // rendered markdown tree, sidecars) is throwaway state.
    let workspace = TempDir::new().context("create scratch workspace")?;
    let root = workspace.path().to_path_buf();
    let rendered_md = root.join("rendered_md");
    fs::create_dir_all(&rendered_md)?;

    eprintln!("[build-ingested] root = {}", root.display());

    // ---- Translate every provider ------------------------------------
    translate_anthropic(&anthropic.join("anthropic_api"), &root)?;
    translate_chatgpt(&chatgpt.join("chatgpt_api"), &root)?;
    translate_slack(&slack.join("slack_api"), &root)?;
    translate_github(&shared.join("github_api"), &root)?;
    translate_gitlab(&shared.join("gitlab_api"), &root)?;
    translate_notion(&shared.join("notion_web"), &root)?;

    // ---- Spawn an ephemeral Dolt sql-server and Load -----------------
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
    eprintln!("[build-ingested] dolt sql-server port = {port}");
    let server = DoltServer::ensure(&dolt_repo_dir, &dolt_cfg).context("dolt sql-server")?;
    let url = server.mysql_url();
    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .with_context(|| format!("connect dolt at {url}"))?;
    init_schema(&pool).await?;
    let summary = load_all(&pool, &root, |_| {}).await.context("load_all")?;
    eprintln!(
        "[build-ingested] loaded documents={}/{} rows={}",
        summary.documents_loaded, summary.documents_total, summary.rows_inserted
    );
    drop(pool);

    // ---- Dump SQL ----------------------------------------------------
    let dump_sql = out.join("dump.sql");
    dolt_dump(&dolt_repo_dir, &dump_sql)?;

    // Stop the sql-server we spawned so the test can re-use the repo dir
    // without contention. (DoltServer drops on scope end already, but we
    // explicitly drop here before the tar step so any flush happens first.)
    drop(server);

    // ---- Tar the rendered_md tree ------------------------------------
    let qmd_tar = out.join("qmd.tar");
    tar_rendered_md(&root, &qmd_tar)?;

    eprintln!("[build-ingested] wrote {}", dump_sql.display());
    eprintln!("[build-ingested] wrote {}", qmd_tar.display());
    Ok(())
}

fn translate_anthropic(fixture: &Path, root: &Path) -> Result<()> {
    use frankweiler_etl_anthropic::translate::{parse::parse_export, render::render_all};
    eprintln!("[build-ingested] anthropic: {}", fixture.display());
    let parsed = parse_export(fixture)
        .with_context(|| format!("anthropic parse {}", fixture.display()))?;
    render_all(&parsed, root).context("anthropic render_all")?;
    Ok(())
}

fn translate_chatgpt(fixture: &Path, root: &Path) -> Result<()> {
    use frankweiler_etl_chatgpt::translate::{parse::parse_api_dir, render::render_all};
    eprintln!("[build-ingested] chatgpt: {}", fixture.display());
    let parsed = parse_api_dir(fixture)
        .with_context(|| format!("chatgpt parse {}", fixture.display()))?;
    render_all(&parsed, root).context("chatgpt render_all")?;
    Ok(())
}

fn translate_slack(fixture: &Path, root: &Path) -> Result<()> {
    use frankweiler_etl_slack::translate::{render::render_all, translate_raw_dir};
    eprintln!("[build-ingested] slack: {}", fixture.display());
    let t = translate_raw_dir(fixture)
        .with_context(|| format!("slack translate_raw_dir {}", fixture.display()))?;
    render_all(&t, root, |_| {}).context("slack render_all")?;
    Ok(())
}

fn translate_github(fixture: &Path, root: &Path) -> Result<()> {
    use frankweiler_etl_github::translate::{parse_api_dir, render_github};
    eprintln!("[build-ingested] github: {}", fixture.display());
    let parsed = parse_api_dir(fixture)
        .with_context(|| format!("github parse {}", fixture.display()))?;
    render_github(&parsed, root).context("render_github")?;
    Ok(())
}

fn translate_gitlab(fixture: &Path, root: &Path) -> Result<()> {
    use frankweiler_etl_gitlab::translate::{parse_api_dir, render_gitlab};
    eprintln!("[build-ingested] gitlab: {}", fixture.display());
    let parsed = parse_api_dir(fixture)
        .with_context(|| format!("gitlab parse {}", fixture.display()))?;
    render_gitlab(&parsed, root).context("render_gitlab")?;
    Ok(())
}

fn translate_notion(fixture: &Path, root: &Path) -> Result<()> {
    use frankweiler_etl_notion::translate::{parse_api_dir, render::render_notion_official};
    eprintln!("[build-ingested] notion: {}", fixture.display());
    let parsed = parse_api_dir(fixture)
        .with_context(|| format!("notion parse {}", fixture.display()))?;
    render_notion_official(&parsed, root).context("render_notion_official")?;
    Ok(())
}

fn dolt_dump(repo_dir: &Path, dump_sql: &Path) -> Result<()> {
    eprintln!("[build-ingested] dolt dump -> {}", dump_sql.display());
    let status = Command::new("dolt")
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
    let file = fs::File::create(dest)
        .with_context(|| format!("create {}", dest.display()))?;
    let mut tar = tar::Builder::new(file);
    if !rendered.is_dir() {
        // No content — still emit an empty archive so the genrule output
        // is materialized.
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
