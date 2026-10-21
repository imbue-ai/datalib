//! What a render reads from the `google_drive` raw store at its pin: the
//! listing entries, each file's newest capture and its parts, and the
//! comments, all small enough to read whole. A Doc's native JSON and its
//! images are read one file at a time, as it renders.

use std::collections::{HashMap, HashSet};
use std::io::Read as _;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::BlobBundle;
use datalib_etl_google_drive::ingest::schema_raw::{
    CAPTURES, CAPTURE_PARTS, COMMENTS, FILES, PART_NATIVE,
};
use datalib_etl_google_drive::ingest::RawDb;
use datalib_etl_render::inputs::{changed_rows, RawRange};
use serde_json::Value;
use sqlx::Row as _;

pub const DOC_MIME: &str = "application/vnd.google-apps.document";

/// Every table a file's document reads; the forward scan diffs each.
/// Every key in them starts with the file id, then `#`.
const TABLES: [&str; 4] = [FILES, CAPTURES, CAPTURE_PARTS, COMMENTS];

/// A file's newest capture and the parts it holds bytes for or not.
#[derive(Debug, Clone)]
pub struct Capture {
    pub id: String,
    /// `(part row id, part name, blake3)`.
    pub parts: Vec<(String, String, Option<String>)>,
}

#[derive(Debug, Default)]
pub struct Listing {
    /// Every Doc the store holds a listing entry for, by file id.
    pub docs: HashMap<String, Value>,
    pub captures: HashMap<String, Capture>,
    /// Each file's comments, as `(raw key, payload)`.
    pub comments: HashMap<String, Vec<(String, Value)>>,
    /// The files to render; `None` renders every Doc.
    pub render: Option<HashSet<String>>,
}

impl Listing {
    fn renders(&self, file: &str) -> bool {
        self.render.as_ref().is_none_or(|r| r.contains(file))
    }
}

/// The file a raw key belongs to: every key is `{file}` or `{file}#…`.
pub fn file_of(key: &str) -> &str {
    key.split_once('#').map_or(key, |(file, _)| file)
}

pub async fn load(db: &RawDb, range: RawRange<'_>) -> Result<Listing> {
    let pin = db.pin().expect("open_reader returns a pinned handle");
    let changed = changed_rows(db.pool(), range, pin, &TABLES).await?;
    let forward = changed.map(|by_table| {
        by_table
            .values()
            .flatten()
            .map(|key| file_of(key).to_string())
            .collect::<HashSet<String>>()
    });
    let mut out = Listing {
        render: range.narrow(forward.as_ref()),
        ..Listing::default()
    };
    for row in sqlx::query("SELECT id, json(payload) AS payload FROM files")
        .fetch_all(db.pool())
        .await
        .context("read files")?
    {
        let id: String = row.try_get("id")?;
        let Some(payload) = row.try_get::<Option<String>, _>("payload")? else {
            continue;
        };
        let entry: Value = serde_json::from_str(&payload)
            .with_context(|| format!("files row {id} is not JSON"))?;
        if out.renders(&id) && entry.get("mimeType").and_then(Value::as_str) == Some(DOC_MIME) {
            out.docs.insert(id, entry);
        }
    }

    let mut newest: HashMap<String, (i64, String)> = HashMap::new();
    for row in sqlx::query("SELECT id, file_id, version FROM captures")
        .fetch_all(db.pool())
        .await
        .context("read captures")?
    {
        let file: String = row.try_get("file_id")?;
        if !out.docs.contains_key(&file) {
            continue;
        }
        let id: String = row.try_get("id")?;
        let version: String = row.try_get("version")?;
        let version: i64 = version.parse().unwrap_or(i64::MIN);
        let slot = newest.entry(file).or_insert((version, id.clone()));
        if (version, &id) > (slot.0, &slot.1) {
            *slot = (version, id);
        }
    }
    let wanted: HashSet<String> = newest.values().map(|(_, id)| id.clone()).collect();
    let mut parts: HashMap<String, Vec<(String, String, Option<String>)>> = HashMap::new();
    for row in sqlx::query("SELECT id, capture_id, part, blake3 FROM capture_parts")
        .fetch_all(db.pool())
        .await
        .context("read capture_parts")?
    {
        let capture: String = row.try_get("capture_id")?;
        if !wanted.contains(&capture) {
            continue;
        }
        parts.entry(capture).or_default().push((
            row.try_get("id")?,
            row.try_get("part")?,
            row.try_get("blake3")?,
        ));
    }
    for (file, (_, id)) in newest {
        let parts = parts.remove(&id).unwrap_or_default();
        out.captures.insert(file, Capture { id, parts });
    }

    for row in sqlx::query("SELECT id, file_id, json(payload) AS payload FROM comments")
        .fetch_all(db.pool())
        .await
        .context("read comments")?
    {
        let file: String = row.try_get("file_id")?;
        if !out.docs.contains_key(&file) {
            continue;
        }
        let id: String = row.try_get("id")?;
        let payload: Option<String> = row.try_get("payload")?;
        let Some(payload) = payload else { continue };
        let comment: Value = serde_json::from_str(&payload)
            .with_context(|| format!("comments row {id} is not JSON"))?;
        out.comments.entry(file).or_default().push((id, comment));
    }
    Ok(out)
}

/// One capture's native JSON, unpacked.
pub async fn native(db: &RawDb, capture: &Capture) -> Result<Value> {
    let (_, _, blake3) = capture
        .parts
        .iter()
        .find(|(_, part, _)| part == PART_NATIVE)
        .with_context(|| format!("capture {} has no native part", capture.id))?;
    let blake3 = blake3
        .as_deref()
        .with_context(|| format!("capture {} stored no native bytes", capture.id))?;
    let object = db
        .cas()
        .get(blake3)
        .await?
        .with_context(|| format!("the blob store has lost {blake3}"))?;
    let mut json = Vec::new();
    flate2::read::GzDecoder::new(&object.bytes[..])
        .read_to_end(&mut json)
        .with_context(|| format!("{blake3} is not gzip"))?;
    serde_json::from_slice(&json).with_context(|| format!("{blake3} is not JSON"))
}

/// The capture's images, keyed by their part row id.
pub async fn images(db: &RawDb, capture: &Capture) -> Result<BlobBundle> {
    let refs: Vec<String> = capture
        .parts
        .iter()
        .filter(|(_, part, _)| part.starts_with("image:"))
        .map(|(id, _, _)| id.clone())
        .collect();
    let mut bundles =
        BlobBundle::load_many(db.pool(), Some(db.cas().pool()), IMAGES_SQL, [((), refs)])
            .await
            .context("load images")?;
    Ok(bundles.remove(&()).unwrap_or_default())
}

const IMAGES_SQL: &str = "
    SELECT id AS ref_id, blake3, NULL AS content_type, NULL AS upstream_name
      FROM capture_parts
     WHERE id IN ({placeholders}) AND blake3 IS NOT NULL";
