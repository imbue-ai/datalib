//! Contacts' photos. Google lists a contact's photo as a link on
//! `googleusercontent.com` that needs no sign-in; each one listed and not
//! held is fetched once, at [`SIZE`], into the blob store.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::blob_cas::CasInsert;
use datalib_etl::run_problems::RunProblems;
use datalib_etl_web::google::is_googleusercontent;
use datalib_etl_web::http::{
    latchkey_curl, HttpError, HttpRequest, HttpResponse, DEFAULT_MAX_BYTES,
};
use datalib_etl_web::owed::{self, BatchError, Fetched, Fetcher, Listed, Loop, Outcome};
use serde_json::Value;
use sqlx::{Sqlite, Transaction};

use super::super::db::RawDb;
use super::super::FetchSummary;
use super::db::PHOTOS;
use super::{FetchOptions, API};

/// The longer side asked for, in pixels; Google never scales one up.
const SIZE: u32 = 512;
const CONCURRENCY: usize = 4;
const FLUSH: usize = 50;
const FLUSH_BYTES: usize = 8 << 20;
const FAILURES_IN_A_ROW: usize = 20;

/// The link to a contact's photo, if it has one of its own: a `default`
/// one is a letter Google drew. The primary one, where there are two.
pub fn photo_url(person: &Value) -> Option<String> {
    let is = |p: &Value, pointer: &str| p.pointer(pointer).and_then(Value::as_bool) == Some(true);
    let own: Vec<&Value> = person
        .get("photos")?
        .as_array()?
        .iter()
        .filter(|p| !is(p, "/default"))
        .collect();
    own.iter()
        .find(|p| is(p, "/metadata/primary"))
        .or_else(|| own.first())
        .and_then(|p| super::str_of(p, "url"))
}

/// `url`, at [`SIZE`]: the link ends in the size it would serve (`=s100`).
pub fn sized(url: &str) -> String {
    match url.rsplit_once("=s") {
        Some((base, size)) if !size.is_empty() && size.bytes().all(|b| b.is_ascii_digit()) => {
            format!("{base}=s{SIZE}")
        }
        _ => url.to_string(),
    }
}

pub(super) async fn fetch(
    opts: &FetchOptions,
    book: &str,
    found: &RunProblems,
    summary: &mut FetchSummary,
) -> Result<()> {
    let db = &opts.db;
    let links: HashMap<String, String> = db.google_photos_listed(book).await?.into_iter().collect();
    // A photo has no version: its link names it.
    let mut listed: Vec<Listed> = links
        .keys()
        .map(|key| Listed::new(key, None::<String>))
        .collect();
    listed.sort_by(|a, b| a.key.cmp(&b.key));
    let owed = owed::owed(db.pool(), PHOTOS, listed).await?;
    if owed.is_empty() {
        return Ok(());
    }
    opts.progress.set_message("fetching photos");
    let photos = Photos {
        db,
        links,
        max_bytes: opts.blob_size_limit_bytes.unwrap_or(DEFAULT_MAX_BYTES),
        requests: AtomicUsize::new(0),
    };
    let l = Loop {
        pool: db.pool(),
        table: PHOTOS,
        phase: "photos",
        stop: &opts.control.stop,
        found,
        sealer: opts.sealer.as_ref(),
        batch: 1,
        concurrency: CONCURRENCY,
        flush: FLUSH,
        flush_bytes: FLUSH_BYTES,
        failures_in_a_row: FAILURES_IN_A_ROW,
    };
    let drained = owed::drain(&l, owed, &photos).await?;
    summary.photos += drained.got;
    summary.errors += drained.failed;
    summary.requests += photos.requests.load(Ordering::Relaxed);
    Ok(())
}

struct Photos<'a> {
    db: &'a RawDb,
    /// Each listed photo's link, by its key.
    links: HashMap<String, String>,
    max_bytes: u64,
    requests: AtomicUsize,
}

struct Photo {
    bytes: Vec<u8>,
    content_type: Option<String>,
}

#[async_trait]
impl Fetcher<Photo> for Photos<'_> {
    async fn fetch(
        &self,
        batch: Vec<Listed>,
    ) -> std::result::Result<Vec<Fetched<Photo>>, BatchError> {
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let outcome = self.one(&listed.key).await?;
            out.push(Fetched { listed, outcome });
        }
        Ok(out)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<Photo>],
    ) -> Result<()> {
        let got = batch.iter().filter_map(|f| match &f.outcome {
            Outcome::Got(photo) => Some(CasInsert {
                id: f.listed.key.as_str(),
                bytes: &photo.bytes,
                content_type: photo.content_type.as_deref(),
            }),
            _ => None,
        });
        let cas = self.db.cas().expect("a download's handle has a blob store");
        for (key, blake3) in cas.put_many(got.collect()).await? {
            RawDb::set_google_photo(tx, key, &blake3).await?;
        }
        Ok(())
    }

    fn weight(&self, photo: &Photo) -> usize {
        photo.bytes.len()
    }
}

impl Photos<'_> {
    async fn one(&self, key: &str) -> std::result::Result<Outcome<Photo>, BatchError> {
        let Some(url) = self.links.get(key).filter(|u| is_googleusercontent(u)) else {
            return Ok(Outcome::Failed(
                "the photo's link is not an https googleusercontent.com one".into(),
            ));
        };
        self.requests.fetch_add(1, Ordering::Relaxed);
        let req = HttpRequest::get(API.service, sized(url))
            .plain()
            .max_bytes(self.max_bytes);
        match latchkey_curl(&req).await {
            Ok(resp) => Ok(read(resp)),
            Err(e @ HttpError::TooLarge { .. }) => Ok(Outcome::Skipped(
                datalib_problems::Reason::OverSizeLimit,
                e.to_string(),
            )),
            Err(e @ HttpError::GaveUp { .. }) => Err(BatchError::Terminal(e.into())),
            Err(e) => Err(BatchError::Batch(e.into())),
        }
    }
}

fn read(resp: HttpResponse) -> Outcome<Photo> {
    let content_type = resp.header("content-type").map(str::to_string);
    if resp.status != 200 {
        return Outcome::Failed(format!("HTTP {}", resp.status));
    }
    if !content_type
        .as_deref()
        .is_some_and(|t| t.starts_with("image/"))
    {
        return Outcome::Failed(format!(
            "the link answered {}, not an image",
            content_type.as_deref().unwrap_or("no type")
        ));
    }
    Outcome::Got(Photo {
        bytes: resp.body,
        content_type,
    })
}
