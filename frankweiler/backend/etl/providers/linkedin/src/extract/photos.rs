//! Fetch each connection's profile photo and store it in the per-source
//! CAS, mapped by a `contact_photos` edge row.
//!
//! Shape (kept consistent with the contacts provider, even though the
//! code isn't shared — this is raw data, owned per-provider):
//!
//! ```sql
//! CREATE TABLE contact_photos (
//!     id        TEXT PRIMARY KEY,   -- "{owner_id}#{source_url}"
//!     owner_id  TEXT NOT NULL,      -- the raw entity id (= connection_uuid)
//!     source_url TEXT NOT NULL,     -- where the bytes came from (og:image URL)
//!     blake3    TEXT NULL           -- CAS key; NULL = attempted, no photo
//! )
//! ```
//!
//! Bytes live in the sibling `<name>.blobs.doltlite_db` CAS keyed by
//! blake3. The render side joins `contact_photos` → `cas_objects` to
//! materialize the image next to the contact's markdown.
//!
//! ## Fetch path
//!
//! LinkedIn profile URLs are HTML pages, not images. We GET the public
//! profile page, scrape its `og:image` meta tag, then GET that image —
//! both via the shared curl chokepoint in **plain** mode
//! ([`HttpRequest::plain`]): these are public, auth-free resources with
//! no latchkey service, and we still get the chokepoint's retry/backoff
//! and playback support.
//!
//! ## Idempotence & retry
//!
//! We only fetch for a connection that has **no** `contact_photos` row
//! yet, and we persist a row only for *settled* outcomes:
//!
//!   * **success** — bytes stored, `blake3` set;
//!   * **no public photo** — the profile page loaded (2xx) but advertised
//!     no `og:image`; recorded with `blake3 = NULL` so we don't re-hammer
//!     a connection that genuinely has no picture.
//!
//! A **transient** failure (LinkedIn's `HTTP 999` bot-block, a 429/5xx, a
//! network error, or an image fetch that didn't return bytes) records
//! *nothing* — so the next `fetch_photos` run retries that connection.
//! That's what lets a rate-limited bulk run finish filling in photos over
//! subsequent syncs, while a fully-fetched connection is still never
//! re-fetched.

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{cas_path_for, BlobCas};
use frankweiler_etl::http::{latchkey_curl, HttpRequest};
use frankweiler_etl::progress::Progress;
use serde::Serialize;
use serde_json::Value;
use sqlx::Row;

use super::schema_raw::connection_uuid;
use super::RawDb;

/// The shared contact→photo edge table name (same in the contacts
/// provider). Lives in the entity raw store; bytes live in the CAS.
pub const CONTACT_PHOTOS_TABLE: &str = "contact_photos";

/// Browser User-Agent for the photo fetch. LinkedIn serves an `HTTP 999`
/// bot-block (no body, no `og:image`) to requests with curl's default
/// UA, but returns the real public profile page — with the member's
/// `profile-displayphoto` `og:image` — to a browser-shaped UA. No auth
/// required. Kept here (not inline) so the synthesizer builds the exact
/// same request, and thus the same playback [`fixture_key`].
const PHOTO_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// The canonical request for fetching a LinkedIn photo URL (profile page
/// or image): plain curl (no latchkey — these are public), with a
/// browser User-Agent. Both extract and [`crate::synthesize`] build
/// requests through this so playback keys match.
pub fn photo_request(url: &str) -> HttpRequest {
    HttpRequest::get("linkedin", url)
        .plain()
        .header("User-Agent", PHOTO_UA)
}

const CONTACT_PHOTOS_DDL: &str = "CREATE TABLE IF NOT EXISTS contact_photos (
    id         TEXT PRIMARY KEY,
    owner_id   TEXT NOT NULL,
    source_url TEXT NOT NULL,
    blake3     TEXT NULL,
    CHECK (blake3 IS NULL OR length(blake3) = 64)
)";

const CONTACT_PHOTOS_BY_OWNER_DDL: &str =
    "CREATE INDEX IF NOT EXISTS contact_photos_by_owner ON contact_photos(owner_id)";

#[derive(Debug, Default, Clone, Serialize)]
pub struct PhotoSummary {
    /// Connections we attempted a fetch for this run (had a URL, no prior row).
    pub attempted: usize,
    /// Photos successfully stored in CAS this run.
    pub fetched: usize,
    /// Profiles that loaded but advertise no photo — recorded permanently
    /// (we won't retry a connection that genuinely has no picture).
    pub no_photo: usize,
    /// Transient failures (bot-block / rate-limit / network). NOT
    /// recorded, so the next run retries these.
    pub transient: usize,
}

/// Fetch-and-store photos for every connection that doesn't already have
/// a `contact_photos` row. `db_path` is the resolved entity db path (its
/// CAS sibling is derived via [`cas_path_for`]).
pub async fn fetch_connection_photos(
    db: &RawDb,
    db_path: &std::path::Path,
    progress: &Progress,
) -> Result<PhotoSummary> {
    // The connections table may be absent (user excluded it) — nothing
    // to do. load_payloads errors on a missing table, so treat that as
    // empty.
    let connections = db.load_payloads("connections").await.unwrap_or_default();
    if connections.is_empty() {
        return Ok(PhotoSummary::default());
    }

    let pool = db.pool();
    sqlx::query(CONTACT_PHOTOS_DDL)
        .execute(pool)
        .await
        .context("create contact_photos")?;
    sqlx::query(CONTACT_PHOTOS_BY_OWNER_DDL)
        .execute(pool)
        .await
        .context("index contact_photos")?;

    // Owners we've already attempted (success or miss) — skip them.
    let already: std::collections::HashSet<String> =
        sqlx::query("SELECT DISTINCT owner_id FROM contact_photos")
            .fetch_all(pool)
            .await
            .context("load existing contact_photos owners")?
            .into_iter()
            .map(|r| r.get::<String, _>("owner_id"))
            .collect();

    let cas = BlobCas::open(&cas_path_for(db_path))
        .await
        .context("open linkedin CAS")?;

    let mut summary = PhotoSummary::default();
    for p in &connections {
        let url = field(p, "URL");
        if url.is_empty() {
            continue;
        }
        let owner_id = connection_uuid(url);
        if already.contains(&owner_id) {
            continue;
        }
        summary.attempted += 1;
        progress.set_message(&format!("photo: {}", display_name(p)));

        match fetch_one(url).await {
            Outcome::Found(photo) => {
                let blake3 = cas
                    .put(&photo.bytes, photo.content_type.as_deref())
                    .await
                    .context("cas put connection photo")?;
                insert_edge(pool, &owner_id, &photo.source_url, Some(&blake3)).await?;
                summary.fetched += 1;
            }
            Outcome::NoPhoto => {
                // Settled: the page loaded but has no photo. Record it
                // (blake3 NULL, keyed on the profile URL) so we don't
                // re-hammer a connection that genuinely has no picture.
                insert_edge(pool, &owner_id, url, None).await?;
                summary.no_photo += 1;
            }
            Outcome::Transient => {
                // Bot-block / rate-limit / network. Record NOTHING so the
                // next run retries this connection.
                summary.transient += 1;
            }
        }
    }
    Ok(summary)
}

/// The settled-or-not result of one connection's photo fetch.
enum Outcome {
    /// Got image bytes.
    Found(FetchedPhoto),
    /// Profile loaded (2xx) but advertises no `og:image` — definitive.
    NoPhoto,
    /// Bot-block / rate-limit / network / empty image — retry next run.
    Transient,
}

/// Render-side: load every stored connection photo as
/// `owner_id (connection_uuid) → (bytes, content_type)`. Joins
/// `contact_photos` → `cas_objects`. Empty when photos were never
/// fetched (the table won't exist). Never fails on a missing table.
pub async fn load_photo_blobs(
    db: &RawDb,
    db_path: &std::path::Path,
) -> Result<std::collections::HashMap<String, (Vec<u8>, Option<String>)>> {
    let pool = db.pool();
    let table_exists: Option<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table' AND name=?")
            .bind(CONTACT_PHOTOS_TABLE)
            .fetch_optional(pool)
            .await
            .context("probe contact_photos")?;
    let mut out = std::collections::HashMap::new();
    if table_exists.is_none() {
        return Ok(out);
    }

    // owner_id → blake3 for the rows that actually have bytes.
    let edges = sqlx::query("SELECT owner_id, blake3 FROM contact_photos WHERE blake3 IS NOT NULL")
        .fetch_all(pool)
        .await
        .context("load contact_photos")?;
    if edges.is_empty() {
        return Ok(out);
    }
    let cas = BlobCas::open(&cas_path_for(db_path))
        .await
        .context("open linkedin CAS")?;
    for row in edges {
        let owner_id: String = row.get("owner_id");
        let blake3: String = row.get("blake3");
        if let Some((bytes, content_type)) = load_cas_bytes(&cas, &blake3).await? {
            out.insert(owner_id, (bytes, content_type));
        }
    }
    Ok(out)
}

async fn load_cas_bytes(cas: &BlobCas, blake3: &str) -> Result<Option<(Vec<u8>, Option<String>)>> {
    let row = sqlx::query("SELECT bytes, content_type FROM cas_objects WHERE blake3 = ?")
        .bind(blake3)
        .fetch_optional(cas.pool())
        .await
        .context("load cas bytes")?;
    Ok(row.map(|r| {
        (
            r.get::<Vec<u8>, _>("bytes"),
            r.get::<Option<String>, _>("content_type"),
        )
    }))
}

struct FetchedPhoto {
    source_url: String,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

/// GET the profile page, scrape `og:image`, GET the image. Classifies
/// the result so the caller knows whether to record it (settled) or
/// leave it for a retry (transient). Errors are folded into
/// [`Outcome::Transient`] — they're worth retrying, not propagating.
async fn fetch_one(profile_url: &str) -> Outcome {
    let page = match latchkey_curl(&photo_request(profile_url)).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(event = "linkedin_photo_page_failed", url = profile_url, error = %e);
            return Outcome::Transient;
        }
    };
    // Non-2xx is a bot-block (LinkedIn's 999), rate-limit, or 5xx — all
    // worth retrying on a later run.
    if !(200..300).contains(&page.status) {
        return Outcome::Transient;
    }
    let Some(img_url) = extract_og_image(&page.body_str()) else {
        // The page loaded cleanly but names no image: this connection
        // has no public photo. Settled — don't keep retrying.
        return Outcome::NoPhoto;
    };
    let img = match latchkey_curl(&photo_request(&img_url)).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(event = "linkedin_photo_image_failed", url = %img_url, error = %e);
            return Outcome::Transient;
        }
    };
    if !(200..300).contains(&img.status) || img.body.is_empty() {
        return Outcome::Transient;
    }
    Outcome::Found(FetchedPhoto {
        source_url: img_url,
        content_type: img.header("content-type").map(str::to_string),
        bytes: img.body,
    })
}

/// Pull the first `og:image` (or `og:image:secure_url` / `twitter:image`)
/// URL out of an HTML head. Deliberately tiny and forgiving — we scan for
/// a `<meta>` whose property/name is one of those and read its `content`,
/// tolerating either attribute order.
fn extract_og_image(html: &str) -> Option<String> {
    const KEYS: &[&str] = &["og:image:secure_url", "og:image", "twitter:image"];
    let lower = html.to_lowercase();
    for key in KEYS {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(&format!("\"{key}\"")) {
            let idx = from + rel;
            // Search the enclosing tag (back to '<', forward to '>') for content="…".
            let tag_start = lower[..idx].rfind('<').unwrap_or(idx);
            let tag_end = lower[idx..]
                .find('>')
                .map(|e| idx + e)
                .unwrap_or(html.len());
            if let Some(content) = attr_value(&html[tag_start..tag_end], "content") {
                let v = content.trim();
                if v.starts_with("http://") || v.starts_with("https://") {
                    // og:image content is HTML-escaped (`&amp;` in the
                    // signed media URL's query string); decode the few
                    // entities that actually appear so the fetch URL is
                    // valid.
                    return Some(html_unescape(v));
                }
            }
            from = tag_end.max(idx + 1);
        }
    }
    None
}

/// Decode the handful of HTML entities that appear in og:image URLs.
fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#38;", "&")
        .replace("&#x26;", "&")
}

/// Read `name="value"` (or `name='value'`) out of a tag fragment,
/// case-insensitive on the attribute name.
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_lowercase();
    let needle = format!("{name}=");
    let mut from = 0;
    while let Some(rel) = lower[from..].find(&needle) {
        let after = from + rel + needle.len();
        let rest = &tag[after..];
        let quote = rest.chars().next()?;
        if quote == '"' || quote == '\'' {
            let body = &rest[1..];
            if let Some(end) = body.find(quote) {
                return Some(body[..end].to_string());
            }
        }
        from = after;
    }
    None
}

async fn insert_edge(
    pool: &sqlx::SqlitePool,
    owner_id: &str,
    source_url: &str,
    blake3: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO contact_photos (id, owner_id, source_url, blake3) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(format!("{owner_id}#{source_url}"))
    .bind(owner_id)
    .bind(source_url)
    .bind(blake3)
    .execute(pool)
    .await
    .context("insert contact_photos row")?;
    Ok(())
}

fn field<'a>(p: &'a Value, key: &str) -> &'a str {
    p.get(key).and_then(Value::as_str).unwrap_or("").trim()
}

fn display_name(p: &Value) -> String {
    format!("{} {}", field(p, "First Name"), field(p, "Last Name"))
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_og_image_either_attr_order() {
        let html = r#"<html><head>
            <meta property="og:image" content="https://media.example/pic.jpg" />
            </head></html>"#;
        assert_eq!(
            extract_og_image(html).as_deref(),
            Some("https://media.example/pic.jpg")
        );
        // content before property
        let html2 = r#"<meta content='https://x/y.png' property="og:image">"#;
        assert_eq!(extract_og_image(html2).as_deref(), Some("https://x/y.png"));
        // secure_url preferred key also works
        let html3 = r#"<meta property="og:image:secure_url" content="https://s/p.jpg">"#;
        assert_eq!(extract_og_image(html3).as_deref(), Some("https://s/p.jpg"));
        // `&amp;` in the signed media URL's query string is decoded.
        let html4 = r#"<meta property="og:image" content="https://media.licdn.com/x?e=1&amp;v=beta&amp;t=zz">"#;
        assert_eq!(
            extract_og_image(html4).as_deref(),
            Some("https://media.licdn.com/x?e=1&v=beta&t=zz")
        );
        // no og:image
        assert_eq!(extract_og_image("<html></html>"), None);
        // non-http content is ignored
        assert_eq!(
            extract_og_image(r#"<meta property="og:image" content="data:foo">"#),
            None
        );
    }
}
