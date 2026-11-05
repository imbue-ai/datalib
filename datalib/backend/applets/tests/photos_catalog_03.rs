//! The photos applet as the gateway runs it, drawing catalog 03 of
//! lightroom_db_diff: the real binary, a grid index holding three photos
//! that point at the catalog, and the bytes it answers checked against
//! the files Lightroom wrote.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use datalib_photo_schema::{Exposure, MediaKind, NormalizedPhoto, Original, Pictures, Place};

const SECRET: &str = "a-test-secret";
/// The Golden Gate photo's file, and the render `previews.db` names
/// current for it.
const FILE_UUID: &str = "90A53874-ACC1-45EE-A843-81BFFE893F0A";
const DIGEST: &str = "3219a2e16918837c891ac13b27c31f04";

fn catalog() -> PathBuf {
    let p = std::path::absolute(std::env::var("LIGHTROOM_CATALOG_03").unwrap()).unwrap();
    assert!(p.exists(), "{}", p.display());
    p
}

fn previews() -> PathBuf {
    catalog().with_file_name("test_catalog_more_face_tags_gps_edit Previews.lrdata")
}

fn photo(uuid: &str, id_local: i64, original: &str) -> NormalizedPhoto {
    NormalizedPhoto {
        uuid: uuid.into(),
        source_id: "lr".into(),
        kind: MediaKind::Photo,
        title: Some(format!("photo {uuid}")),
        headline: None,
        caption: None,
        copyright: None,
        creator: None,
        captured_at: Some("2007-01-01T16:41:39-08:00".into()),
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
        file_name: original.rsplit('/').next().unwrap().into(),
        folder: None,
        original: Original {
            absolute: Some(format!("/nowhere/{original}")),
            relative_to_catalog: Some(format!("../../test_photos/{original}")),
        },
        pictures: Pictures::Lightroom {
            catalog: Some(catalog().display().to_string()),
            image_id_local: id_local,
            file_uuid: FILE_UUID.into(),
        },
    }
}

/// A grid index holding only what the applet reads: the photos, and the
/// order of their rows.
async fn index(root: &Path, photos: &[NormalizedPhoto]) {
    let db = datalib_runtime::layout::grid_index_db(root);
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&db)
        .create_if_missing(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .unwrap();
    for (_, ddl) in datalib_schema::source_photos::DDL {
        sqlx::query(*ddl).execute(&pool).await.unwrap();
    }
    sqlx::query("CREATE TABLE grid_rows (uuid TEXT PRIMARY KEY, touched_at_utc TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    for (i, p) in photos.iter().enumerate() {
        sqlx::query("INSERT INTO source_photos VALUES (?, ?, ?, ?)")
            .bind(&p.uuid)
            .bind(&p.uuid)
            .bind(&p.source_id)
            .bind(serde_json::to_string(p).unwrap())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO grid_rows VALUES (?, ?)")
            .bind(&p.uuid)
            .bind(format!("2007-01-0{}T00:00:00+00:00", i + 1))
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
}

fn start(root: &Path) -> (Child, u16) {
    let bin = std::fs::canonicalize(std::env::var("APPLET_BIN").unwrap()).unwrap();
    let secret_file = root.join("system/applet-secret");
    datalib_env::secret_file::write(&secret_file, SECRET).unwrap();
    let mut child = Command::new(bin)
        .args(["photos", "-p", "0"])
        .env(datalib_env::DATALIB_APPLET_SECRET_FILE, &secret_file)
        .env("DATALIB_DAG_DATA_ROOT", root)
        .env("DATALIB_PARENT_PIPE", "0")
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start the applet");
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(port) = line.strip_prefix("DATALIB_APPLET_PORT=") {
                let _ = tx.send(port.parse::<u16>().unwrap());
            }
        }
    });
    let port = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the applet never announced a port");
    (child, port)
}

struct Answer {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

fn get(port: u16, path: &str, extra: &str) -> Answer {
    let mut conn = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        conn,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-datalib-applet-secret: {SECRET}\r\n\
         {extra}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    conn.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8(raw[..split].to_vec()).unwrap();
    Answer {
        status: head[9..12].parse().unwrap(),
        head,
        body: raw[split + 4..].to_vec(),
    }
}

fn preview_file(long_edge: u32) -> Vec<u8> {
    let dir = previews().join("9").join("90A5");
    std::fs::read(dir.join(format!("{FILE_UUID}-{DIGEST}_{long_edge}"))).unwrap()
}

/// A thumbnail and a full-size picture come from the render Lightroom's
/// `previews.db` names current, never an older one beside it; a photo
/// with no preview falls back to its original, found beside the catalog,
/// and says so; one with neither is a 404 that says why.
#[tokio::test(flavor = "multi_thread")]
async fn pictures_come_from_the_current_preview_else_the_original() {
    assert!(previews().join("previews.db").exists());
    let root = tempfile::tempdir().unwrap();
    index(
        root.path(),
        &[
            photo("gg", 86, "GoldenGateBridge-001.jpg"),
            photo("no-preview", 9999, "Hermann_Hesse_2.jpg"),
            photo("nothing", 9998, "not-there.jpg"),
        ],
    )
    .await;
    let (mut child, port) = start(root.path());

    let thumb = get(port, "/photo/gg/image?size=thumb", "");
    assert_eq!(
        thumb.status,
        200,
        "{}",
        String::from_utf8_lossy(&thumb.body)
    );
    assert_eq!(thumb.header("x-datalib-image-from"), Some("preview"));
    assert_eq!(thumb.header("content-type"), Some("image/jpeg"));
    assert_eq!(thumb.body, preview_file(320));

    let full = get(port, "/photo/gg/image?size=full", "");
    assert_eq!(full.body, preview_file(3024));
    let etag = full.header("etag").unwrap().to_string();
    let again = get(
        port,
        "/photo/gg/image?size=full",
        &format!("If-None-Match: {etag}\r\n"),
    );
    assert_eq!(again.status, 304);

    let fallback = get(port, "/photo/no-preview/image?size=full", "");
    assert_eq!(fallback.status, 200);
    assert_eq!(fallback.header("x-datalib-image-from"), Some("original"));
    let original = catalog()
        .parent()
        .unwrap()
        .join("../../test_photos/Hermann_Hesse_2.jpg");
    assert_eq!(fallback.body, std::fs::read(&original).unwrap());

    let range = get(port, "/photo/no-preview/original", "Range: bytes=0-9\r\n");
    assert_eq!(range.status, 206);
    assert_eq!(range.body.len(), 10);
    assert!(range
        .header("content-range")
        .unwrap()
        .starts_with("bytes 0-9/"));

    let missing = get(port, "/photo/nothing/image?size=thumb", "");
    assert_eq!(missing.status, 404);
    assert!(missing.json()["error"]
        .as_str()
        .unwrap()
        .contains("not on this computer"));

    let details = get(port, "/photo/gg", "").json();
    assert_eq!(details["photo"]["uuid"], "gg");
    assert_eq!(
        details["previews"]["long_edges"],
        serde_json::json!([320, 756, 1512, 3024])
    );

    let listed = get(port, "/photos", "").json();
    let order: Vec<&str> = listed["photos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["uuid"].as_str().unwrap())
        .collect();
    assert_eq!(order, ["nothing", "no-preview", "gg"], "newest first");

    assert_eq!(get(port, "/photo/unknown", "").status, 404);
    drop(child.stdin.take());
    let _ = child.wait();
}
