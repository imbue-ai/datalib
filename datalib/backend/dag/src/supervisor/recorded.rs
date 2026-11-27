//! The loop's tables at a data root, as this crate's tests read them back
//! and set them up.

use std::path::Path;

use datalib_supervisor_store::record::LoopState;
use datalib_supervisor_store::store::Store;

pub(crate) async fn saved_state(root: &Path) -> LoopState {
    let store = Store::open(root).await.unwrap();
    let record = store.load_state().await.unwrap();
    store.close().await;
    record
}

/// Write `next` over the loop's tables at `root`.
pub(crate) async fn record(root: &Path, next: &LoopState) {
    let store = Store::open(root).await.unwrap();
    let prev = store.load_state().await.unwrap();
    store.save_state(&prev, next).await.unwrap();
    store.close().await;
}
