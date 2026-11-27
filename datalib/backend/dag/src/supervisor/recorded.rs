//! The loop's tables at a data root, as this crate's tests read them back
//! and set them up.

use std::path::Path;

use datalib_supervisor_store::record::Record;
use datalib_supervisor_store::store::Store;

pub(crate) async fn recorded(root: &Path) -> Record {
    let store = Store::open(root).await.unwrap();
    let record = store.load_record().await.unwrap();
    store.close().await;
    record
}

/// Write `next` over the loop's tables at `root`.
pub(crate) async fn record(root: &Path, next: &Record) {
    let store = Store::open(root).await.unwrap();
    let prev = store.load_record().await.unwrap();
    store.save_record(&prev, next).await.unwrap();
    store.close().await;
}
