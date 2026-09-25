//! The raw store, read at the commit the driver pinned.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use datalib_etl_calendar::ingest::db::{
    LoadedAccount, LoadedCalendar, LoadedGoogleEvent, LoadedIcsObject, RawDb,
};
use datalib_etl_render::inputs::{changed_rows, RawRange};

#[derive(Debug, Default)]
pub struct Parsed {
    pub account: Option<LoadedAccount>,
    pub calendars: Vec<LoadedCalendar>,
    pub ics: Vec<LoadedIcsObject>,
    pub google: Vec<LoadedGoogleEvent>,
    /// The rows the diff names as changed since the cursor, per table;
    /// `None` when there is no cursor and everything renders.
    pub changed: Option<HashMap<String, HashSet<String>>>,
    /// The commit this parse read.
    pub head: Option<String>,
}

pub const TABLES: &[&str] = &["ics_objects", "google_events", "calendars", "accounts"];

/// `None` when the store is absent or cannot be read — not an empty
/// calendar, which would sweep every document the source has.
pub fn parse(db_path: &Path, range: RawRange<'_>) -> Result<Option<Parsed>> {
    if !db_path.exists() {
        return Ok(None);
    }
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let Some(db) = RawDb::open_reader(&path, range.pin).await? else {
                return Ok(None);
            };
            let loaded = async {
                let pin = db.pin().expect("open_reader returns a pinned handle");
                Ok::<_, anyhow::Error>(Parsed {
                    account: db.load_account().await?,
                    calendars: db.load_calendars().await?,
                    ics: db.load_ics_objects().await?,
                    google: db.load_google_events().await?,
                    changed: changed_rows(db.pool(), range, pin, TABLES).await?,
                    head: Some(pin.commit().to_string()),
                })
            }
            .await;
            // Closed, not dropped: the next open of this store is a second
            // connection until this one is actually gone.
            db.close().await;
            loaded.map(Some)
        })
    })
}
