//! The files a source owes, once they have come: their bytes into the
//! blob CAS, and each edge its `blake3`. A provider says how it fetches
//! one; how it lands is here, once.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::Result;
use datalib_etl::blob_cas::{BlobCas, CasEdgeRow, CasInsert};
use datalib_problems::Reason;
use sqlx::{Sqlite, Transaction};

use crate::owed::{Fetched, Outcome};
use datalib_sql::{sql_id, Sql};

/// `ref id → blake3` of every file held, under any record. A file's bytes
/// never change, so one held is not fetched again.
pub type HeldFiles = Mutex<HashMap<String, String>>;

/// What one owed edge's fetch came to, for the file it names.
pub struct File {
    pub ref_id: String,
    pub got: Got,
}

pub enum Got {
    /// The CAS holds the bytes already, by this hash.
    Held(String),
    /// Bytes this run fetched.
    Fetched {
        bytes: Vec<u8>,
        content_type: Option<String>,
    },
    /// The same file, fetched for an earlier edge in this batch.
    FetchedAbove,
}

impl File {
    /// Bytes it holds until its flush.
    pub fn weight(&self) -> usize {
        match &self.got {
            Got::Fetched { bytes, .. } => bytes.len(),
            Got::Held(_) | Got::FetchedAbove => 0,
        }
    }
}

/// A file over the source's size limit is skipped, and judged against the
/// limit again each run.
pub fn over_limit(size: Option<u64>, limit: Option<u64>) -> Option<Outcome<File>> {
    let (size, limit) = (size?, limit?);
    (size > limit).then(|| {
        Outcome::Skipped(
            Reason::OverSizeLimit,
            format!("size {size} > limit {limit}"),
        )
    })
}

/// Lands a flush of `E`'s edges: the bytes first, since the CAS is its own
/// file and commits itself, then each edge's `blake3` in `tx`; an edge
/// that points at nothing is deleted. How many files' bytes were stored.
pub async fn store<E: CasEdgeRow>(
    cas: &BlobCas,
    tx: &mut Transaction<'static, Sqlite>,
    batch: &[Fetched<File>],
    held: &HeldFiles,
) -> Result<usize> {
    let files = batch.iter().filter_map(|f| match &f.outcome {
        Outcome::Got(file) | Outcome::Unusable(file, ..) => Some((f.listed.key.as_str(), file)),
        _ => None,
    });
    let inserts: Vec<CasInsert<'_, &str>> = files
        .clone()
        .filter_map(|(_, file)| match &file.got {
            Got::Fetched {
                bytes,
                content_type,
            } => Some(CasInsert {
                id: file.ref_id.as_str(),
                bytes,
                content_type: content_type.as_deref(),
            }),
            Got::Held(_) | Got::FetchedAbove => None,
        })
        .collect();
    let landed = inserts.len();
    let stored = cas.put_many(inserts).await?;
    // Audited: the table is a provider's `&'static str`; values are bound.
    let update = Sql::dynamic(
        sql_id!("etl.web.owed_files.update_edge_blake3"),
        format!("UPDATE {} SET blake3 = ? WHERE id = ?", E::TABLE),
    );
    let delete = Sql::dynamic(
        sql_id!("etl.web.owed_files.delete_edge"),
        format!("DELETE FROM {} WHERE id = ?", E::TABLE),
    );
    for (key, file) in files {
        let blake3 = match &file.got {
            Got::Held(held) => held.clone(),
            Got::Fetched { .. } | Got::FetchedAbove => stored[file.ref_id.as_str()].clone(),
        };
        update
            .query()
            .bind(&blake3)
            .bind(key)
            .execute(&mut **tx)
            .await?;
        held.lock().unwrap().insert(file.ref_id.clone(), blake3);
    }
    for f in batch.iter().filter(|f| matches!(f.outcome, Outcome::Gone)) {
        delete
            .query()
            .bind(&f.listed.key)
            .execute(&mut **tx)
            .await?;
    }
    Ok(landed)
}
