//! The interruption test: a download cut off at any request and then run
//! again ends with the store an uninterrupted run leaves.
//!
//! A download is correct under interruption only if what is left to do
//! can be worked out from the store alone, wherever the run stopped
//! (docs/dev/data_architecture_ingestion.md, "What is left to fetch"). This cuts a replayed run off at each
//! request in turn, two ways, and checks that. It commits the store at
//! the cut, including what the run had not sealed, so a download that
//! leans on "the unsealed tail is discarded" fails it.
//!
//! Run it twice per provider: from an empty store, and from the store an
//! earlier run left ([`Rig::seed`]) against a tape where upstream has
//! changed. Garmin's old code passed every cut of a first sync and
//! failed the second kind: none of its bugs could show until something
//! was already stored.
//!
//! Playback only: the cut is made where a replayed request is served.

use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;

use datalib_etl::stop::StopFlag;

/// How a run is cut off at its chosen request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// The process dies: the request never answers and nothing after it
    /// runs. Whatever SQL transactions had committed are in the store.
    Kill,
    /// The step is asked to stop: the request fails as interrupted, the
    /// flag is up, and the download ends by its own stop handling.
    Stop,
}

struct Cut {
    at: Option<u64>,
    how: How,
    stop: StopFlag,
    served: AtomicU64,
    killed: tokio::sync::Notify,
}

/// The cut in force. One for the process, not one per task: a download
/// that spawns its requests onto other tasks must still be counted and
/// cut. So, like the playback root, only one [`run`] may be under way at
/// a time.
static CUT: std::sync::Mutex<Option<Arc<Cut>>> = std::sync::Mutex::new(None);

fn cut_in_force() -> Option<Arc<Cut>> {
    CUT.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

fn set_cut(cut: Option<Arc<Cut>>) {
    *CUT.lock().unwrap_or_else(|p| p.into_inner()) = cut;
}

/// What a replayed request should do in place of answering.
pub(crate) enum Strike {
    Interrupted,
}

/// Called where a replayed request is about to be served. Counts it, and
/// at the chosen request cuts the run off. A no-op outside [`run`].
pub(crate) async fn before_request() -> Option<Strike> {
    let cut = cut_in_force()?;
    let n = cut.served.fetch_add(1, Ordering::SeqCst) + 1;
    if cut.at != Some(n) {
        return None;
    }
    match cut.how {
        How::Stop => {
            cut.stop.request();
            Some(Strike::Interrupted)
        }
        How::Kill => {
            cut.killed.notify_one();
            std::future::pending().await
        }
    }
}

/// One run of `download`, cut off at its `at`-th replayed request.
pub struct Ran<T> {
    /// `None` when the run was killed.
    pub finished: Option<T>,
    /// Replayed requests the run made, the one it was cut at included.
    pub requests: u64,
}

/// Run `download`, counting its replayed requests, and cut it off at the
/// `at`-th. `stop` must be the flag the download itself reads. A killed
/// download is dropped, which aborts the tasks it spawned and holds.
pub async fn run<T>(
    at: Option<u64>,
    how: How,
    stop: StopFlag,
    download: impl Future<Output = T>,
) -> Ran<T> {
    let cut = Arc::new(Cut {
        at,
        how,
        stop,
        served: AtomicU64::new(0),
        killed: tokio::sync::Notify::new(),
    });
    set_cut(Some(cut.clone()));
    let finished = tokio::select! {
        biased;
        _ = cut.killed.notified() => None,
        out = download => Some(out),
    };
    set_cut(None);
    Ran {
        finished,
        requests: cut.served.load(Ordering::SeqCst),
    }
}

/// One provider's download, as the test needs to drive it.
#[async_trait]
pub trait Rig: Sync {
    type Store: Send;

    /// Put under a fresh `dir` whatever the download starts from. The
    /// default is nothing: a first sync. A first sync cannot show a bug
    /// that needs something already stored (a changed item, a widened
    /// range), so a provider also runs the check from the store an
    /// earlier run left, against a tape where upstream has since moved.
    async fn seed(&self, _dir: &Path) -> Result<()> {
        Ok(())
    }

    /// Open the raw store under `dir`, as a writer.
    async fn open(&self, dir: &Path) -> Result<Self::Store>;

    /// One whole download against the playback tape, reading `stop`.
    /// Every run must use the same pinned now.
    async fn download(&self, store: &Self::Store, stop: StopFlag) -> Result<()>;

    /// Commit whatever the store holds, sealed or not, and close it.
    async fn seal(&self, store: Self::Store) -> Result<()>;

    /// The mirrored content under `dir`, as text to compare: the data
    /// tables, in a fixed order. Not attempt counts or their stamps,
    /// which a run that was cut off legitimately has more of.
    async fn contents(&self, dir: &Path) -> Result<String>;
}

/// Cut a download off at each request `which` picks out of the `n` an
/// uninterrupted run makes, run it again, and require the store an
/// uninterrupted run leaves. `scratch` is an empty directory to work in.
pub async fn every_cut_resumes(
    rig: &impl Rig,
    how: How,
    scratch: &Path,
    which: impl FnOnce(u64) -> Vec<u64>,
) -> Result<()> {
    let whole = scratch.join("whole");
    let n = one_run(rig, &whole, None, how)
        .await
        .context("the uninterrupted run")?;
    let want = rig.contents(&whole).await?;
    if want.trim().is_empty() {
        bail!("the uninterrupted run mirrored nothing, so there is nothing to compare");
    }
    one_run(rig, &whole, None, how)
        .await
        .context("a second uninterrupted run")?;
    let again = rig.contents(&whole).await?;
    if again != want {
        bail!(
            "a second run over an unchanged upstream changed the store: {}",
            first_difference(&want, &again)
        );
    }

    for k in which(n) {
        let dir = scratch.join(format!("cut-{k}"));
        one_run(rig, &dir, Some(k), how)
            .await
            .with_context(|| format!("the run cut off at request {k} of {n}"))?;
        one_run(rig, &dir, None, how)
            .await
            .with_context(|| format!("the run after a cut at request {k} of {n}"))?;
        let got = rig.contents(&dir).await?;
        if got != want {
            bail!(
                "cut off at request {k} of {n} ({how:?}) and run again, the store differs \
                 from an uninterrupted run's: {}",
                first_difference(&want, &got)
            );
        }
    }
    Ok(())
}

/// Opens, downloads (cut at `at`), seals. Returns the requests made. A
/// run that was cut may end either way; one that was not must succeed.
async fn one_run(rig: &impl Rig, dir: &Path, at: Option<u64>, how: How) -> Result<u64> {
    if !dir.exists() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        rig.seed(dir).await.context("seed the store")?;
    }
    let store = rig.open(dir).await.context("open the store")?;
    let stop = StopFlag::new();
    let ran = run(at, how, stop.clone(), rig.download(&store, stop)).await;
    let sealed = rig.seal(store).await.context("seal the store");
    if at.is_none() {
        match ran.finished {
            Some(Ok(())) => {}
            Some(Err(e)) => return Err(e.context("the download failed")),
            None => bail!("a run nothing cut off did not finish"),
        }
    }
    sealed?;
    Ok(ran.requests)
}

fn first_difference(want: &str, got: &str) -> String {
    let (mut w, mut g) = (want.lines(), got.lines());
    let mut line = 0;
    loop {
        line += 1;
        match (w.next(), g.next()) {
            (Some(a), Some(b)) if a == b => continue,
            (None, None) => return "no line differs".to_string(),
            (a, b) => {
                let show = |s: Option<&str>| match s {
                    Some(s) => s.chars().take(300).collect::<String>(),
                    None => "<nothing>".to_string(),
                };
                return format!(
                    "line {line}: uninterrupted has `{}`, this has `{}`",
                    show(a),
                    show(b)
                );
            }
        }
    }
}

/// The rows of `tables`, one JSON object per line under a line naming
/// each table, ordered by every column: what a [`Rig::contents`] returns.
pub async fn dump_tables(pool: &sqlx::SqlitePool, tables: &[&str]) -> Result<String> {
    use sqlx::{Column, Row, TypeInfo, ValueRef};
    let mut out = String::new();
    for table in tables {
        out.push_str(&format!("== {table}\n"));
        // Audited: `table` is a name the calling test wrote, never data.
        let sql = format!("SELECT * FROM \"{table}\"");
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(pool)
            .await
            .with_context(|| format!("read {table}"))?;
        let mut lines: Vec<String> = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut fields = serde_json::Map::new();
            for (i, col) in row.columns().iter().enumerate() {
                let raw = row.try_get_raw(i)?;
                let value = if raw.is_null() {
                    serde_json::Value::Null
                } else {
                    match raw.type_info().name() {
                        "INTEGER" => row.try_get::<i64, _>(i)?.into(),
                        "REAL" => row.try_get::<f64, _>(i)?.into(),
                        "BLOB" => {
                            let bytes = row.try_get::<Vec<u8>, _>(i)?;
                            String::from_utf8_lossy(&bytes).into_owned().into()
                        }
                        _ => row.try_get::<String, _>(i)?.into(),
                    }
                };
                fields.insert(col.name().to_string(), value);
            }
            lines.push(serde_json::Value::Object(fields).to_string());
        }
        lines.sort();
        for line in lines {
            out.push_str(&line);
            out.push('\n');
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A toy download in the shape the bugs had: it stores an item's
    /// "have it" row in one transaction and the item's detail in a later
    /// one, and skips an item it has a row for. Cut between the two, it
    /// never fetches the detail. The check has to catch that.
    struct RowDoublesAsDone {
        detail_with_the_row: bool,
    }

    #[async_trait]
    impl Rig for RowDoublesAsDone {
        type Store = sqlx::SqlitePool;

        async fn open(&self, dir: &Path) -> Result<Self::Store> {
            datalib_etl::doltlite_raw::open(
                &dir.join("s.doltlite_db"),
                &[
                    "CREATE TABLE IF NOT EXISTS items (id TEXT PRIMARY KEY)",
                    "CREATE TABLE IF NOT EXISTS details (id TEXT PRIMARY KEY, body TEXT)",
                ],
            )
            .await
        }

        async fn download(&self, pool: &Self::Store, _stop: StopFlag) -> Result<()> {
            use crate::http::{latchkey_curl, HttpRequest, HttpService};
            let get = |path: &str| {
                let req = HttpRequest::get(
                    HttpService::Github,
                    format!("https://api.example.test/{path}"),
                );
                async move { latchkey_curl(&req).await }
            };
            get("items").await?;
            for id in ["picard", "riker"] {
                let have: Option<String> = if self.detail_with_the_row {
                    sqlx::query_scalar("SELECT id FROM details WHERE id = ?")
                        .bind(id)
                        .fetch_optional(pool)
                        .await?
                } else {
                    sqlx::query_scalar("SELECT id FROM items WHERE id = ?")
                        .bind(id)
                        .fetch_optional(pool)
                        .await?
                };
                if have.is_some() {
                    continue;
                }
                sqlx::query("INSERT OR IGNORE INTO items VALUES (?)")
                    .bind(id)
                    .execute(pool)
                    .await?;
                get(&format!("items/{id}")).await?;
                sqlx::query("INSERT OR REPLACE INTO details VALUES (?, 'log')")
                    .bind(id)
                    .execute(pool)
                    .await?;
            }
            Ok(())
        }

        async fn seal(&self, pool: Self::Store) -> Result<()> {
            datalib_etl::doltlite_raw::commit_run(&pool, "test").await?;
            pool.close().await;
            Ok(())
        }

        async fn contents(&self, dir: &Path) -> Result<String> {
            let pool = self.open(dir).await?;
            let out = dump_tables(&pool, &["items", "details"]).await;
            pool.close().await;
            out
        }
    }

    fn tape(dir: &Path) -> std::path::PathBuf {
        use crate::http::{fixture_key, HttpRequest, HttpService};
        let root = dir.join("tape");
        for path in ["items", "items/picard", "items/riker"] {
            let req = HttpRequest::get(
                HttpService::Github,
                format!("https://api.example.test/{path}"),
            );
            let file = root.join("github").join(fixture_key(&req));
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            let answer = crate::http::HttpResponse {
                status: 200,
                headers: Default::default(),
                body: "{}".into(),
                duration_ms: 0,
            };
            std::fs::write(file, serde_json::to_vec(&answer).unwrap()).unwrap();
        }
        root
    }

    #[tokio::test]
    async fn a_download_whose_row_doubles_as_done_fails_and_one_that_asks_what_it_holds_passes() {
        let d = tempfile::tempdir().unwrap();
        let tape = tape(d.path());
        let every = |n: u64| (1..=n).collect::<Vec<_>>();
        crate::http::tests::with_playback(&tape, async {
            for how in [How::Kill, How::Stop] {
                let broken = RowDoublesAsDone {
                    detail_with_the_row: false,
                };
                let scratch = d.path().join(format!("broken-{how:?}"));
                let err = every_cut_resumes(&broken, how, &scratch, every)
                    .await
                    .expect_err("a detail skipped for good must be caught");
                assert!(format!("{err:#}").contains("the store differs"), "{err:#}");

                let sound = RowDoublesAsDone {
                    detail_with_the_row: true,
                };
                let scratch = d.path().join(format!("sound-{how:?}"));
                every_cut_resumes(&sound, how, &scratch, every)
                    .await
                    .unwrap();
            }
        })
        .await;
    }
}
