//! Part of a sync that fails is a `problems` row, not a failed step, and
//! the row clears only once the same thing has been tried again and
//! worked.

use std::fs;
use std::path::{Path, PathBuf};

use datalib_etl::http::{HttpRequest, HttpResponse, HttpService, PLAYBACK_ENV};
use datalib_etl::retry::{self, RetryGuard};
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl::synthesize::{write_fixture, Synthesizer};
use datalib_etl_claude::ingest::{db_path_for, fetch, FetchOptions, FetchSummary, RawDb};
use datalib_etl_claude::synthesize::{ClaudeSynth, BASE, DETAIL_QUERY};
use serde_json::{json, Value};
use tempfile::TempDir;

const ENTERPRISE: (&str, &str) = ("org-a", "Enterprise");
const DEFIANT: (&str, &str) = ("org-b", "Defiant");
const VOYAGER: (&str, &str) = ("org-c", "Voyager");

fn conv(id: &str, (org, org_name): (&str, &str)) -> Value {
    json!({
        "uuid": id,
        "name": id,
        "updated_at": "2369-01-02T00:00:00Z",
        "organization_uuid": org,
        "account": {"uuid": "acct-1"},
        "chat_messages": [],
        "_source": {"via": "claude.ai/api", "org_uuid": org, "org_name": org_name},
    })
}

fn project(id: &str, name: &str, (org, org_name): (&str, &str)) -> Value {
    json!({
        "uuid": id,
        "name": name,
        "creator": {"uuid": "acct-1"},
        "created_at": "2369-01-01T00:00:00Z",
        "updated_at": "2369-01-01T00:00:00Z",
        "_source": {"org_uuid": org, "org_name": org_name},
        "docs": [{"uuid": format!("{id}-doc"), "file_name": "orders.md", "content": "Engage."}],
    })
}

fn api_get(path: &str) -> HttpRequest {
    HttpRequest::get(HttpService::Claude, format!("{BASE}{path}"))
        .header("Accept", "application/json")
}

fn listing((org, _): (&str, &str)) -> HttpRequest {
    api_get(&format!("/organizations/{org}/chat_conversations"))
}

fn projects((org, _): (&str, &str)) -> HttpRequest {
    api_get(&format!("/organizations/{org}/projects"))
}

fn docs((org, _): (&str, &str), project: &str) -> HttpRequest {
    api_get(&format!("/organizations/{org}/projects/{project}/docs"))
}

fn detail((org, _): (&str, &str), id: &str) -> HttpRequest {
    api_get(&format!(
        "/organizations/{org}/chat_conversations/{id}?{DETAIL_QUERY}"
    ))
}

/// One account's input snapshot, its playback tape, and the store a run
/// writes.
struct Account {
    _dir: TempDir,
    api: PathBuf,
    playback: PathBuf,
    raw: PathBuf,
}

impl Account {
    fn new(with_users: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let a = Account {
            api: dir.path().join("api"),
            playback: dir.path().join("playback"),
            raw: dir.path().join("raw"),
            _dir: dir,
        };
        fs::create_dir_all(&a.raw).unwrap();
        fs::create_dir_all(&a.api).unwrap();
        if with_users {
            a.write_users();
        }
        a
    }

    fn write_users(&self) {
        write_json(&self.api.join("users.json"), &json!([{"uuid": "acct-1"}]));
    }

    /// Rewrite what upstream holds, and every fixture that answers it.
    fn holds(&self, convs: &[Value], projects: &[Value]) {
        write_json(
            &self.api.join("conversations.json"),
            &Value::Array(convs.to_vec()),
        );
        let dir = self.api.join("projects");
        let _ = fs::remove_dir_all(&dir);
        for p in projects {
            write_json(
                &dir.join(format!("{}.json", p["uuid"].as_str().unwrap())),
                p,
            );
        }
        ClaudeSynth::new(&self.api)
            .synthesize(&self.playback)
            .unwrap();
    }

    fn answer(&self, req: &HttpRequest, status: u16, body: &[u8]) {
        let resp = HttpResponse {
            status,
            headers: Default::default(),
            body: body.to_vec(),
            duration_ms: 0,
        };
        write_fixture(&self.playback, req, &resp).unwrap();
    }

    fn fail(&self, req: &HttpRequest, status: u16) {
        self.answer(req, status, b"{\"error\":\"no\"}");
    }

    async fn run(&self, tweak: impl FnOnce(&mut FetchOptions)) -> anyhow::Result<FetchSummary> {
        std::env::set_var(PLAYBACK_ENV, &self.playback);
        let db = RawDb::open(&db_path_for(&self.raw)).await.unwrap();
        let control = datalib_etl::control::DownloadControl::default();
        // One failure is the give-up: a 429 ends as a rate limit at once.
        let fast = std::time::Duration::from_millis(1);
        let guard = RetryGuard::new(
            std::time::Duration::from_secs(3600),
            1,
            fast,
            fast,
            control.stop.clone(),
        );
        let mut o = FetchOptions {
            export_dir: Some(self.api.clone()),
            control,
            ..FetchOptions::new(db.clone())
        };
        tweak(&mut o);
        let s = retry::scope(guard, fetch(o)).await;
        db.commit_all("test").await.unwrap();
        db.close().await;
        std::env::remove_var(PLAYBACK_ENV);
        s
    }

    /// `(scope_key, sample)` of every `problems` row.
    async fn problems(&self) -> Vec<(String, String)> {
        self.query("SELECT scope_key, sample FROM problems ORDER BY scope_key")
            .await
    }

    async fn keys(&self) -> Vec<String> {
        self.problems().await.into_iter().map(|(k, _)| k).collect()
    }

    async fn conversation_ids(&self) -> Vec<String> {
        self.query("SELECT id, id FROM conversations ORDER BY id")
            .await
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    async fn query(&self, sql: &'static str) -> Vec<(String, String)> {
        let db = RawDb::open(&db_path_for(&self.raw)).await.unwrap();
        let rows = sqlx::query_as(sql).fetch_all(db.pool()).await.unwrap();
        db.close().await;
        rows
    }
}

fn write_json(path: &Path, v: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(v).unwrap()).unwrap();
}

fn no_keys() -> Vec<String> {
    Vec::new()
}

/// One test, several scenarios, run in sequence: `PLAYBACK_ENV` is
/// process-global, so as separate tests they would race.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn part_of_a_sync_that_fails_is_a_problem_row() {
    an_org_whose_listing_fails_costs_only_that_org().await;
    project_listings_that_fail_are_rows_until_they_list().await;
    a_named_conversation_that_fails_costs_only_itself().await;
    an_account_that_will_not_load_is_a_phase_row().await;
    a_failed_attachment_says_why_and_lands_on_a_later_run().await;
    a_stub_no_listing_names_goes_with_its_problem().await;
    a_refused_org_holds_back_the_stub_prune_and_all_refused_fails().await;
    a_project_whose_docs_failed_is_asked_again().await;
    a_file_claude_no_longer_has_is_not_asked_for_again().await;
    a_rate_limit_ends_the_walk_with_one_row().await;
}

/// A conversation listing that failed other than 403 used to fail the
/// whole step; a refused org's row is reported beside it, once.
async fn an_org_whose_listing_fails_costs_only_that_org() {
    let acct = Account::new(true);
    let convs = [
        conv("c-a1", ENTERPRISE),
        conv("c-b1", DEFIANT),
        conv("c-c1", VOYAGER),
    ];
    acct.holds(&convs, &[]);
    let s = acct.run(|_| {}).await.unwrap();
    assert_eq!(s.fetched, 3, "{s:?}");

    acct.fail(&listing(DEFIANT), 500);
    acct.fail(&listing(VOYAGER), 403);
    acct.run(|_| {}).await.expect("two orgs listed nothing");
    assert_eq!(
        acct.keys().await,
        ["listing:conversations org:Defiant", "listing:org:Voyager"]
    );
    assert_eq!(
        acct.conversation_ids().await,
        ["c-a1", "c-b1", "c-c1"],
        "an org that did not list is not pruned"
    );

    acct.holds(&convs, &[]);
    acct.run(|_| {}).await.unwrap();
    assert_eq!(acct.keys().await, no_keys());
}

/// Every project listing that failed was a `warn!`, and a configured
/// project uuid nothing had was another.
async fn project_listings_that_fail_are_rows_until_they_list() {
    let acct = Account::new(true);
    let convs = [conv("c-a1", ENTERPRISE), conv("c-b1", DEFIANT)];
    let held = [
        project("p-1", "Bridge Operations", ENTERPRISE),
        project("p-2", "Holodeck", ENTERPRISE),
        project("p-3", "Ops", DEFIANT),
    ];
    acct.holds(&convs, &held);
    let day_one = |o: &mut FetchOptions| o.now = Some("2369-02-01T00:00:00Z".into());
    acct.run(|o| {
        day_one(o);
        o.project_uuids = vec!["p-1".into(), "p-typo".into()];
    })
    .await
    .unwrap();
    assert_eq!(acct.keys().await, ["config:project_uuids:p-typo"]);

    // A day later by the run's clock, so the docs are due again.
    let day_two = |o: &mut FetchOptions| o.now = Some("2369-02-02T01:00:00Z".into());
    acct.fail(&docs(ENTERPRISE, "p-1"), 500);
    acct.fail(&docs(ENTERPRISE, "p-2"), 403);
    acct.fail(&projects(DEFIANT), 500);
    acct.run(day_two)
        .await
        .expect("the conversations still synced");
    assert_eq!(
        acct.keys().await,
        [
            "listing:project_docs Bridge Operations",
            "listing:project_docs Holodeck",
            "listing:projects org:Defiant",
        ]
    );

    // A docs listing that failed left its sweep marker alone, so the
    // same run's now asks for it again.
    acct.holds(&convs, &held);
    let s = acct.run(day_two).await.unwrap();
    assert_eq!(s.project_docs_fetched, 3, "{s:?}");
    assert_eq!(acct.keys().await, no_keys());
}

/// One named conversation that failed used to fail the step, and its
/// `config:` rows outlived a move off `conv_uuids`.
async fn a_named_conversation_that_fails_costs_only_itself() {
    let acct = Account::new(true);
    let convs = [conv("c-a1", ENTERPRISE), conv("c-a2", ENTERPRISE)];
    acct.holds(&convs, &[]);
    acct.fail(&detail(ENTERPRISE, "c-a1"), 500);
    acct.fail(&detail(ENTERPRISE, "c-nope"), 404);
    let s = acct
        .run(|o| o.conv_uuids = vec!["c-a1".into(), "c-nope".into(), "c-a2".into()])
        .await
        .expect("one failure is not the run's");
    assert_eq!((s.fetched, s.errors), (1, 1), "{s:?}");
    assert_eq!(
        acct.keys().await,
        ["config:conv_uuids:c-nope", "conversations:c-a1"]
    );

    acct.holds(&convs, &[]);
    acct.run(|_| {}).await.unwrap();
    assert_eq!(acct.keys().await, no_keys());
}

/// `/account` failing was a `warn!` and an empty users table.
async fn an_account_that_will_not_load_is_a_phase_row() {
    let acct = Account::new(false);
    acct.holds(&[conv("c-a1", ENTERPRISE)], &[]);
    acct.run(|_| {}).await.unwrap();
    assert_eq!(acct.keys().await, ["phase:account"]);

    acct.write_users();
    acct.holds(&[conv("c-a1", ENTERPRISE)], &[]);
    acct.run(|_| {}).await.unwrap();
    assert_eq!(acct.keys().await, no_keys());
}

/// An attachment's row said "no bytes" whatever went wrong, and was tried
/// again only when its conversation changed.
async fn a_failed_attachment_says_why_and_lands_on_a_later_run() {
    let acct = Account::new(true);
    let mut with_file = conv("c-a1", ENTERPRISE);
    with_file["chat_messages"] = json!([{
        "uuid": "m-1",
        "sender": "human",
        "text": "Readout attached.",
        "content": [{"type": "text", "text": "Readout attached."}],
        "files": [{"file_uuid": "f-1", "file_name": "readout.png", "file_kind": "image",
                   "preview_url": "/api/files/f-1/preview"}],
    }]);
    acct.holds(&[with_file], &[]);
    let s = acct.run(|_| {}).await.unwrap();
    assert_eq!(s.failed_blobs, 1, "{s:?}");
    assert_eq!(
        acct.problems().await,
        [(
            "claude_attachments:c-a1#f-1".to_string(),
            "no recorded response: GET https://claude.ai/api/files/f-1/preview".to_string()
        )]
    );

    acct.answer(
        &HttpRequest::get(
            HttpService::Claude,
            "https://claude.ai/api/files/f-1/preview",
        ),
        200,
        b"tricorder",
    );
    let s = acct.run(|_| {}).await.unwrap();
    assert_eq!(
        (s.skipped, s.new_blobs),
        (1, 1),
        "the conversation is unchanged, its file is fetched again: {s:?}"
    );
    assert_eq!(acct.keys().await, no_keys());
}

/// A conversation whose every fetch failed is a stub with no org, which
/// the per-org prune never reached: deleted upstream, it and its row
/// stood for good.
async fn a_stub_no_listing_names_goes_with_its_problem() {
    let acct = Account::new(true);
    acct.holds(&[conv("c-a1", ENTERPRISE), conv("c-x", ENTERPRISE)], &[]);
    acct.fail(&detail(ENTERPRISE, "c-x"), 500);
    acct.run(|_| {}).await.unwrap();
    assert_eq!(acct.keys().await, ["conversations:c-x"]);

    acct.holds(&[conv("c-a1", ENTERPRISE)], &[]);
    acct.run(|_| {}).await.unwrap();
    assert_eq!(acct.keys().await, no_keys());
    assert_eq!(acct.conversation_ids().await, ["c-a1"]);
}

/// A stub belongs to whichever org listed it, so an org that refused its
/// listing holds the stub prune back; and a run every org refuses (a
/// session expired inside the org cache) fails rather than reading as
/// "everything deleted".
async fn a_refused_org_holds_back_the_stub_prune_and_all_refused_fails() {
    let acct = Account::new(true);
    acct.holds(
        &[
            conv("c-a1", ENTERPRISE),
            conv("c-x", ENTERPRISE),
            conv("c-b1", DEFIANT),
        ],
        &[],
    );
    acct.fail(&detail(ENTERPRISE, "c-x"), 500);
    acct.run(|_| {}).await.unwrap();
    assert_eq!(acct.keys().await, ["conversations:c-x"]);

    acct.holds(&[conv("c-a1", ENTERPRISE), conv("c-b1", DEFIANT)], &[]);
    acct.fail(&listing(DEFIANT), 403);
    acct.run(|_| {}).await.unwrap();
    assert_eq!(
        acct.keys().await,
        ["conversations:c-x", "listing:org:Defiant"],
        "the stub may be Defiant's"
    );

    acct.fail(&listing(ENTERPRISE), 403);
    let err = acct.run(|_| {}).await.expect_err("no org would list");
    assert!(format!("{err:#}").contains("every org refused"), "{err:#}");
    assert_eq!(acct.conversation_ids().await, ["c-a1", "c-b1", "c-x"]);
}

/// A changed project's metadata was stored and its docs listing failed;
/// the next run saw the metadata current and the docs swept under a day
/// ago, so it asked for nothing and the row cleared on a stale project.
async fn a_project_whose_docs_failed_is_asked_again() {
    let acct = Account::new(true);
    let convs = [conv("c-a1", ENTERPRISE)];
    let at = |o: &mut FetchOptions| o.now = Some("2369-02-01T00:00:00Z".into());
    acct.holds(&convs, &[project("p-1", "Bridge Operations", ENTERPRISE)]);
    acct.run(at).await.unwrap();

    let mut changed = project("p-1", "Bridge Operations", ENTERPRISE);
    changed["updated_at"] = json!("2369-01-05T00:00:00Z");
    acct.holds(&convs, &[changed.clone()]);
    acct.fail(&docs(ENTERPRISE, "p-1"), 500);
    let an_hour_on = |o: &mut FetchOptions| o.now = Some("2369-02-01T01:00:00Z".into());
    acct.run(an_hour_on).await.unwrap();
    assert_eq!(
        acct.keys().await,
        ["listing:project_docs Bridge Operations"]
    );

    acct.holds(&convs, &[changed]);
    let s = acct.run(an_hour_on).await.unwrap();
    assert_eq!(
        (s.projects_skipped, s.project_docs_fetched),
        (1, 1),
        "{s:?}"
    );
    assert_eq!(acct.keys().await, no_keys());
}

fn conv_with_file() -> Value {
    let mut c = conv("c-a1", ENTERPRISE);
    c["chat_messages"] = json!([{
        "uuid": "m-1",
        "sender": "human",
        "text": "Readout attached.",
        "content": [{"type": "text", "text": "Readout attached."}],
        "files": [{"file_uuid": "f-1", "file_name": "readout.png", "file_kind": "image",
                   "preview_url": "/api/files/f-1/preview"}],
    }]);
    c
}

fn preview() -> HttpRequest {
    HttpRequest::get(
        HttpService::Claude,
        "https://claude.ai/api/files/f-1/preview",
    )
}

/// A 404 on a file was retried every run, for good.
async fn a_file_claude_no_longer_has_is_not_asked_for_again() {
    let acct = Account::new(true);
    acct.holds(&[conv_with_file()], &[]);
    acct.fail(&preview(), 404);
    acct.run(|_| {}).await.unwrap();
    let rows = acct
        .query("SELECT scope_key, reason FROM problems ORDER BY scope_key")
        .await;
    assert_eq!(
        rows,
        [(
            "claude_attachments:c-a1#f-1".to_string(),
            "not_found".to_string()
        )]
    );

    let s = acct.run(|_| {}).await.unwrap();
    assert_eq!(
        (s.skipped, s.failed_blobs),
        (1, 0),
        "an unchanged conversation's gone file is not asked for: {s:?}"
    );
}

/// After the give-up guard tripped, every later request was refused at
/// once, and the walk wrote one failure row per conversation and file.
async fn a_rate_limit_ends_the_walk_with_one_row() {
    let acct = Account::new(true);
    acct.holds(&[conv("c-a1", ENTERPRISE), conv("c-a2", ENTERPRISE)], &[]);
    acct.fail(&detail(ENTERPRISE, "c-a1"), 429);
    let s = acct
        .run(|_| {})
        .await
        .expect("a rate limit is not a failed step");
    assert_eq!((s.fetched, s.errors), (0, 0), "{s:?}");
    assert_eq!(acct.keys().await, ["phase:conversations"]);

    // In the retry pass too.
    let acct = Account::new(true);
    acct.holds(&[conv_with_file()], &[]);
    acct.run(|_| {}).await.unwrap();
    acct.fail(&preview(), 429);
    acct.run(|_| {}).await.unwrap();
    assert_eq!(
        acct.keys().await,
        ["claude_attachments:c-a1#f-1", "phase:attachments"]
    );
}
