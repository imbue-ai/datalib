//! Previews for the rows a store held before the column existed. They are
//! made from the `.eml` it holds, which asks upstream for nothing.

use anyhow::{Context, Result};
use datalib_etl::raw_store::Sealer;
use datalib_etl::stop::StopFlag;
use tracing::info;

use super::db::RawDb;
use super::envelope;

/// Rows written in one transaction. A batch's bytes are read and dropped one
/// message at a time, so this bounds no memory, only what a stop can lose.
const BATCH: usize = 200;

/// Fill `emails.preview` wherever it is NULL and the bytes are held. One
/// whose bytes will not parse stays NULL, and render reports that message.
/// A row filled is never asked about again, so this costs nothing once a
/// store has caught up.
pub(crate) async fn fill_from_held_bytes(
    db: &RawDb,
    sealer: Option<&Sealer>,
    stop: &StopFlag,
) -> Result<()> {
    let unfilled: Vec<(String, String)> = sqlx::query_as(
        "SELECT e.id, b.blake3 FROM emails e
         JOIN email_blobs b ON b.email_id = e.id AND b.blob_id = e.blob_id
         WHERE e.preview IS NULL AND b.blake3 IS NOT NULL",
    )
    .fetch_all(db.pool())
    .await
    .context("select the emails with no preview")?;
    let mut filled = 0usize;
    for batch in unfilled.chunks(BATCH) {
        if stop.requested() {
            break;
        }
        let mut previews = Vec::with_capacity(batch.len());
        for (id, blake3) in batch {
            let Some(object) = db.cas().get(blake3).await? else {
                continue;
            };
            if let Ok(msg) = envelope::parse(&object.bytes) {
                previews.push((id, envelope::preview(&msg)));
            }
        }
        let mut tx = db.pool().begin().await.context("begin previews tx")?;
        for (id, preview) in &previews {
            sqlx::query("UPDATE emails SET preview = ? WHERE id = ?")
                .bind(preview)
                .bind(id)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("write the preview of email {id}"))?;
        }
        tx.commit().await.context("commit previews tx")?;
        if let Some(sealer) = sealer {
            sealer.wrote(previews.len() as u64).await;
        }
        filled += previews.len();
    }
    if filled > 0 {
        info!(
            event = "email_previews_filled",
            filled,
            unfilled = unfilled.len(),
            "made previews from the held bytes",
        );
    }
    Ok(())
}
