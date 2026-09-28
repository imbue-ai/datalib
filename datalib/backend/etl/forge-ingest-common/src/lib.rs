//! What the forge providers' downloads (github, gitlab) share. A forge
//! sync reads the account it runs as, discovers every change request —
//! pull request or merge request — the person is on through a handful
//! of searches, and fetches each one with its comments. [`sync`] is that
//! run; a [`Forge`] is what one forge does differently.

pub mod client;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::download_run::DownloadRun;
use datalib_etl::progress::Progress;
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::Value;
use sqlx::SqlitePool;

pub use client::{ForgeClient, ForgeError, LATCHKEY_TIMEOUT, PER_PAGE};

/// A change request a search listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// The repository or project.
    pub container: String,
    pub number: u32,
    /// When the forge last saw it change, from the listing; empty when
    /// the listing did not say, or the change request was named
    /// directly. Empty never matches what the store holds.
    pub updated_at: String,
}

#[async_trait]
pub trait Forge: Sync {
    type Summary: Default + Serialize + Send;
    /// `PR` or `MR`, in logs.
    const ITEM: &'static str;
    /// What goes between container and number: `#` or `!`.
    const SIGIL: char;
    /// This provider's `scope_config` record. Its discovery scopes share
    /// one, because `refresh_window_days` is one knob for all of them;
    /// the per-scope cursors stay in `sync_scope_state`.
    const SCOPE_CONFIG_KEY: &'static str;

    fn pool(&self) -> &SqlitePool;

    /// Where the account the run authenticates as is read.
    fn self_url(&self) -> String;

    async fn store_self(&self, me: &Value) -> Result<()>;

    async fn search(
        &self,
        client: &ForgeClient,
        scope: &str,
        me: &Value,
        since: Option<&str>,
    ) -> Result<Vec<Value>>;

    /// `since` as a search takes it, from the shared policy's RFC 3339
    /// stamp.
    fn since_param(&self, stamp: String) -> String {
        stamp
    }

    /// A search result as a change request; `None` for one that names
    /// none.
    fn listed(&self, item: &Value) -> Option<Listed>;

    /// Whether the store holds any change request yet. An empty one
    /// discovers everything, whatever the cursors say.
    async fn any_stored(&self) -> Result<bool>;

    /// The `updated_at` of every change request held, so one the listing
    /// shows unchanged is not fetched again. Empty for a forge whose
    /// listing is not trusted for that.
    async fn stored_updated_at(&self) -> Result<HashMap<(String, u32), String>> {
        Ok(HashMap::new())
    }

    /// Fetch one change request and everything under it.
    async fn fetch_one(
        &self,
        client: &ForgeClient,
        cr: &Listed,
        summary: &mut Self::Summary,
    ) -> Result<()>;

    fn record_skipped(&self, _summary: &mut Self::Summary) {}

    fn record_requests(&self, summary: &mut Self::Summary, requests: u64);
}

/// What one sync is asked to do.
pub struct SyncOptions<'a> {
    /// Discovery scopes, as the forge's search takes them.
    pub scopes: &'a [String],
    /// On a non-empty store, only look again at change requests updated
    /// in the last N days; 0 is unbounded.
    pub refresh_window_days: u32,
    /// Safety cap on how many are fetched (`None` = unbounded).
    pub max_items: Option<usize>,
    /// Change requests named directly. When any are, discovery is
    /// skipped and only these are fetched.
    pub targets: &'a [(String, u32)],
    /// Ignore the per-scope cursors, for a full backfill.
    pub full_sync: bool,
    pub sleep_between: Duration,
    pub progress: &'a Progress,
    /// The run's knobs, as `sync_runs` records them.
    pub run_config: Value,
}

pub async fn sync<F: Forge>(
    forge: &F,
    client: &ForgeClient,
    opts: SyncOptions<'_>,
) -> Result<F::Summary> {
    let _ = datalib_etl::latchkey::ensure_curl_router();
    let pool = forge.pool();
    let run = DownloadRun::start(pool, &opts.run_config).await?;

    // Diff the scope-affecting params against the ones that produced the
    // current cursors. `None` (fresh store, or one written before
    // `sync_scope_config` existed) means no adjustment — see the module
    // docs on `scope_config`.
    let scope_cfg = datalib_etl::scope_state::refresh_window_blob(opts.refresh_window_days);
    let prior_scope_cfg = datalib_etl::scope_config::load_or_none(pool, F::SCOPE_CONFIG_KEY).await;

    let mut summary = F::Summary::default();
    // Whether discovery covered every scope this run. Only then has the
    // run satisfied `refresh_window_days`; see `scope_config`.
    let mut discovery_complete = true;

    let work = async {
        let (me, _) = client.get(&forge.self_url()).await?;
        if !me.is_object() {
            anyhow::bail!("{} returned non-object", forge.self_url());
        }
        forge.store_self(&me).await?;

        let keys: Vec<Listed> = if !opts.targets.is_empty() {
            // Named directly: no listing, so nothing to compare against —
            // always fetched. Discovery is skipped, so this run says
            // nothing about whether a widened window was covered.
            discovery_complete = false;
            opts.targets
                .iter()
                .map(|(container, number)| Listed {
                    container: container.clone(),
                    number: *number,
                    updated_at: String::new(),
                })
                .collect()
        } else {
            let full = opts.full_sync || !forge.any_stored().await?;
            let state = datalib_etl::doltlite_raw::load_scope_state(pool).await?;
            let discovered = discover(
                forge,
                client,
                &me,
                opts.scopes,
                &state,
                opts.refresh_window_days,
                full,
                prior_scope_cfg.as_ref(),
            )
            .await;
            if discovered.failed_scopes > 0 {
                discovery_complete = false;
            }
            // Persisted before the per-item fetch, so a crash halfway
            // does not lose discovery progress.
            for (scope, at) in &discovered.new_state {
                datalib_etl::doltlite_raw::upsert_scope_state(pool, scope, at).await?;
            }
            discovered.keys
        };
        let keys: Vec<Listed> = match opts.max_items {
            Some(cap) => keys.into_iter().take(cap).collect(),
            None => keys,
        };
        tracing::info!(count = keys.len(), "{}s to fetch", F::ITEM);

        // One scan of what is held, so the per-item comparison is O(1).
        // This is what lets an interrupted run resume cheaply: the
        // listing still names everything, and the ones already fetched
        // are skipped.
        let stored = if opts.full_sync {
            HashMap::new()
        } else {
            forge.stored_updated_at().await?
        };

        opts.progress.set_length(Some(keys.len() as u64));
        for cr in &keys {
            opts.progress.inc(1);
            opts.progress
                .set_message(&format!("{}{}{}", cr.container, F::SIGIL, cr.number));
            let unchanged = !cr.updated_at.is_empty()
                && stored.get(&(cr.container.clone(), cr.number)) == Some(&cr.updated_at);
            if unchanged {
                forge.record_skipped(&mut summary);
            } else if let Err(e) = forge.fetch_one(client, cr, &mut summary).await {
                tracing::error!(
                    container = %cr.container, number = cr.number, error = %e,
                    "{} fetch failed; skipping", F::ITEM,
                );
            }
            if opts.sleep_between > Duration::ZERO {
                tokio::time::sleep(opts.sleep_between).await;
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    let result = work.await;
    forge.record_requests(&mut summary, client.request_count());
    // Record the config only once this run has actually satisfied it. A
    // skipped scope or a targets-only run leaves the prior blob in place
    // so the next run re-plans the widening.
    datalib_etl::scope_config::store_if_satisfied(
        pool,
        F::SCOPE_CONFIG_KEY,
        &scope_cfg,
        result.is_ok() && discovery_complete,
    )
    .await;
    run.finish(&result, &summary).await;
    result?;
    Ok(summary)
}

/// What one discovery pass found.
struct Discovery {
    /// Unique by `(container, number)`, sorted by it, each with the
    /// newest `updated_at` any scope listed.
    keys: Vec<Listed>,
    /// Next-run cursor per scope. Only scopes that actually searched
    /// appear, so a failed scope keeps its old cursor and retries.
    new_state: HashMap<String, String>,
    /// Scopes whose search failed and were stepped over. Non-zero means
    /// discovery was incomplete, so a widened window has *not* been
    /// satisfied and the config must not be recorded — the blob is one
    /// row for all scopes, so recording it would lose the widening for
    /// the scopes that never ran.
    failed_scopes: usize,
}

#[allow(clippy::too_many_arguments)]
async fn discover<F: Forge>(
    forge: &F,
    client: &ForgeClient,
    me: &Value,
    scopes: &[String],
    state: &HashMap<String, String>,
    refresh_window_days: u32,
    full: bool,
    prior: Option<&Value>,
) -> Discovery {
    let mut newest: HashMap<(String, u32), String> = HashMap::new();
    let mut new_state: HashMap<String, String> = HashMap::new();
    let mut failed_scopes = 0usize;
    for scope in scopes {
        let since = datalib_etl::scope_state::since_for_scope(
            state,
            scope,
            refresh_window_days,
            full,
            prior,
        )
        .map(|stamp| forge.since_param(stamp));
        tracing::info!(scope, since, "searching {}s", F::ITEM);
        let results = match forge.search(client, scope, me, since.as_deref()).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(scope, error = %e, "search failed; skipping scope");
                failed_scopes += 1;
                continue;
            }
        };
        for listed in results.iter().filter_map(|item| forge.listed(item)) {
            let key = (listed.container, listed.number);
            match newest.get(&key) {
                Some(held) if *held >= listed.updated_at => {}
                _ => {
                    newest.insert(key, listed.updated_at);
                }
            }
        }
        new_state.insert(
            scope.clone(),
            IsoOffsetTimestamp::now_local().to_rfc3339_secs(),
        );
        tracing::info!(scope, count = results.len(), "scope done");
    }
    let mut keys: Vec<Listed> = newest
        .into_iter()
        .map(|((container, number), updated_at)| Listed {
            container,
            number,
            updated_at,
        })
        .collect();
    keys.sort_by(|a, b| (&a.container, a.number).cmp(&(&b.container, b.number)));
    Discovery {
        keys,
        new_state,
        failed_scopes,
    }
}

/// Fetch a change request's own record: `None`, logged, when the
/// request failed or came back as something other than an object — the
/// run steps over it rather than failing.
pub async fn get_change_request(
    client: &ForgeClient,
    url: &str,
    item: &str,
    cr: &Listed,
) -> Option<Value> {
    match client.get(url).await {
        Ok((v, _)) if v.is_object() => Some(v),
        Ok(_) => {
            tracing::error!(container = %cr.container, number = cr.number, "{item} returned non-object");
            None
        }
        Err(e) => {
            tracing::error!(container = %cr.container, number = cr.number, error = %e, "{item} meta failed; skipping");
            None
        }
    }
}

/// Walk a change request's whole list of one kind of child. `None`
/// means the walk failed and this run learned nothing about that list:
/// an empty list from a failed request is indistinguishable from "all
/// deleted", and pruning on it would wipe every comment the change
/// request has. The caller neither prunes nor treats the absence as
/// meaningful.
pub async fn walk_children(
    client: &ForgeClient,
    url: &str,
    cr: &Listed,
    what: &str,
) -> Option<Vec<Value>> {
    match client.paginate(url).await {
        Ok(children) => Some(children),
        Err(e) => {
            tracing::warn!(
                event = "forge_child_list_failed",
                container = %cr.container, number = cr.number, list = what, error = %e,
                "could not list this change request's {what}; leaving what we already hold alone",
            );
            None
        }
    }
}

/// Delete `table`'s rows under one change request that its fresh,
/// complete listing did not name. Scoped to that change request: the
/// endpoint enumerated its children and nothing else.
pub async fn prune_children(
    pool: &SqlitePool,
    table: &'static str,
    scope: &[(&str, &str)],
    keep: &std::collections::HashSet<String>,
) -> Result<usize> {
    let gone = datalib_etl::prune::prune_scope(pool, table, scope, keep).await?;
    if !gone.is_empty() {
        tracing::info!(
            event = "forge_children_pruned",
            table,
            scope = ?scope,
            removed = gone.len(),
            "the forge no longer lists these; deleting our copies",
        );
    }
    Ok(gone.len())
}
