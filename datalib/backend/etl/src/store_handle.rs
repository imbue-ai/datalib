//! One rule, in one place: a handle that opened doltlite stores releases
//! every one of them.
//!
//! Closing matters because dropping a pool only *schedules* the
//! disconnect. Until it completes the file still has a connection on it,
//! and the next opener contends with that connection rather than having
//! the store to itself — see `datalib/backend/etl/README.md`.

use sqlx::sqlite::SqlitePool;

/// A handle that owns one or more doltlite pools.
///
/// Implement it with `#[derive(RawStoreHandle)]`, which reads the struct's
/// fields: a store added later is picked up without anyone remembering to
/// list it. Writing [`pools`](RawStoreHandle::pools) by hand puts that
/// back in a person's hands, which is the mistake this exists to remove —
/// `RawStoreSession::finish` named one of its two pools and left the
/// other open for exactly that reason.
#[async_trait::async_trait]
pub trait RawStoreHandle {
    /// Every pool this handle opened, in declaration order.
    fn pools(&self) -> Vec<&SqlitePool>;

    /// Close all of them and wait for the connections to actually go away.
    async fn close_all(&self)
    where
        Self: Sync,
    {
        for pool in self.pools() {
            pool.close().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_cas::BlobCas;
    use datalib_etl_macros::RawStoreHandle;

    async fn pool(path: &std::path::Path) -> SqlitePool {
        crate::doltlite_raw::open(
            path,
            &["CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY)"],
        )
        .await
        .unwrap()
    }

    /// The shape every provider's `RawDb` has, plus a field that is not a
    /// store — the derive has to take the first two and leave the third.
    #[derive(RawStoreHandle)]
    struct Handle {
        pool: SqlitePool,
        cas: BlobCas,
        #[allow(dead_code)]
        not_a_store: String,
    }

    /// The derive counts the stores, so adding one to a struct cannot
    /// leave it out of `close_all`. Asserting the count is the point: a
    /// `pools()` written by hand is what silently returned one of two.
    #[tokio::test]
    async fn the_derive_reports_every_store_field_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let h = Handle {
            pool: pool(&dir.path().join("entities.doltlite_db")).await,
            cas: BlobCas::open(&dir.path().join("blobs.doltlite_db"))
                .await
                .unwrap(),
            not_a_store: "ignored".into(),
        };
        assert_eq!(
            h.pools().len(),
            2,
            "the entity pool and the CAS, and not the String"
        );
        h.close_all().await;
        assert!(h.pools().iter().all(|p| p.is_closed()), "all closed");
    }
}
