//! `datalib-applet photos` — a photo's details and its pictures, by the
//! uuid of its grid row. Details are the `NormalizedPhoto` its render
//! stored in the grid index (`source_photos`); pictures are the source's
//! own previews, looked up when asked, else the original when a browser
//! can draw it, and the answer says which. It never reads a source's raw
//! store. `docs/dev/plans/photos_applet.md` is the design.

mod pictures;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    body::Body,
    extract::{Path as UrlPath, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware,
    response::{IntoResponse, Json, Response},
    routing::get,
    Router,
};
use datalib_photo_schema::{NormalizedPhoto, Pictures};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use pictures::{Level, Size};

const DATA_ROOT_ENV: &str = datalib_env::DATALIB_DAG_DATA_ROOT;
const COMPONENT_JS: &str = include_str!("component.js");
const COMPONENT_NAME: &str = "photos";
/// Which of a header's values says where a picture came from.
const IMAGE_FROM: &str = "x-datalib-image-from";
const PAGE: i64 = 60;

#[derive(Serialize)]
struct ComponentMeta {
    title: String,
    description: String,
    component_hash: String,
    component_args: Vec<String>,
    icon: &'static str,
    /// The kinds of name this applet answers for: the UI draws a
    /// `datalib:photo/…` from this applet's routes.
    serves: Vec<&'static str>,
}

pub fn write_frontend(dir: &Path) -> Result<()> {
    let namespace = dir
        .file_name()
        .and_then(|s| s.to_str())
        .context("--frontend-dir needs a path whose last segment is the namespace")?
        .to_string();
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let hash = sha256_hex(COMPONENT_JS.as_bytes());
    let js = dir.join(format!("{hash}.js"));
    if !js.exists() {
        datalib_runtime::atomic::write(&js, COMPONENT_JS.as_bytes())
            .with_context(|| format!("write {}", js.display()))?;
    }
    let meta = ComponentMeta {
        title: "Photos".to_string(),
        description: "Photos and videos, newest taken first; open one for its details.".to_string(),
        component_hash: hash,
        component_args: vec![namespace],
        icon: "photo",
        serves: vec!["photo"],
    };
    let meta_path = dir.join(format!("{COMPONENT_NAME}.json"));
    std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)
        .with_context(|| format!("write {}", meta_path.display()))?;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn serve(port: u16) -> Result<()> {
    let root = std::env::var_os(DATA_ROOT_ENV)
        .map(PathBuf::from)
        .with_context(|| {
            format!(
                "{DATA_ROOT_ENV} is not set. The gateway sets it to the data root; \
                 to run this by hand, set it yourself"
            )
        })?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    rt.block_on(async move {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind {addr}"))?;
        let bound = listener.local_addr().context("read the bound address")?;
        let gate = Arc::new(crate::gate::Gate::from_env(bound.port())?);
        let app = routes(Arc::new(root)).layer(middleware::from_fn_with_state(
            gate,
            crate::unified_document_index::require_gateway,
        ));
        tracing::info!(address = %bound, "listening");
        crate::announce_port(bound.port());
        axum::serve(listener, app).await.context("serve")
    })
}

fn routes(root: Arc<PathBuf>) -> Router {
    Router::new()
        .route("/photos", get(list))
        .route("/photo/{uuid}", get(one))
        .route("/photo/{uuid}/image", get(image))
        .route("/photo/{uuid}/original", get(original))
        .route("/health", get(|| async { Json(json!({"ok": true})) }))
        .with_state(root)
}

type AppState = State<Arc<PathBuf>>;

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

fn ours(e: anyhow::Error) -> ApiError {
    let msg = format!("{e:#}");
    tracing::error!(error = %msg, "photos");
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, msg)
}

fn not_found(why: impl Into<String>) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, why.into())
}

// ── The index ────────────────────────────────────────────────────────────

/// The grid index, read-only, or `None` before the first `grid_index`.
async fn open_index(root: &Path) -> Result<Option<SqlitePool>> {
    let db = datalib_runtime::layout::grid_index_db(root);
    if !db.exists() {
        return Ok(None);
    }
    Ok(Some(datalib_pin::open_reader(&db).await.with_context(
        || format!("open the grid index {}", db.display()),
    )?))
}

async fn find(root: &Path, uuid: &str) -> Result<Option<NormalizedPhoto>> {
    let Some(pool) = open_index(root).await? else {
        return Ok(None);
    };
    let json: Result<Option<String>, sqlx::Error> =
        sqlx::query_scalar("SELECT photo_json FROM source_photos WHERE photo_uuid = ? LIMIT 1")
            .bind(uuid)
            .fetch_optional(&pool)
            .await;
    pool.close().await;
    let json = match json {
        Ok(json) => json,
        Err(e) if datalib_pin::is_missing_table(&e, "source_photos") => None,
        Err(e) => return Err(e).context("read source_photos"),
    };
    json.map(|j| serde_json::from_str(&j).context("a stored photo"))
        .transpose()
}

#[derive(Deserialize, Default)]
struct ListQuery {
    source_id: Option<String>,
    offset: Option<i64>,
}

#[derive(Serialize)]
struct Listed {
    uuid: String,
    source_id: String,
    title: String,
    captured_at: Option<String>,
    kind: &'static str,
}

/// Photos newest taken first, a page at a time: what the gallery card
/// draws as a contact sheet.
async fn list(
    State(root): AppState,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let offset = q.offset.unwrap_or(0).max(0);
    let Some(pool) = open_index(&root).await.map_err(ours)? else {
        return Ok(Json(json!({ "photos": [], "next_offset": null })));
    };
    let rows = sqlx::query(
        "SELECT sp.photo_uuid, sp.photo_json FROM source_photos sp \
           JOIN grid_rows g ON g.uuid = sp.photo_uuid \
          WHERE ?1 IS NULL OR sp.source_id = ?1 \
          ORDER BY g.touched_at_utc DESC, sp.photo_uuid \
          LIMIT ?2 OFFSET ?3",
    )
    .bind(q.source_id.as_deref())
    .bind(PAGE + 1)
    .bind(offset)
    .fetch_all(&pool)
    .await;
    pool.close().await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) if datalib_pin::is_missing_table(&e, "source_photos") => Vec::new(),
        Err(e) => return Err(ours(anyhow::Error::new(e).context("list source_photos"))),
    };
    let more = rows.len() as i64 > PAGE;
    let photos: Vec<NormalizedPhoto> = rows
        .iter()
        .take(PAGE as usize)
        .map(|r| {
            serde_json::from_str(r.get("photo_json")).with_context(|| {
                let uuid: String = r.get("photo_uuid");
                format!("the stored photo {uuid} does not read")
            })
        })
        .collect::<Result<_>>()
        .map_err(ours)?;
    let photos: Vec<Listed> = photos
        .into_iter()
        .map(|p| Listed {
            title: p
                .title
                .clone()
                .or(p.headline.clone())
                .unwrap_or_else(|| p.file_name.clone()),
            uuid: p.uuid,
            source_id: p.source_id,
            captured_at: p.captured_at,
            kind: p.kind.as_str(),
        })
        .collect();
    Ok(Json(json!({
        "photos": photos,
        "next_offset": more.then_some(offset + PAGE),
    })))
}

/// What the card draws: the photo, which preview sizes its current
/// render has, and whether its original is there to fall back to.
async fn one(
    State(root): AppState,
    UrlPath(uuid): UrlPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let photo = find(&root, &uuid)
        .await
        .map_err(ours)?
        .ok_or_else(|| not_found(format!("no photo {uuid}")))?;
    let render = current_render(&photo).await.map_err(ours)?;
    let original = original_file(&photo).await;
    Ok(Json(json!({
        "photo": photo,
        "previews": render.as_ref().map(|r| json!({
            "long_edges": r.levels.iter().map(|l| l.long_edge).collect::<Vec<_>>(),
        })),
        "original": original.as_ref().map(|(path, content_type)| json!({
            "path": path,
            "content_type": content_type,
            "drawable": content_type.is_some_and(pictures::drawable),
        })),
    })))
}

// ── The pictures ─────────────────────────────────────────────────────────

struct Render {
    dir: PathBuf,
    uuid: String,
    digest: String,
    levels: Vec<Level>,
    /// How to turn the stored pixels to show them, Lightroom's code.
    orientation: Option<String>,
}

/// The render Lightroom's `previews.db` says is current for this photo,
/// or `None` when there are no previews to read.
async fn current_render(photo: &NormalizedPhoto) -> Result<Option<Render>> {
    let Pictures::Lightroom {
        catalog: Some(catalog),
        image_id_local,
        ..
    } = &photo.pictures
    else {
        return Ok(None);
    };
    let Some(dir) = pictures::previews_dir(Path::new(catalog)) else {
        return Ok(None);
    };
    let db = dir.join("previews.db");
    if !db.exists() {
        return Ok(None);
    }
    let opts = datalib_sqlite_open::file(&db)
        .read_only(true)
        .create_if_missing(false)
        .busy_timeout(std::time::Duration::from_secs(2));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .with_context(|| format!("open {}", db.display()))?;
    let read = async {
        let entry = sqlx::query(
            "SELECT CAST(uuid AS TEXT) AS uuid, CAST(digest AS TEXT) AS digest, \
                    CAST(orientation AS TEXT) AS orientation \
               FROM ImageCacheEntry WHERE CAST(imageId AS INTEGER) = ?",
        )
        .bind(image_id_local)
        .fetch_optional(&pool)
        .await
        .context("read ImageCacheEntry")?;
        let Some(entry) = entry else {
            return Ok(None);
        };
        let (uuid, digest): (String, String) = (entry.get("uuid"), entry.get("digest"));
        let orientation: Option<String> = entry.get("orientation");
        let levels = sqlx::query(
            "SELECT CAST(level AS INTEGER) AS level, \
                    CAST(longDimension AS INTEGER) AS long_edge \
               FROM PyramidLevel WHERE uuid = ? AND digest = ? ORDER BY level",
        )
        .bind(&uuid)
        .bind(&digest)
        .fetch_all(&pool)
        .await
        .context("read PyramidLevel")?
        .into_iter()
        .map(|r| Level {
            level: r.get("level"),
            long_edge: r.get("long_edge"),
        })
        .collect();
        Ok::<_, anyhow::Error>(Some(Render {
            dir: dir.clone(),
            uuid,
            digest,
            levels,
            orientation,
        }))
    }
    .await;
    pool.close().await;
    read
}

/// One size of a render: the loose JPEG of a newer catalog, else the
/// level inside an older catalog's `.lrprev`.
async fn preview_bytes(render: &Render, level: Level) -> Result<Option<Vec<u8>>> {
    if let Some(path) =
        pictures::loose_file(&render.dir, &render.uuid, &render.digest, level.long_edge)
    {
        if let Ok(bytes) = tokio::fs::read(&path).await {
            return Ok(Some(bytes));
        }
    }
    let Some(path) = pictures::lrprev_file(&render.dir, &render.uuid, &render.digest) else {
        return Ok(None);
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => Ok(pictures::lrprev_level(&bytes, level.level).map(<[u8]>::to_vec)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// The original that is on disk, with the type its bytes say.
async fn original_file(photo: &NormalizedPhoto) -> Option<(PathBuf, Option<&'static str>)> {
    let Pictures::Lightroom { catalog, .. } = &photo.pictures;
    let candidates = pictures::original_candidates(
        photo.original.absolute.as_deref(),
        photo.original.relative_to_catalog.as_deref(),
        catalog.as_deref().map(Path::new),
    );
    for path in candidates {
        if let Ok(mut file) = tokio::fs::File::open(&path).await {
            use tokio::io::AsyncReadExt;
            let mut head = [0u8; 16];
            let n = file.read(&mut head).await.unwrap_or(0);
            return Some((path, pictures::sniff(&head[..n])));
        }
    }
    None
}

#[derive(Deserialize)]
struct ImageQuery {
    size: Size,
}

async fn image(
    State(root): AppState,
    UrlPath(uuid): UrlPath<String>,
    Query(q): Query<ImageQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let photo = find(&root, &uuid)
        .await
        .map_err(ours)?
        .ok_or_else(|| not_found(format!("no photo {uuid}")))?;
    if let Some(render) = current_render(&photo).await.map_err(ours)? {
        if let Some(level) = pictures::pick(&render.levels, q.size) {
            if let Some(bytes) = preview_bytes(&render, level).await.map_err(ours)? {
                let code = render.orientation.as_deref().unwrap_or("AB");
                let etag = format!("\"{}-{}-{code}\"", render.digest, level.long_edge);
                let bytes = turned(bytes, code, &uuid);
                return Ok(picture(bytes, "preview", &etag, &headers));
            }
        }
    }
    let why_no_preview = "Lightroom has no preview of it";
    match original_file(&photo).await {
        Some((path, Some(ct))) if ct.starts_with("image/") && pictures::drawable(ct) => {
            tracing::warn!(photo = %uuid, path = %path.display(), "no preview; serving the original");
            let bytes = tokio::fs::read(&path)
                .await
                .with_context(|| format!("read {}", path.display()))
                .map_err(ours)?;
            let etag = format!("\"{}\"", sha256_hex(&bytes));
            Ok(picture(bytes, "original", &etag, &headers))
        }
        Some((path, ct)) => Err(not_found(format!(
            "{why_no_preview}, and its original {} is {}, which a browser does not draw",
            path.display(),
            ct.unwrap_or("of a type this does not know")
        ))),
        None => Err(not_found(format!(
            "{why_no_preview}, and its original is not on this computer"
        ))),
    }
}

/// A preview as Lightroom shows it: its stored pixels, told how to turn.
fn turned(bytes: Vec<u8>, code: &str, uuid: &str) -> Vec<u8> {
    let Some(orientation) = pictures::exif_orientation(code) else {
        tracing::warn!(photo = %uuid, code, "unknown Lightroom orientation; serving the preview unturned");
        return bytes;
    };
    if orientation == 1 {
        return bytes;
    }
    match pictures::with_orientation(&bytes, orientation) {
        Some(out) => out,
        None => {
            tracing::warn!(photo = %uuid, code, "preview is not a JPEG this can turn; serving it unturned");
            bytes
        }
    }
}

/// A picture's bytes, typed by what they are. Not cached as immutable:
/// an edit in Lightroom changes the picture at the same address.
fn picture(bytes: Vec<u8>, from: &'static str, etag: &str, headers: &HeaderMap) -> Response {
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag);
    let mut builder = Response::builder()
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CACHE_CONTROL, "private, no-cache")
        .header(header::ETAG, etag)
        .header(IMAGE_FROM, from);
    if fresh {
        builder = builder.status(StatusCode::NOT_MODIFIED);
        return builder.body(Body::empty()).unwrap_or_default();
    }
    let content_type = pictures::sniff(&bytes).unwrap_or("application/octet-stream");
    builder
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(bytes))
        .unwrap_or_default()
}

/// The original file, a byte range at a time when asked, so a video can
/// play and seek. Only a type a browser draws is sent.
async fn original(
    State(root): AppState,
    UrlPath(uuid): UrlPath<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let photo = find(&root, &uuid)
        .await
        .map_err(ours)?
        .ok_or_else(|| not_found(format!("no photo {uuid}")))?;
    let Some((path, content_type)) = original_file(&photo).await else {
        return Err(not_found("its original is not on this computer"));
    };
    let Some(content_type) = content_type.filter(|ct| pictures::drawable(ct)) else {
        return Err(ApiError(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("{} is not a type a browser draws", path.display()),
        ));
    };
    let file = tokio::fs::File::open(&path)
        .await
        .with_context(|| format!("open {}", path.display()))
        .map_err(ours)?;
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let range = pictures::byte_range(
        headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
        len,
    );
    let (start, end) = range.unwrap_or((0, len.saturating_sub(1)));
    let body = Body::from_stream(chunks(file, start, end + 1 - start));
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CACHE_CONTROL, "private, no-cache")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(
            header::CONTENT_LENGTH,
            if len == 0 { 0 } else { end + 1 - start },
        )
        .header(IMAGE_FROM, "original");
    if range.is_some() {
        builder = builder.status(StatusCode::PARTIAL_CONTENT).header(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{len}"))
                .map_err(|e| ours(anyhow::anyhow!("a Content-Range header: {e}")))?,
        );
    }
    builder
        .body(body)
        .map_err(|e| ours(anyhow::anyhow!("build the response: {e}")))
}

/// `len` bytes of `file` from `start`, read 256 KiB at a time.
fn chunks(
    file: tokio::fs::File,
    start: u64,
    len: u64,
) -> impl futures::Stream<Item = std::io::Result<Vec<u8>>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    futures::stream::unfold(
        (file, start, len, false),
        |(mut file, at, left, seeked)| async move {
            if left == 0 {
                return None;
            }
            if !seeked {
                if let Err(e) = file.seek(std::io::SeekFrom::Start(at)).await {
                    return Some((Err(e), (file, at, 0, true)));
                }
            }
            let mut buf = vec![0u8; left.min(256 * 1024) as usize];
            match file.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok(buf), (file, at + n as u64, left - n as u64, true)))
                }
                Err(e) => Some((Err(e), (file, at, 0, true))),
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ten_forward() -> NormalizedPhoto {
        use datalib_photo_schema::{Exposure, MediaKind, Original, Place};
        NormalizedPhoto {
            uuid: "ten-forward".into(),
            source_id: "lr".into(),
            kind: MediaKind::Photo,
            title: Some("Ten Forward".into()),
            headline: None,
            caption: None,
            copyright: None,
            creator: None,
            captured_at: Some("2369-04-01T20:00:00-08:00".into()),
            offset_from: None,
            edited_at_utc: None,
            camera: None,
            lens: None,
            exposure: Exposure::default(),
            rating: None,
            pick: None,
            color_label: None,
            keywords: Vec::new(),
            people: Vec::new(),
            albums: Vec::new(),
            place: Place::default(),
            geo: None,
            h3: Vec::new(),
            width: None,
            height: None,
            duration_seconds: None,
            file_name: "ten_forward.jpg".into(),
            folder: None,
            original: Original::default(),
            pictures: Pictures::Lightroom {
                catalog: None,
                image_id_local: 1,
                file_uuid: "F1".into(),
            },
        }
    }

    /// A grid index holding only what `list` reads, with one more photo
    /// in it each call.
    async fn index_photo(root: &Path, uuid: &str, photo_json: &str, touched: &str) {
        let db = datalib_runtime::layout::grid_index_db(root);
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let opts = datalib_sqlite_open::file(&db).create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .unwrap();
        for (_, ddl) in datalib_schema::source_photos::DDL {
            sqlx::query(*ddl).execute(&pool).await.unwrap();
        }
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS grid_rows (uuid TEXT PRIMARY KEY, touched_at_utc TEXT)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO source_photos VALUES (?1, ?1, 'lr', ?2)")
            .bind(uuid)
            .bind(photo_json)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO grid_rows VALUES (?, ?)")
            .bind(uuid)
            .bind(touched)
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }

    async fn list_all(root: &Path) -> Result<serde_json::Value, ApiError> {
        let listed = list(
            State(Arc::new(root.to_path_buf())),
            Query(ListQuery::default()),
        )
        .await;
        listed.map(|Json(body)| body)
    }

    /// A photo a newer build wrote, with a kind this one does not know,
    /// left the gallery as if the photo were not there.
    #[tokio::test]
    async fn a_photo_that_does_not_read_fails_the_list() {
        let root = tempfile::tempdir().unwrap();
        let good = serde_json::to_string(&ten_forward()).unwrap();
        index_photo(root.path(), "ten-forward", &good, "2369-04-01T00:00:00Z").await;
        let Ok(body) = list_all(root.path()).await else {
            panic!("a readable photo failed the list");
        };
        assert_eq!(body["photos"][0]["uuid"], "ten-forward");
        assert_eq!(body["photos"][0]["title"], "Ten Forward");

        let bad = r#"{"uuid": "holodeck-3", "kind": "hologram"}"#;
        index_photo(root.path(), "holodeck-3", bad, "2369-04-02T00:00:00Z").await;
        match list_all(root.path()).await {
            Ok(body) => panic!("listed as {body}"),
            Err(ApiError(status, msg)) => {
                assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
                assert!(msg.contains("holodeck-3"), "{msg}");
            }
        }
    }
}
