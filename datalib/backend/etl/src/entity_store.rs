//! The open/close half every provider's `RawDb` shares: its entity
//! store, opened by the download step to write or by render to read at
//! one commit, and — for a provider that keeps bytes — the blob CAS
//! beside it. A provider's `RawDb` wraps one of these, derefs to it, and
//! adds only the queries that are its own; [`raw_db!`](crate::raw_db)
//! declares that wrapper.

use std::path::Path;

use crate::doltlite_raw::Migration;
use anyhow::Result;
use datalib_etl_macros::RawStoreHandle;
use serde_json::Value;
use sqlx::sqlite::SqlitePool;

use crate::blob_cas::{self, BlobCas};
use crate::doltlite_raw as dr;
use crate::pin::Pin;
use crate::store_handle::RawStoreHandle;

/// A provider's entity store, and the commit a reader is pinned at.
#[derive(Clone, Debug, RawStoreHandle)]
pub struct EntityStore {
    pool: SqlitePool,
    /// The commit a reader's connection reads, or `None` for the download
    /// step reading back what it just wrote. Set once, at open.
    pin: Option<Pin>,
}

impl EntityStore {
    /// Open to write, reconciling the schema to `ddl`. Only the download
    /// step that owns the store does this.
    pub async fn open<S: AsRef<str>>(db_path: &Path, ddl: &[S]) -> Result<Self> {
        Self::open_migrating(db_path, ddl, &[]).await
    }

    /// [`Self::open`], climbing the provider's migration ladder first
    /// (etl/README.md §"The migration ladder").
    pub async fn open_migrating<S: AsRef<str>>(
        db_path: &Path,
        ddl: &[S],
        ladder: &[Migration],
    ) -> Result<Self> {
        let slices: Vec<&str> = ddl.iter().map(AsRef::as_ref).collect();
        let pool = dr::open_migrating(db_path, &slices, ladder).await?;
        Ok(Self { pool, pin: None })
    }

    /// Open to *read* somebody else's store, pinned at `commit`, else
    /// HEAD.
    ///
    /// **`None` means the store cannot be read** — nothing is committed
    /// yet — not that it is empty. The caller skips rather than treating
    /// it as a source with no rows: an empty read is what makes render
    /// sweep every document the source has.
    ///
    /// No DDL, and no write of any kind: an ordinary [`Self::open`] would
    /// discard the downloader's in-flight rows, reconcile the schema and
    /// commit on the way in. So a store the current downloader has not
    /// touched keeps whatever columns it has; probe with `column_exists`
    /// and fall back where that matters. See [`dr::open_reader`].
    pub async fn open_reader(db_path: &Path, commit: Option<&str>) -> Result<Option<Self>> {
        let Some(reader) = dr::open_reader(db_path, commit).await? else {
            return Ok(None);
        };
        Ok(Some(Self {
            pool: reader.pool().clone(),
            pin: Some(reader.pin().clone()),
        }))
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// The commit this reader reads at. `None` on the writer's handle.
    pub fn pin(&self) -> Option<&Pin> {
        self.pin.as_ref()
    }

    /// Release every store this handle opened, and wait for the
    /// connections to go away. Dropping only schedules that.
    pub async fn close(&self) {
        self.close_all().await;
    }

    pub async fn load_payloads(&self, table: &str) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, table).await
    }

    /// Like [`Self::load_payloads`], but yields `(id, payload)` so the
    /// caller can join a row against a sibling table.
    pub async fn load_payloads_with_id(&self, table: &str) -> Result<Vec<(String, Value)>> {
        dr::load_payloads_with_id(&self.pool, table).await
    }
}

/// An [`EntityStore`] and the blob CAS beside it, opened and closed
/// together. Derefs to the entity store.
#[derive(Clone, Debug, RawStoreHandle)]
pub struct CasEntityStore {
    entities: EntityStore,
    cas: BlobCas,
}

impl CasEntityStore {
    pub async fn open<S: AsRef<str>>(db_path: &Path, ddl: &[S]) -> Result<Self> {
        Self::open_migrating(db_path, ddl, &[]).await
    }

    pub async fn open_migrating<S: AsRef<str>>(
        db_path: &Path,
        ddl: &[S],
        ladder: &[Migration],
    ) -> Result<Self> {
        let entities = EntityStore::open_migrating(db_path, ddl, ladder).await?;
        let cas = BlobCas::open(&blob_cas::cas_path_for(db_path)).await?;
        Ok(Self { entities, cas })
    }

    /// [`EntityStore::open_reader`], with the CAS opened read-only.
    pub async fn open_reader(db_path: &Path, commit: Option<&str>) -> Result<Option<Self>> {
        let Some(entities) = EntityStore::open_reader(db_path, commit).await? else {
            return Ok(None);
        };
        let cas = BlobCas::open_reader(&blob_cas::cas_path_for(db_path)).await?;
        Ok(Some(Self { entities, cas }))
    }

    pub fn cas(&self) -> &BlobCas {
        &self.cas
    }

    pub async fn close(&self) {
        self.close_all().await;
    }
}

impl std::ops::Deref for CasEntityStore {
    type Target = EntityStore;

    fn deref(&self) -> &EntityStore {
        &self.entities
    }
}

/// Declare a provider's `RawDb`: a newtype over [`EntityStore`] or
/// [`CasEntityStore`] that derefs to it, closes every pool it opened, and
/// opens with the provider's DDL (a `Vec<String>` or a `&[&str]`) and,
/// when it keeps one, its migration ladder. The provider's own queries go
/// in an `impl RawDb` of its own beside the macro.
///
/// ```ignore
/// datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl());
/// datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl(), LADDER);
/// ```
#[macro_export]
macro_rules! raw_db {
    ($(#[$meta:meta])* $vis:vis $name:ident : $store:ident, $ddl:expr) => {
        $crate::raw_db!($(#[$meta])* $vis $name : $store, $ddl, &[]);
    };
    ($(#[$meta:meta])* $vis:vis $name:ident : $store:ident, $ddl:expr, $ladder:expr) => {
        $(#[$meta])*
        #[derive(Clone, Debug)]
        $vis struct $name {
            store: $crate::entity_store::$store,
        }

        impl ::std::ops::Deref for $name {
            type Target = $crate::entity_store::$store;

            fn deref(&self) -> &Self::Target {
                &self.store
            }
        }

        impl $crate::store_handle::RawStoreHandle for $name {
            fn pools(&self) -> ::std::vec::Vec<&::sqlx::sqlite::SqlitePool> {
                $crate::store_handle::RawStoreHandle::pools(&self.store)
            }

            fn versioned_pools(&self) -> ::std::vec::Vec<&::sqlx::sqlite::SqlitePool> {
                $crate::store_handle::RawStoreHandle::versioned_pools(&self.store)
            }
        }

        impl $name {
            $vis async fn open(db_path: &::std::path::Path) -> ::anyhow::Result<Self> {
                let store =
                    $crate::entity_store::$store::open_migrating(db_path, &$ddl[..], $ladder)
                        .await?;
                Ok(Self { store })
            }

            /// Pinned at `commit`, else HEAD; `None` when nothing is
            /// committed. See [`EntityStore::open_reader`](
            /// $crate::entity_store::EntityStore::open_reader).
            $vis async fn open_reader(
                db_path: &::std::path::Path,
                commit: ::std::option::Option<&str>,
            ) -> ::anyhow::Result<::std::option::Option<Self>> {
                let store = $crate::entity_store::$store::open_reader(db_path, commit).await?;
                Ok(store.map(|store| Self { store }))
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    const DDL: &str = "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY)";

    /// A CAS store's handle reaches both files, so closing it leaves
    /// neither with a connection on it.
    #[tokio::test]
    async fn a_cas_store_reports_and_closes_both_pools() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("entities.doltlite_db");
        let store = CasEntityStore::open(&db, &[DDL]).await.unwrap();
        assert_eq!(store.pools().len(), 2, "the entity pool and the CAS");
        assert!(store.pin().is_none(), "the writer is not pinned");
        store.close().await;
        assert!(store.pools().iter().all(|p| p.is_closed()));
    }
}
