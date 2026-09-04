//! `datalib-dirtree-diff` — diff two fsindex scans into one HTML page.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use datalib_dirtree_diff::model::{Inputs, SideInput};
use datalib_dirtree_diff::store::{self, SideSpec};
use datalib_dirtree_diff::{analyze, parse_size, render};
use datalib_obs::{init as init_obs, ObsArgs};

#[derive(Parser, Debug)]
#[command(
    name = "datalib-dirtree-diff",
    about = "Diff two fsindex directory scans into one self-contained HTML page, \
             with moves detected via directory tree-hashes."
)]
struct Args {
    /// Left scan as PATH[#REF]; REF is a branch, HEAD~N, or a commit
    /// hash. Defaults to HEAD.
    #[arg(long)]
    left: SideSpec,

    /// Right scan as PATH[#REF].
    #[arg(long)]
    right: SideSpec,

    /// Where to write the page.
    #[arg(short, long, default_value = "dirtree_diff.html")]
    out: PathBuf,

    /// Also write the intermediate representation as JSON — the same
    /// structure the page is rendered from, and readable back into a
    /// `DiffResult`.
    #[arg(long)]
    json: Option<PathBuf>,

    /// Render every entry, including unchanged ones. Costs a full scan
    /// of both corpora; without it the page holds only changed paths
    /// plus their ancestor directories, derived from the diff alone.
    #[arg(long)]
    full_tree: bool,

    /// Report content duplicated WITHIN each tree, for entries at or
    /// above this size (4096, 64K, 1M, 2G). A directory counts as one
    /// entry, so a folder copied inside the same tree is a single
    /// finding. One full scan per side; 0 turns it off.
    #[arg(long, default_value = "1M", value_parser = parse_size)]
    dup_threshold: i64,

    /// Skip the corpus scan that separates "deleted outright" from
    /// "deleted here, identical bytes still elsewhere".
    #[arg(long)]
    no_copy_detection: bool,

    /// Keep the unified scratch database for inspection.
    #[arg(long)]
    keep_scratch: bool,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "dirtree-diff")?;

    for spec in [&args.left, &args.right] {
        if !spec.db.exists() {
            anyhow::bail!("no such database: {}", spec.db.display());
        }
    }

    // Resolve each ref inside its own file — the only database where a
    // ref name means anything — then read only through those pins.
    let left_pool = store::open(&args.left.db).await?;
    let left_commit = store::resolve_ref(&left_pool, &args.left.reference).await?;
    left_pool.close().await;

    let right_pool = store::open(&args.right.db).await?;
    let right_commit = store::resolve_ref(&right_pool, &args.right.reference).await?;
    right_pool.close().await;

    let same_file = std::fs::canonicalize(&args.left.db)? == std::fs::canonicalize(&args.right.db)?;
    let scratch = tempfile::tempdir().context("scratch dir")?;

    let pool = if same_file {
        // Both refs already share a chunk store; nothing to unify.
        store::open(&args.left.db).await?
    } else {
        let unified = scratch.path().join("unified.doltlite_db");
        store::unify(&unified, &args.left.db, &args.right.db).await?;
        // A fresh connection, deliberately: the one that did the
        // fetching cannot see the tables it fetched. See `store::unify`.
        store::open(&unified).await?
    };

    let diff = store::fetch_diff(&pool, &left_commit, &right_commit).await?;

    let (mut copies_right, mut copies_left) = (Default::default(), Default::default());
    if !args.no_copy_detection {
        // Only digests the move pairing could not already account for
        // need the corpus scan.
        let (_, residual_removed, residual_added) =
            datalib_dirtree_diff::analyze::pair_moves(&diff);
        let want_right: BTreeSet<String> =
            residual_removed.iter().map(|e| e.digest.clone()).collect();
        let want_left: BTreeSet<String> = residual_added.iter().map(|e| e.digest.clone()).collect();
        copies_right = store::lookup_digests(&pool, &right_commit, &want_right).await?;
        copies_left = store::lookup_digests(&pool, &left_commit, &want_left).await?;
    }

    let inputs = Inputs {
        left: SideInput {
            db: args.left.db.canonicalize()?.display().to_string(),
            reference: args.left.reference.clone(),
            commit: left_commit.to_string(),
            full: if args.full_tree {
                Some(store::load_side(&pool, &left_commit).await?)
            } else {
                None
            },
            dup_candidates: store::duplicate_candidates(&pool, &left_commit, args.dup_threshold)
                .await?,
        },
        right: SideInput {
            db: args.right.db.canonicalize()?.display().to_string(),
            reference: args.right.reference.clone(),
            commit: right_commit.to_string(),
            full: if args.full_tree {
                Some(store::load_side(&pool, &right_commit).await?)
            } else {
                None
            },
            dup_candidates: store::duplicate_candidates(&pool, &right_commit, args.dup_threshold)
                .await?,
        },
        diff,
        copies_right,
        copies_left,
        dup_threshold: args.dup_threshold,
        copy_detection: !args.no_copy_detection,
        unified: !same_file,
    };
    pool.close().await;

    // Everything from here is pure.
    let result = analyze(&inputs);

    std::fs::write(&args.out, render::render(&result)?)
        .with_context(|| format!("write {}", args.out.display()))?;
    if let Some(path) = &args.json {
        std::fs::write(path, serde_json::to_string_pretty(&result)?)
            .with_context(|| format!("write {}", path.display()))?;
    }

    let s = &result.summary;
    // CLI summary to stdout: this binary is a pipe-friendly tool, so a
    // one-line machine-greppable summary on stdout is intentional (the
    // corpus-scan notices above go to the stderr log sink).
    #[allow(clippy::disallowed_macros)]
    {
        println!(
            "wrote {} — {} move(s) (+{} rolled up), {} modified, {} added, {} deleted, \
             {} deleted-with-copy-remaining; duplicates within each tree: {} group(s) left \
             ({} B), {} right ({} B)",
            args.out.display(),
            s.moves,
            s.rolled_up,
            s.modified,
            s.added,
            s.removed,
            s.removed_but_copy_remains,
            result.left.dup_groups.len(),
            result.left.dup_wasted,
            result.right.dup_groups.len(),
            result.right.dup_wasted,
        );
    }

    if args.keep_scratch {
        // Deliberately leaked: the caller asked to inspect it, so the
        // path has to outlive this process.
        let kept = scratch.keep();
        tracing::info!(path = %kept.display(), "scratch database kept");
    }
    Ok(())
}
