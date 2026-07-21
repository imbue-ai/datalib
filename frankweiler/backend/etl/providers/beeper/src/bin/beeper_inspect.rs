// Standalone read-only CLI: runs outside the pipeline, so there's
// no MultiProgress / indicatif bars on screen. The tool's whole job is
// to write tabular data to stdout. Exempt from the workspace-wide ban
// defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! `beeper-inspect` — quick read-only dump of a Beeper doltlite raw
//! store. Works around the fact that the system `sqlite3` CLI can't
//! read our doltlite-format files (different record encoding).

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Result;
use clap::Parser;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, env = "BEEPER_DB")]
    db: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let opts =
        SqliteConnectOptions::from_str(&format!("sqlite://{}", args.db.display()))?.read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;

    let counts: Vec<(String, i64)> = vec![
        ("rooms".into(), single_count(&pool, "rooms").await?),
        ("users".into(), single_count(&pool, "users").await?),
        ("events".into(), single_count(&pool, "events").await?),
        (
            "media_attachments".into(),
            single_count(&pool, "beeper_media_attachments").await?,
        ),
    ];
    println!("== counts ==");
    for (t, n) in &counts {
        println!("  {t:8} {n}");
    }

    println!("\n== rooms ==");
    let rows = sqlx::query(
        "SELECT source, network, title, is_dm, account_id,
                external_room_id, external_workspace_id
         FROM rooms ORDER BY network, title",
    )
    .fetch_all(&pool)
    .await?;
    for r in &rows {
        let source: String = r.try_get("source")?;
        let network: String = r.try_get("network")?;
        let title: Option<String> = r.try_get("title")?;
        let is_dm: i64 = r.try_get("is_dm")?;
        let account: Option<String> = r.try_get("account_id")?;
        let ext_room: Option<String> = r.try_get("external_room_id")?;
        let ext_ws: Option<String> = r.try_get("external_workspace_id")?;
        println!(
            "  [{source}] network={network} title={title:?} is_dm={is_dm}\n      account={account:?}\n      external_room_id={ext_room:?}\n      external_workspace_id={ext_ws:?}"
        );
    }

    println!("\n== event-type histogram ==");
    let rows = sqlx::query(
        "SELECT network, event_type, COUNT(*) AS n FROM events GROUP BY network, event_type ORDER BY 1, 3 DESC",
    )
    .fetch_all(&pool)
    .await?;
    for r in &rows {
        let network: String = r.try_get("network")?;
        let event_type: String = r.try_get("event_type")?;
        let n: i64 = r.try_get("n")?;
        println!("  {network:10} {event_type:12} {n}");
    }

    println!("\n== 5 most recent text events ==");
    let rows = sqlx::query(
        "SELECT network, timestamp_ms, text_content, external_event_id
         FROM events
         WHERE text_content IS NOT NULL
         ORDER BY timestamp_ms DESC LIMIT 5",
    )
    .fetch_all(&pool)
    .await?;
    for r in &rows {
        let network: String = r.try_get("network")?;
        let ts_ms: i64 = r.try_get("timestamp_ms")?;
        let text: String = r.try_get("text_content")?;
        let ext: Option<String> = r.try_get("external_event_id")?;
        let preview: String = text.chars().take(70).collect();
        println!("  [{network}] external={ext:?} ts={ts_ms} {preview:?}");
    }

    println!("\n== external_event_id population by network ==");
    let rows = sqlx::query(
        "SELECT network,
                SUM(CASE WHEN external_event_id IS NOT NULL THEN 1 ELSE 0 END) AS with_ext,
                COUNT(*) AS total
         FROM events GROUP BY network",
    )
    .fetch_all(&pool)
    .await?;
    for r in &rows {
        let network: String = r.try_get("network")?;
        let with_ext: i64 = r.try_get("with_ext")?;
        let total: i64 = r.try_get("total")?;
        println!("  {network:10} {with_ext}/{total} events have external_event_id");
    }

    println!("\n== beeper_media_attachments ==");
    let rows = sqlx::query("SELECT event_uuid, ref_id, blake3 FROM beeper_media_attachments")
        .fetch_all(&pool)
        .await?;
    for r in &rows {
        let event_uuid: String = r.try_get("event_uuid")?;
        let ref_id: String = r.try_get("ref_id")?;
        let hash: Option<String> = r.try_get("blake3")?;
        let h_short = hash.as_deref().map(|h| &h[..16.min(h.len())]);
        println!("  event={event_uuid} ref_id={ref_id:?} blake3={h_short:?}");
    }

    println!("\n== users ==");
    let rows =
        sqlx::query("SELECT source, network, native_user_id, full_name, display_name FROM users")
            .fetch_all(&pool)
            .await?;
    for r in &rows {
        let source: String = r.try_get("source")?;
        let network: Option<String> = r.try_get("network")?;
        let mxid: String = r.try_get("native_user_id")?;
        let full: Option<String> = r.try_get("full_name")?;
        let display: Option<String> = r.try_get("display_name")?;
        println!(
            "  [{source}] network={network:?} mxid={mxid} full_name={full:?} display={display:?}"
        );
    }

    Ok(())
}

async fn single_count(pool: &sqlx::SqlitePool, table: &str) -> Result<i64> {
    let row = sqlx::query(&format!("SELECT COUNT(*) AS n FROM {table}"))
        .fetch_one(pool)
        .await?;
    Ok(row.try_get("n")?)
}
