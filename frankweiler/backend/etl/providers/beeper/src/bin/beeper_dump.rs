//! Throwaway debug helper: dump a sample room state from the raw
//! store so we can see what Beeper actually emits for bridge
//! identification. Delete once the inference is reliable.

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Result;
use clap::Parser;
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;

#[derive(Parser, Debug)]
struct Args {
    #[arg(long)]
    db: PathBuf,
    /// Substring to match against display_name or matrix_room_id. Empty = first row.
    #[arg(long, default_value = "")]
    grep: String,
    /// Maximum rooms to dump.
    #[arg(long, default_value_t = 1)]
    limit: i64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", args.db.display()))?
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;

    // Use json() so we get back parseable text regardless of how the
    // payload column was stored (TEXT vs JSONB).
    let q = if args.grep.is_empty() {
        "SELECT matrix_room_id, display_name, bridge_network, json(payload) AS p \
         FROM rooms LIMIT ?"
    } else {
        "SELECT matrix_room_id, display_name, bridge_network, json(payload) AS p \
         FROM rooms \
         WHERE display_name LIKE ?1 OR matrix_room_id LIKE ?1 \
         LIMIT ?2"
    };
    let mut query = sqlx::query(q);
    if !args.grep.is_empty() {
        query = query.bind(format!("%{}%", args.grep));
    }
    let query = query.bind(args.limit);
    let rows = query.fetch_all(&pool).await?;
    for row in &rows {
        let mxid: String = row.try_get("matrix_room_id")?;
        let name: Option<String> = row.try_get("display_name")?;
        let net: String = row.try_get("bridge_network")?;
        let payload: String = row.try_get("p")?;
        eprintln!("\n=== {} ({:?}) bridge_network={} ===", mxid, name, net);
        let v: Value = serde_json::from_str(&payload)?;
        // Print one line per state event so we can grep types.
        if let Some(arr) = v.as_array() {
            // Bias the dump toward bridge-identification events.
            let mut interesting: Vec<&Value> = arr
                .iter()
                .filter(|e| {
                    let t = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    t.contains("bridge")
                        || t == "m.room.create"
                        || t == "m.room.name"
                        || t == "m.room.canonical_alias"
                        || t.starts_with("com.beeper")
                        || t.starts_with("fi.mau")
                        || t.starts_with("uk.half-shot")
                })
                .collect();
            if interesting.is_empty() {
                interesting = arr.iter().take(8).collect();
            }
            for ev in interesting {
                println!(
                    "{}",
                    serde_json::to_string(ev).unwrap_or_default()
                );
            }
        }
    }
    Ok(())
}
