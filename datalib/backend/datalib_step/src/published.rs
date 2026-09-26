//! What a built-in step has published, as the version its outcome reports:
//! the commit its store's `main` is at. Read however the step ended, since a
//! failed or stopped step's commits stand, and a writer's open can publish
//! one its crashed predecessor left. A seal names the same commit the same
//! way, so finishing on the commit last sealed moves nothing downstream.

use std::path::Path;

use anyhow::Result;

use crate::events::OutputClaim;
use crate::function::Function;
use crate::source::StepEnv;

/// The version a step's success reports too; `None` for a step whose tree
/// holds no doltlite store.
pub async fn version(env: &StepEnv, data_root: &Path) -> Result<Option<String>> {
    let tree = data_root.join(&env.step);
    let store = match env.function {
        Function::Ingest => return crate::ingest::raw_store_version(&tree).await,
        Function::RenderMarkdown => datalib_etl_render::indexed_markdown::path_for(&tree),
        Function::GridIndex => datalib_core::layout::grid_index_db(data_root),
        Function::QmdIndex | Function::KeywordIndex | Function::Embed | Function::EmbeddingMap => {
            return Ok(None)
        }
    };
    datalib_etl::doltlite_raw::head_commit_at_path(&store).await
}

/// The claim a failed or stopped step reports. A head that cannot be read
/// is logged and left out: the step's consumers then read what it sealed.
pub async fn claims(env: &StepEnv, data_root: &Path) -> Vec<OutputClaim> {
    match version(env, data_root).await {
        Ok(Some(version)) => vec![OutputClaim {
            path: env.step.clone(),
            version,
            rows: None,
        }],
        Ok(None) => Vec::new(),
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "could not read what this step published; its consumers read what it sealed"
            );
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(step: &str, function: Function) -> StepEnv {
        StepEnv {
            step: step.to_string(),
            group: step.split('/').next().unwrap().to_string(),
            group_type: None,
            source_group: None,
            source_group_type: None,
            function,
            inputs: Vec::new(),
        }
    }

    /// A stopped or failed ingest reports the commit its entities store is
    /// at, spelled as the seal that made it was: without this, what it
    /// published reaches its render only at its next success.
    #[tokio::test]
    async fn a_step_reports_the_commit_its_store_is_at() {
        let root = tempfile::tempdir().unwrap();
        let ingest = env("mail/ingest", Function::Ingest);
        assert!(
            claims(&ingest, root.path()).await.is_empty(),
            "no store yet"
        );

        let store = datalib_etl::raw_layout::entities_db(&root.path().join("mail/ingest"));
        std::fs::create_dir_all(store.parent().unwrap()).unwrap();
        let pool = datalib_etl::doltlite_raw::open(&store, &[]).await.unwrap();
        sqlx::query("CREATE TABLE t (x INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        let sealed = datalib_etl::doltlite_raw::commit_run(&pool, "seal")
            .await
            .unwrap();
        pool.close().await;
        let claimed = claims(&ingest, root.path()).await;
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].path, "mail/ingest");
        assert_eq!(Some(claimed[0].version.clone()), sealed);

        let qmd = env("unified_index/qmd_index", Function::QmdIndex);
        assert!(
            claims(&qmd, root.path()).await.is_empty(),
            "no store of its own"
        );
    }
}
