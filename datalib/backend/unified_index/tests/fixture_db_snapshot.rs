//! Snapshot of the TNG fixture's `backend_index.doltlite_db` contents.
//!
//! One row per source is a storage report (`provider: "datalib"`,
//! `kind: "Source Size"`), and its `text` carries the measured size of
//! that source's raw store. **A doltlite store's size is not stable
//! across rebuilds of identical data** — measured over this fixture,
//! five of its sixteen sources moved by 1-22 bytes on a re-bake of the
//! same inputs. So the byte figure is scrubbed out of `text` before it
//! is digested, the way `stable_source_url` scrubs a sandbox path.
//! Everything else about the row — its path, its counts, its ids —
//! stays pinned.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

fn fixture_db_path() -> PathBuf {
    if let Ok(r) = runfiles::Runfiles::create() {
        if let Some(candidate) =
            r.rlocation("_main/tests/fixtures/ingested/backend_index.doltlite_db")
        {
            if candidate.exists() {
                return candidate;
            }
        }
    }
    let cargo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let bazel_bin = cargo_root
        .join("../../../bazel-bin/tests/fixtures/ingested/backend_index.doltlite_db")
        .canonicalize()
        .unwrap_or_else(|_| {
            panic!(
                "fixture backend_index.doltlite_db not found. Run \
                 `bazelisk build //tests/fixtures:ingested_tng` first."
            )
        });
    bazel_bin
}

async fn open_readonly(path: &std::path::Path) -> SqlitePool {
    // Doltlite-format databases reject `immutable=1` (and the WAL-mode
    // open machinery in general) — the prolly chunk store doesn't
    // model a frozen-bytes view the way stock SQLite's pager does. So
    // we open with `read_only=true` only.
    let real = path
        .canonicalize()
        .unwrap_or_else(|e| panic!("canonicalize {}: {e}", path.display()));
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", real.display()))
        .expect("parse url")
        .read_only(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap_or_else(|e| panic!("open {}: {e}", real.display()))
}

/// SHA-256 of a long string, truncated to 16 hex chars. We snapshot
/// the digest of long body fields (`text`, `entire_chat`) instead of
/// the body itself: the body changes break ~50 lines of diff for what
/// is conceptually a one-row update, and reading a 5KB markdown chat
/// in a `.snap` file isn't actually useful. The digest still catches
/// the regression — if a row's body changes, its digest changes.
fn digest(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let d = h.finalize();
    format!("sha256:{:.16x}", BytesAsHex(&d[..]))
}

struct BytesAsHex<'a>(&'a [u8]);
impl<'a> std::fmt::LowerHex for BytesAsHex<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

fn stable_source_url(v: Option<String>) -> Option<String> {
    let u = v?;
    let Some(rest) = u.strip_prefix("file://") else {
        return Some(u);
    };
    let tail = rest.rsplit('/').next().unwrap_or(rest);
    Some(format!("file://…/{tail}"))
}

/// A storage row's text with the measured size scrubbed out.
///
/// `slack/raw — 292.2 KiB, 8 files` becomes
/// `slack/raw — <size>, 8 files`. Only a byte-unit token is taken: a
/// table row reads `…#messages — 14 rows` and keeps its count, which is
/// stable and is most of what this golden is pinning. Same spirit as
/// [`stable_source_url`]: keep the row, drop the part the environment
/// decides.
/// The `grid_rows.provider` the storage reports carry, from the enum
/// that owns the spelling rather than repeated as a literal.
fn provider_datalib() -> &'static str {
    datalib_schema::providers::Provider::Datalib.as_str()
}

fn stable_text(provider: Option<&str>, text: &str) -> String {
    if provider != Some(provider_datalib()) {
        return text.to_string();
    }
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let looks_like_a_size = |tok: &str| match tok.rsplit_once(' ') {
        Some((n, unit)) => {
            UNITS.contains(&unit)
                && !n.is_empty()
                && n.chars().all(|c| c.is_ascii_digit() || c == '.')
        }
        None => false,
    };
    text.split(", ")
        .map(|part| match part.split_once(" — ") {
            Some((head, tail)) if looks_like_a_size(tail) => format!("{head} — <size>"),
            _ if looks_like_a_size(part) => "<size>".to_string(),
            _ => part.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn stable_row_set_hash(provider: Option<&str>, v: Option<String>) -> Option<String> {
    match provider {
        Some("pdf") => Some("<machine-specific: rows embed an absolute path>".to_string()),
        _ => v,
    }
}

#[tokio::test]
async fn snapshot_grid_rows_and_documents() {
    let db = fixture_db_path();
    let pool = open_readonly(&db).await;

    // ── grid_rows ────────────────────────────────────────────────
    let rows = sqlx::query(
        "SELECT uuid, provider, kind, source_label, when_ts, author, account, \
                project, org_uuid, org_name, channel, conversation_name, conversation_uuid, \
                message_index, entire_chat, text, slack_link, qmd_path, \
                source_url, git_sha, upstream_id, upstream_entity_kind, upstream_scope, \
                notion_page_uuid, \
                notion_block_uuid, markdown_uuid \
         FROM grid_rows ORDER BY uuid",
    )
    .fetch_all(&pool)
    .await
    .expect("read grid_rows");

    let grid_rows: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let provider: Option<String> = r.try_get("provider").ok();
            let text = stable_text(
                provider.as_deref(),
                &r.try_get::<String, _>("text").unwrap_or_default(),
            );
            let entire_chat: String = r.try_get("entire_chat").unwrap_or_default();
            json!({
                "uuid": r.try_get::<String, _>("uuid").ok(),
                "provider": r.try_get::<String, _>("provider").ok(),
                "kind": r.try_get::<String, _>("kind").ok(),
                "source_label": r.try_get::<String, _>("source_label").ok(),
                // `Option<String>`, not `String`: `when_ts` is nullable, and
                // reading it as a bare `String` rendered SQL NULL as `""` —
                // so this golden could not tell "upstream gave us no
                // timestamp" from "upstream gave us an empty one". That is
                // exactly the distinction §6 turns on, and it was invisible
                // here until a fixture finally had an undated record.
                "when_ts": r.try_get::<Option<String>, _>("when_ts").ok().flatten(),
                "author": r.try_get::<Option<String>, _>("author").ok().flatten(),
                "account": r.try_get::<Option<String>, _>("account").ok().flatten(),
                "project": r.try_get::<Option<String>, _>("project").ok().flatten(),
                "org_uuid": r.try_get::<Option<String>, _>("org_uuid").ok().flatten(),
                "org_name": r.try_get::<Option<String>, _>("org_name").ok().flatten(),
                "channel": r.try_get::<Option<String>, _>("channel").ok().flatten(),
                "conversation_name": r.try_get::<Option<String>, _>("conversation_name").ok().flatten(),
                "conversation_uuid": r.try_get::<String, _>("conversation_uuid").ok(),
                "message_index": r.try_get::<Option<i64>, _>("message_index").ok().flatten(),
                "text_len": text.chars().count(),
                "text_sha": digest(&text),
                "entire_chat": entire_chat,
                "slack_link": r.try_get::<Option<String>, _>("slack_link").ok().flatten(),
                "qmd_path": r.try_get::<Option<String>, _>("qmd_path").ok().flatten(),
                "source_url": stable_source_url(
                    r.try_get::<Option<String>, _>("source_url").ok().flatten(),
                ),
                "git_sha": r.try_get::<Option<String>, _>("git_sha").ok().flatten(),
                "upstream_id": r.try_get::<Option<String>, _>("upstream_id").ok().flatten(),
                "upstream_entity_kind": r.try_get::<Option<String>, _>("upstream_entity_kind").ok().flatten(),
                "upstream_scope": r.try_get::<Option<String>, _>("upstream_scope").ok().flatten(),
                "notion_page_uuid": r.try_get::<Option<String>, _>("notion_page_uuid").ok().flatten(),
                "notion_block_uuid": r.try_get::<Option<String>, _>("notion_block_uuid").ok().flatten(),
                "markdown_uuid": r.try_get::<Option<String>, _>("markdown_uuid").ok().flatten(),
            })
        })
        .collect();

    // ── documents ────────────────────────────────────────────────
    // Includes source_fingerprint (render's input-hash) since the
    // markdowns_loaded table merged into documents.
    let drows = sqlx::query(
        "SELECT markdown_uuid, source_name, provider, kind, title, \
                created_at, updated_at, md_path, source_fingerprint, \
                row_set_hash, renderer_version, rendered_at \
         FROM markdowns ORDER BY markdown_uuid",
    )
    .fetch_all(&pool)
    .await
    .expect("read documents");

    let documents: Vec<serde_json::Value> = drows
        .iter()
        .map(|r| {
            json!({
                "markdown_uuid": r.try_get::<String, _>("markdown_uuid").ok(),
                "source_name": r.try_get::<String, _>("source_name").ok(),
                "provider": r.try_get::<String, _>("provider").ok(),
                "kind": r.try_get::<String, _>("kind").ok(),
                "title": r.try_get::<Option<String>, _>("title").ok().flatten(),
                "created_at": r.try_get::<Option<String>, _>("created_at").ok().flatten(),
                "updated_at": r.try_get::<Option<String>, _>("updated_at").ok().flatten(),
                "md_path": r.try_get::<Option<String>, _>("md_path").ok().flatten(),
                "source_fingerprint": r.try_get::<Option<String>, _>("source_fingerprint").ok().flatten(),
                "row_set_hash": stable_row_set_hash(
                    r.try_get::<String, _>("provider").ok().as_deref(),
                    r.try_get::<Option<String>, _>("row_set_hash").ok().flatten(),
                ),
                "renderer_version": r.try_get::<Option<String>, _>("renderer_version").ok().flatten(),
                "rendered_at": r.try_get::<Option<String>, _>("rendered_at").ok().flatten(),
            })
        })
        .collect();

    // ── dolt_log ─────────────────────────────────────────────────
    // doltlite stamps every `dolt_commit` call into `dolt_log`. The
    // grid_index step issues exactly ONE commit per run for the
    // index DB (see datalib_step::grid_index);
    // that, plus doltlite's own "Initialize data repository" boot
    // commit, is what we expect to see here. Snapshotting the
    // commit-message column catches:
    let log_rows = sqlx::query("SELECT message FROM dolt_log() ORDER BY date ASC, message ASC")
        .fetch_all(&pool)
        .await
        .expect("read dolt_log");

    let dolt_log: Vec<serde_json::Value> = log_rows
        .iter()
        .map(|r| json!({"message": r.try_get::<String, _>("message").ok()}))
        .collect();

    // Storage rows are summarized rather than enumerated. They are 43%
    // of the rows and none of this golden's purpose — it exists to
    // catch *provider render* regressions, and a measurement row's
    // per-field detail is pinned by `datalib_step::introspect`'s own
    // tests. Listing them in full added 4,458 lines here, so every
    // future churn of this file would carry them too.
    //
    // The digest keeps the coverage that belongs at this level: a row
    // added, removed, re-keyed, or whose count moved all change it,
    // because it is taken over each row's `(uuid, text)` and the text
    // carries the count.
    let (storage, grid_rows): (Vec<_>, Vec<_>) = grid_rows
        .into_iter()
        .partition(|r| r["provider"] == json!(provider_datalib()));
    let mut by_source_kind: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for r in &storage {
        let key = (
            r["account"].as_str().unwrap_or_default().to_string(),
            r["kind"].as_str().unwrap_or_default().to_string(),
        );
        by_source_kind.entry(key).or_default().push(format!(
            "{}\t{}",
            r["uuid"].as_str().unwrap_or_default(),
            r["text_sha"].as_str().unwrap_or_default(),
        ));
    }
    let storage_rows: Vec<serde_json::Value> = by_source_kind
        .into_iter()
        .map(|((source, kind), mut members)| {
            members.sort();
            json!({
                "source": source,
                "kind": kind,
                "rows": members.len(),
                "members_sha": digest(&members.join("\n")),
            })
        })
        .collect();

    let bundle = json!({
        "summary": {
            "grid_rows_count": grid_rows.len(),
            "storage_rows_count": storage.len(),
            "documents_count": documents.len(),
            "dolt_log_count": dolt_log.len(),
        },
        "grid_rows": grid_rows,
        "storage_rows": storage_rows,
        "documents": documents,
        "dolt_log": dolt_log,
    });

    // Pretty-printed JSON is the most diff-friendly representation —
    // one field per line, sorted keys, no insta-yaml quoting surprises.
    // Measured against the alternatives on this same data: JSON Lines
    // is 340 lines to this one's 6,971 and CSV is 341, but CSV has no
    // native null and this golden turns on null != "" (see `when_ts`
    // above), and neither shows you *which* field moved. The size
    // problem was the storage rows, and it is fixed above.
    let snapshot = serde_json::to_string_pretty(&bundle).expect("serialize");
    insta::assert_snapshot!("fixture_backend_index", snapshot);
}

#[cfg(test)]
mod tests {
    use super::stable_text;

    /// The scrub takes the size and nothing else: the path and the
    /// counts are stable and are what this golden is for.
    #[test]
    fn only_the_measured_size_is_scrubbed() {
        assert_eq!(
            stable_text(Some("datalib"), "slack/raw — 292.2 KiB, 8 files"),
            "slack/raw — <size>, 8 files"
        );
        assert_eq!(
            stable_text(Some("datalib"), "slack/raw/e.doltlite_db — 280.2 KiB"),
            "slack/raw/e.doltlite_db — <size>"
        );
        // A table row has no size to begin with, so its count must
        // survive: it is stable, and it is the half of a Table row this
        // golden exists to pin.
        assert_eq!(
            stable_text(
                Some("datalib"),
                "slack/raw/e.doltlite_db#messages — 14 rows"
            ),
            "slack/raw/e.doltlite_db#messages — 14 rows"
        );
        // A tree keeps its file count while losing its size.
        assert_eq!(
            stable_text(Some("datalib"), "s/raw — 1.0 KiB, 1 file"),
            "s/raw — <size>, 1 file"
        );
    }

    /// Every other provider's text is left exactly as it is — an em
    /// dash in a chat message must not be treated as a measurement.
    #[test]
    fn other_providers_are_untouched() {
        let body = "Tea — Earl Grey — hot, 3 of them";
        assert_eq!(stable_text(Some("slack"), body), body);
        assert_eq!(stable_text(None, body), body);
    }
}
