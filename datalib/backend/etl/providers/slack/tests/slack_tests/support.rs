//! What the playback tests share: a scratch tree, serving it, one download
//! into a store, and reading the store back.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use datalib_etl::http::PLAYBACK_ENV;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl::synthesize::Synthesizer;
use datalib_etl_slack::ingest::{
    block_on_load_all, db_path_for, fetch, FetchOptions, FetchSummary, RawDb,
};
use datalib_etl_slack::recorded::{record_auth, record_conversations, record_users, CHANNEL_TYPES};
use datalib_etl_slack::synthesize::SlackSynth;
use serde_json::{json, Value};
use tempfile::TempDir;

/// Recorded calls go under `api`, are synthesized into `playback`, and
/// the download writes its store under `out`.
pub struct Tree {
    _dir: TempDir,
    pub api: PathBuf,
    pub playback: PathBuf,
    pub out: PathBuf,
}

impl Tree {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self {
            api: dir.path().join("input_raw"),
            playback: dir.path().join("playback"),
            out: dir.path().join("out_raw"),
            _dir: dir,
        }
    }

    /// Synthesizes what was recorded and points the transport at it.
    /// Returns how many fixtures that made.
    pub fn serve(&self) -> usize {
        serve(&self.api, &self.playback)
    }
}

pub fn serve(api: &Path, playback: &Path) -> usize {
    let report = SlackSynth::new(api).synthesize(playback).unwrap();
    std::env::set_var(PLAYBACK_ENV, playback);
    report.fixtures_written
}

/// One download into `out`, every channel from the default `since`, no
/// refresh window and no media, with `adjust` applied on top. The store
/// is opened here and closed before anything reads it back: a second live
/// connection to one file makes the `dolt_commit`s inside `open` fail.
pub async fn fetch_into(
    out: &Path,
    adjust: impl FnOnce(FetchOptions) -> FetchOptions,
) -> anyhow::Result<FetchSummary> {
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let summary = fetch(adjust(FetchOptions {
        refresh_window_days: 0,
        members_only: false,
        media: false,
        ..FetchOptions::new(db.clone())
    }))
    .await;
    db.commit_all("test").await.unwrap();
    db.close().await;
    summary
}

/// Alice, alone in `#general` (`C1`): the listing calls of a one-channel
/// workspace, its history left to the test.
pub fn record_general(api: &Path) {
    record_auth(api).unwrap();
    record_conversations(
        api,
        CHANNEL_TYPES,
        json!([{"id": "C1", "name": "general", "is_member": true}]),
    )
    .unwrap();
    record_users(api, json!([{"id": "U1", "name": "alice"}])).unwrap();
}

pub fn msg(ts: &str, text: &str) -> Value {
    json!({"ts": ts, "user": "U1", "text": text})
}

pub fn stored_ts(out: &Path) -> Vec<String> {
    let raw = block_on_load_all(&db_path_for(out)).expect("load db");
    let mut ts: Vec<String> = raw.messages.iter().map(|m| m.ts.clone()).collect();
    ts.sort();
    ts
}

pub fn channels_with_messages(out: &Path) -> BTreeSet<String> {
    let raw = block_on_load_all(&db_path_for(out)).expect("load db");
    raw.messages.iter().map(|m| m.channel_id.clone()).collect()
}
