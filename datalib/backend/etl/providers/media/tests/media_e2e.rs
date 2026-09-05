//! End-to-end over the fixture corpus: scan → store.
//!
//! Asserts against the raw store rather than against log lines, per
//! AGENTS.md §"Inspecting doltlite stores" — a log line says what the
//! code *said*, the store says what it *did*.
//!
//! The corpus is built around metadata-only variants (see
//! `//tests/fixtures/make_media_fixtures.py`), so most of what is
//! checked here is one claim in two directions: **`blake3` differs and
//! `payload_blake3` does not**. That pair is the provider's central
//! promise, and a test that only asserted the first half would pass
//! against a `payload_blake3` that was silently NULL everywhere.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::Result;
use sqlx::Row;

use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl_media::download::{self, RawDb};

const NOW: &str = "2364-04-13T08:45:00-07:00";
const STANZA: &str = "tng_media";

fn fixture_dir() -> PathBuf {
    let rel =
        std::env::var("MEDIA_FIXTURE_DIR").expect("MEDIA_FIXTURE_DIR must be set by the build");
    // Under `bazel test` the runfiles root is CWD; under `cargo test`
    // the env var is repo-relative from the workspace root.
    let p = PathBuf::from(&rel);
    if p.is_dir() {
        return p;
    }
    let up = PathBuf::from("../../../../..").join(&rel);
    assert!(
        up.is_dir(),
        "fixture dir not found: {rel} (cwd {:?})",
        std::env::current_dir()
    );
    up
}

struct Harness {
    _tmp: tempfile::TempDir,
    raw_dir: PathBuf,
    root: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let raw_dir = tmp.path().join("raw");
        std::fs::create_dir_all(&raw_dir).unwrap();
        Self {
            root: fixture_dir(),
            _tmp: tmp,
            raw_dir,
        }
    }

    async fn db(&self) -> Result<RawDb> {
        RawDb::open(&download::db_path_for(&self.raw_dir)).await
    }

    async fn scan(&self) -> Result<download::FetchSummary> {
        self.scan_with(|o| o).await
    }

    async fn scan_with<F>(&self, tweak: F) -> Result<download::FetchSummary>
    where
        F: FnOnce(download::FetchOptions) -> download::FetchOptions,
    {
        let db = self.db().await?;
        // A temp cache per harness: tests must never touch this host's
        // real one.
        let cache = FingerprintCache::open(&self.raw_dir.join("fingerprints.sqlite")).await?;
        download::fetch(tweak(download::FetchOptions {
            db,
            source_name: STANZA.to_string(),
            root: self.root.clone(),
            cache,
            ignore: vec![],
            max_bytes: None,
            payload_max_bytes: None,
            playlists: true,
            skip_dataless: true,
            force_rehash: false,
            now: NOW.to_string(),
            progress: datalib_etl::progress::Progress::noop(),
        }))
        .await
    }
}

/// `path -> blake3` for every indexed file.
async fn files(db: &RawDb) -> Result<HashMap<String, String>> {
    let rows = sqlx::query("SELECT id, blake3 FROM media_files")
        .fetch_all(db.pool())
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<String, _>("id"), r.get::<String, _>("blake3")))
        .collect())
}

/// `blake3 -> (payload_blake3, payload_scheme, class, container)`.
async fn items(
    db: &RawDb,
) -> Result<HashMap<String, (Option<String>, Option<String>, String, String)>> {
    let rows = sqlx::query(
        "SELECT blake3, payload_blake3, payload_scheme, media_class, container FROM media_items",
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.get::<String, _>("blake3"),
                (
                    r.get::<Option<String>, _>("payload_blake3"),
                    r.get::<Option<String>, _>("payload_scheme"),
                    r.get::<String, _>("media_class"),
                    r.get::<String, _>("container"),
                ),
            )
        })
        .collect())
}

/// The payload hash and scheme of the item at one path.
async fn payload_of(db: &RawDb, path: &str) -> Result<(Option<String>, Option<String>)> {
    let row = sqlx::query(
        "SELECT i.payload_blake3 AS p, i.payload_scheme AS s
           FROM media_files f JOIN media_items i ON i.blake3 = f.blake3
          WHERE f.id = ?",
    )
    .bind(path)
    .fetch_one(db.pool())
    .await?;
    Ok((row.get("p"), row.get("s")))
}

/// Assert that two paths hold different bytes but the same signal.
async fn assert_metadata_only_variant(db: &RawDb, a: &str, b: &str, scheme: &str) -> Result<()> {
    let f = files(db).await?;
    let (ha, hb) = (
        f.get(a).unwrap_or_else(|| panic!("{a} not indexed")),
        f.get(b).unwrap_or_else(|| panic!("{b} not indexed")),
    );
    assert_ne!(ha, hb, "{a} and {b} should be different files");

    let (pa, sa) = payload_of(db, a).await?;
    let (pb, sb) = payload_of(db, b).await?;
    assert!(pa.is_some(), "{a} has no payload hash to compare");
    assert_eq!(pa, pb, "{a} and {b} should share a payload hash");
    assert_eq!(sa.as_deref(), Some(scheme), "{a} scheme");
    assert_eq!(sb.as_deref(), Some(scheme), "{b} scheme");
    Ok(())
}

#[tokio::test]
async fn scan_records_every_class_and_ignores_non_media() -> Result<()> {
    let h = Harness::new();
    let s = h.scan().await?;
    let db = h.db().await?;

    assert_eq!(s.errors, 0, "no errors expected on the fixture corpus");
    assert!(s.audio > 0 && s.images > 0 && s.videos > 0, "{s:?}");

    let f = files(&db).await?;
    assert!(f.contains_key("music/ode_to_spot.mp3"));
    assert!(f.contains_key("photos/bridge.jpg"));
    assert!(f.contains_key("video/holodeck_clip.mp4"));
    // Non-media never reaches any table.
    assert!(!f.contains_key("readme.txt"), "readme.txt must be ignored");
    // Playlists are not items.
    assert!(!f.contains_key("playlists/bridge_ambience.m3u"));

    let items = items(&db).await?;
    let classes: HashSet<&str> = items.values().map(|v| v.2.as_str()).collect();
    assert_eq!(
        classes,
        HashSet::from(["audio", "image", "video"]),
        "all three classes should be present"
    );
    Ok(())
}

#[tokio::test]
async fn one_item_two_paths_when_a_file_is_copied() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await?;
    let f = files(&db).await?;

    let original = &f["music/ode_to_spot.mp3"];
    let copy = &f["archive/ode_to_spot_copy.mp3"];
    assert_eq!(original, copy, "byte-identical copies are one item");

    let n: i64 = sqlx::query("SELECT COUNT(*) AS n FROM media_files WHERE blake3 = ?")
        .bind(original)
        .fetch_one(db.pool())
        .await?
        .get("n");
    assert_eq!(n, 2, "both locations should be recorded");
    Ok(())
}

// ── The payload hash: one claim, one test per container ─────────────

#[tokio::test]
async fn retagging_an_mp3_leaves_the_payload_hash_alone() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await?;
    assert_metadata_only_variant(
        &db,
        "music/ode_to_spot.mp3",
        "music/ode_to_spot_retagged.mp3",
        "mp3.frames.v1",
    )
    .await?;

    // The untagged file has no ID3 block and no Xing frame at all, and
    // still lands on the same payload hash — which is what proves the
    // VBR header frame is excluded rather than merely tolerated.
    let (bare, _) = payload_of(&db, "music/untagged_hum.mp3").await?;
    let (tagged, _) = payload_of(&db, "music/ode_to_spot.mp3").await?;
    assert_eq!(bare, tagged);
    Ok(())
}

#[tokio::test]
async fn flac_cover_art_and_tags_are_excluded() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await?;
    assert_metadata_only_variant(
        &db,
        "music/warp_core_hum.flac",
        "music/warp_core_hum_with_art.flac",
        "flac.frames.v1",
    )
    .await
}

#[tokio::test]
async fn wav_info_chunks_are_excluded() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await?;
    assert_metadata_only_variant(
        &db,
        "music/tea_earl_grey.wav",
        "music/tea_earl_grey_untagged.wav",
        "wav.data.v1",
    )
    .await
}

#[tokio::test]
async fn rewriting_a_jpegs_exif_leaves_the_payload_hash_alone() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await?;
    assert_metadata_only_variant(
        &db,
        "photos/bridge.jpg",
        "photos/bridge_recaptioned.jpg",
        "jpeg.scan.v1",
    )
    .await?;

    // A JPEG with no EXIF block at all, same scan: also the same hash.
    let (none, _) = payload_of(&db, "photos/no_exif.jpg").await?;
    let (with, _) = payload_of(&db, "photos/bridge.jpg").await?;
    assert_eq!(none, with);
    Ok(())
}

#[tokio::test]
async fn png_text_chunks_are_excluded() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await?;
    assert_metadata_only_variant(
        &db,
        "photos/holodeck.png",
        "photos/holodeck_untagged.png",
        "png.idat.v1",
    )
    .await
}

/// The Lightroom case, and the reason the DNG recipe skips
/// reduced-resolution IFDs.
#[tokio::test]
async fn re_rendering_a_dng_preview_leaves_the_sensor_data_identity_intact() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await?;
    assert_metadata_only_variant(
        &db,
        "photos/sensor.dng",
        "photos/sensor_edited.dng",
        "tiff.strips.v1",
    )
    .await
}

#[tokio::test]
async fn a_video_gets_per_track_sample_hashes() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await?;
    let (p, s) = payload_of(&db, "video/holodeck_clip.mp4").await?;
    assert!(p.is_some(), "the clip should have a payload hash");
    assert_eq!(s.as_deref(), Some("bmff.samples.v1"));
    Ok(())
}

/// The rule that keeps `GROUP BY payload_blake3` honest.
#[tokio::test]
async fn an_unparsable_container_gets_null_rather_than_the_file_hash() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await?;
    let f = files(&db).await?;

    for path in ["video/mystery.avi", "music/corrupt.mp3"] {
        let (p, s) = payload_of(&db, path).await?;
        assert_eq!(p, None, "{path} should have no payload hash");
        assert_eq!(s, None, "{path} should have no payload scheme");
        // …and specifically must NOT have fallen back to the file hash.
        let file_hash = &f[path];
        let n: i64 = sqlx::query("SELECT COUNT(*) AS n FROM media_items WHERE payload_blake3 = ?")
            .bind(file_hash)
            .fetch_one(db.pool())
            .await?
            .get("n");
        assert_eq!(n, 0, "{path} must not use its file hash as a payload hash");
    }
    Ok(())
}

#[tokio::test]
async fn the_payload_ceiling_leaves_null_and_is_counted() -> Result<()> {
    let h = Harness::new();
    // Below every fixture's size, so nothing gets a payload hash.
    let s = h
        .scan_with(|o| download::FetchOptions {
            payload_max_bytes: Some(16),
            ..o
        })
        .await?;
    assert_eq!(s.payload_hashed, 0);
    assert!(s.payload_skipped > 0, "the skips must be visible: {s:?}");

    let db = h.db().await?;
    let n: i64 =
        sqlx::query("SELECT COUNT(*) AS n FROM media_items WHERE payload_blake3 IS NOT NULL")
            .fetch_one(db.pool())
            .await?
            .get("n");
    assert_eq!(n, 0);
    Ok(())
}

// ── Metadata ────────────────────────────────────────────────────────

#[tokio::test]
async fn audio_tags_are_hoisted_into_columns() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await?;

    let row = sqlx::query(
        "SELECT a.* FROM media_files f JOIN media_audio a ON a.blake3 = f.blake3
          WHERE f.id = 'music/ode_to_spot.mp3'",
    )
    .fetch_one(db.pool())
    .await?;
    assert_eq!(
        row.get::<Option<String>, _>("title").as_deref(),
        Some("Ode to Spot")
    );
    assert_eq!(
        row.get::<Option<String>, _>("artist").as_deref(),
        Some("Data")
    );
    assert_eq!(
        row.get::<Option<String>, _>("album").as_deref(),
        Some("Bridge Recitals")
    );
    assert_eq!(
        row.get::<Option<String>, _>("album_artist").as_deref(),
        Some("USS Enterprise Ensemble")
    );
    assert_eq!(row.get::<Option<i64>, _>("track_no"), Some(3));
    assert_eq!(row.get::<Option<i64>, _>("track_total"), Some(9));
    assert_eq!(row.get::<Option<i64>, _>("disc_no"), Some(1));
    assert_eq!(row.get::<Option<i64>, _>("sample_rate_hz"), Some(44100));

    // Vorbis comments through the same columns.
    let flac = sqlx::query(
        "SELECT a.artist AS artist, a.album AS album FROM media_files f
           JOIN media_audio a ON a.blake3 = f.blake3
          WHERE f.id = 'music/warp_core_hum.flac'",
    )
    .fetch_one(db.pool())
    .await?;
    assert_eq!(
        flac.get::<Option<String>, _>("artist").as_deref(),
        Some("Geordi La Forge")
    );
    assert_eq!(
        flac.get::<Option<String>, _>("album").as_deref(),
        Some("Engineering Ambience")
    );
    Ok(())
}

#[tokio::test]
async fn exif_is_hoisted_including_the_capture_offset_and_gps() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await?;

    let row = sqlx::query(
        "SELECT v.* FROM media_files f JOIN media_visual v ON v.blake3 = f.blake3
          WHERE f.id = 'photos/bridge.jpg'",
    )
    .fetch_one(db.pool())
    .await?;
    assert_eq!(
        row.get::<Option<String>, _>("camera_make").as_deref(),
        Some("Starfleet Optical")
    );
    assert_eq!(
        row.get::<Option<String>, _>("camera_model").as_deref(),
        Some("Tricorder Mk VII")
    );
    assert_eq!(row.get::<Option<i64>, _>("iso"), Some(400));
    // EXIF's `2364:04:13 08:45:00` plus `OffsetTimeOriginal`, rendered
    // as ISO-8601 with the offset preserved.
    assert_eq!(
        row.get::<Option<String>, _>("captured_at").as_deref(),
        Some("2364-04-13T08:45:00-07:00")
    );

    let lat: Option<f64> = row.get("gps_lat");
    let lon: Option<f64> = row.get("gps_lon");
    assert!((lat.unwrap() - 37.7749).abs() < 1e-3, "lat {lat:?}");
    // The western hemisphere: dropping `GPSLongitudeRef` would put this
    // at +122 instead.
    assert!((lon.unwrap() + 122.4194).abs() < 1e-3, "lon {lon:?}");
    Ok(())
}

#[tokio::test]
async fn dimensions_come_from_the_container_when_there_is_no_exif() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await?;

    let dims = |path: &'static str| {
        let pool = db.pool().clone();
        async move {
            let row = sqlx::query(
                "SELECT v.width AS w, v.height AS h FROM media_files f
                   JOIN media_visual v ON v.blake3 = f.blake3 WHERE f.id = ?",
            )
            .bind(path)
            .fetch_one(&pool)
            .await
            .unwrap();
            (
                row.get::<Option<i64>, _>("w"),
                row.get::<Option<i64>, _>("h"),
            )
        }
    };

    // PNG: from IHDR.
    assert_eq!(dims("photos/holodeck.png").await, (Some(4), Some(3)));
    // JPEG with no EXIF: from the SOF marker.
    assert_eq!(dims("photos/no_exif.jpg").await, (Some(480), Some(320)));
    // MP4: from tkhd's 16.16 fixed-point fields.
    assert_eq!(
        dims("video/holodeck_clip.mp4").await,
        (Some(320), Some(240))
    );

    // DNG: the *photograph's* size, not the embedded preview's. The
    // fixture's preview is 64x48 and its sensor image 320x240 with a
    // DefaultCropSize of 316x236 — so a reader that stopped at the
    // primary IFD, as EXIF alone does, would report 64x48 here.
    assert_eq!(dims("photos/sensor.dng").await, (Some(316), Some(236)));
    assert_eq!(
        dims("photos/sensor_edited.dng").await,
        (Some(316), Some(236))
    );
    Ok(())
}

#[tokio::test]
async fn a_video_reports_duration_codecs_and_its_own_capture_metadata() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await?;

    let row = sqlx::query(
        "SELECT i.duration_ms AS d, v.video_codec AS vc, v.audio_codec AS ac,
                v.captured_at AS at, v.gps_lat AS lat, v.gps_lon AS lon
           FROM media_files f
           JOIN media_items i ON i.blake3 = f.blake3
           JOIN media_visual v ON v.blake3 = f.blake3
          WHERE f.id = 'video/holodeck_clip.mp4'",
    )
    .fetch_one(db.pool())
    .await?;
    // 900 units at timescale 600.
    assert_eq!(row.get::<Option<i64>, _>("d"), Some(1500));
    assert_eq!(row.get::<Option<String>, _>("vc").as_deref(), Some("avc1"));
    assert_eq!(row.get::<Option<String>, _>("ac").as_deref(), Some("mp4a"));
    // The `©day` tag wins over `mvhd`, because it carries a real
    // offset where mvhd claims a UTC the camera may not have used.
    assert_eq!(
        row.get::<Option<String>, _>("at").as_deref(),
        Some("2364-04-13T08:45:00-0700")
    );
    // …and `©xyz` is where an iPhone puts a video's coordinates.
    let lat: Option<f64> = row.get("lat");
    let lon: Option<f64> = row.get("lon");
    assert!((lat.unwrap() - 37.7749).abs() < 1e-6, "lat {lat:?}");
    assert!((lon.unwrap() + 122.4194).abs() < 1e-6, "lon {lon:?}");
    Ok(())
}

/// A file can carry both kinds of metadata, and an item has only one
/// class — so choosing the reader by class dropped whichever half did
/// not match. A music video lost its title; an `.m4a` lost its
/// recording date.
#[tokio::test]
async fn a_bmff_file_keeps_both_its_tags_and_its_capture_metadata() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await?;

    let both = |path: &'static str| {
        let pool = db.pool().clone();
        async move {
            sqlx::query(
                "SELECT a.title AS tag_title, a.artist AS artist,
                        v.captured_at AS captured_at, v.video_codec AS vc
                   FROM media_files f
                   LEFT JOIN media_audio a ON a.blake3 = f.blake3
                   LEFT JOIN media_visual v ON v.blake3 = f.blake3
                  WHERE f.id = ?",
            )
            .bind(path)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };

    // A music video: `ilst` tags *and* a capture date and codec.
    let clip = both("video/holodeck_clip.mp4").await;
    assert_eq!(
        clip.get::<Option<String>, _>("tag_title").as_deref(),
        Some("Holodeck Program 9")
    );
    assert_eq!(
        clip.get::<Option<String>, _>("artist").as_deref(),
        Some("Reginald Barclay")
    );
    assert_eq!(
        clip.get::<Option<String>, _>("captured_at").as_deref(),
        Some("2364-04-13T08:45:00-0700")
    );
    assert_eq!(clip.get::<Option<String>, _>("vc").as_deref(), Some("avc1"));

    // The mirror image: an audio file with a recording date.
    let m4a = both("music/bridge_recital.m4a").await;
    assert_eq!(
        m4a.get::<Option<String>, _>("tag_title").as_deref(),
        Some("Bridge Recital")
    );
    assert_eq!(
        m4a.get::<Option<String>, _>("captured_at").as_deref(),
        Some("2364-04-13T09:15:00-0700")
    );
    // …but no video codec: an audio item's stream codec belongs on
    // `media_items`, not on a capture row.
    assert_eq!(m4a.get::<Option<String>, _>("vc"), None);

    Ok(())
}

/// The other half of reading both: a file that carries only one kind
/// must not gain an all-NULL row in the other table.
#[tokio::test]
async fn single_purpose_files_get_exactly_one_class_row() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await?;

    for (path, want_audio, want_visual) in [
        ("music/ode_to_spot.mp3", true, false),
        ("music/tea_earl_grey.wav", true, false),
        ("music/warp_core_hum.flac", true, false),
        ("photos/bridge.jpg", false, true),
        ("photos/holodeck.png", false, true),
        ("photos/sensor.dng", false, true),
    ] {
        let row = sqlx::query(
            "SELECT (a.blake3 IS NOT NULL) AS has_audio,
                    (v.blake3 IS NOT NULL) AS has_visual
               FROM media_files f
               LEFT JOIN media_audio a ON a.blake3 = f.blake3
               LEFT JOIN media_visual v ON v.blake3 = f.blake3
              WHERE f.id = ?",
        )
        .bind(path)
        .fetch_one(db.pool())
        .await?;
        assert_eq!(
            row.get::<i64, _>("has_audio") == 1,
            want_audio,
            "{path} audio"
        );
        assert_eq!(
            row.get::<i64, _>("has_visual") == 1,
            want_visual,
            "{path} visual"
        );
    }
    Ok(())
}

// ── Playlists ───────────────────────────────────────────────────────

#[tokio::test]
async fn a_playlist_keeps_its_order_and_its_unresolvable_entries() -> Result<()> {
    let h = Harness::new();
    let s = h.scan().await?;
    let db = h.db().await?;

    let pl = sqlx::query(
        "SELECT title, entry_count, format FROM media_playlists
          WHERE id = 'playlists/bridge_ambience.m3u'",
    )
    .fetch_one(db.pool())
    .await?;
    assert_eq!(
        pl.get::<Option<String>, _>("title").as_deref(),
        Some("Bridge Ambience")
    );
    assert_eq!(pl.get::<i64, _>("entry_count"), 6);
    assert_eq!(pl.get::<String, _>("format"), "m3u");

    // "How much of this playlist do I still have?" is a join, not a
    // stored count — so it is always current, where a column would go
    // stale the moment a track is added without a rescan.
    let have: i64 = sqlx::query(
        "SELECT COUNT(*) AS n FROM media_playlist_entries e
           JOIN media_files f ON f.id = e.resolved_path
          WHERE e.playlist_id = 'playlists/bridge_ambience.m3u'",
    )
    .fetch_one(db.pool())
    .await?
    .get("n");
    assert_eq!(have, 3);

    let rows = sqlx::query(
        "SELECT e.position       AS position,
                e.target_raw     AS target_raw,
                e.target_kind    AS target_kind,
                e.resolved_path  AS resolved_path,
                e.ext_title      AS ext_title,
                f.blake3         AS found
           FROM media_playlist_entries e
           LEFT JOIN media_files f ON f.id = e.resolved_path
          WHERE e.playlist_id = 'playlists/bridge_ambience.m3u'
          ORDER BY e.position",
    )
    .fetch_all(db.pool())
    .await?;
    assert_eq!(rows.len(), 6);

    // Order is preserved exactly as written.
    assert_eq!(
        rows[0].get::<String, _>("target_raw"),
        "../music/ode_to_spot.mp3"
    );
    assert_eq!(
        rows[0].get::<Option<String>, _>("resolved_path").as_deref(),
        Some("music/ode_to_spot.mp3")
    );
    assert!(rows[0].get::<Option<String>, _>("found").is_some());
    assert_eq!(
        rows[0].get::<Option<String>, _>("ext_title").as_deref(),
        Some("Data - Ode to Spot")
    );

    // The valuable row: a track that resolves to a path holding
    // nothing. Kept, with the raw text intact and a NULL item.
    let deleted = &rows[2];
    assert_eq!(
        deleted.get::<String, _>("target_raw"),
        "../music/deleted_long_ago.mp3"
    );
    assert_eq!(
        deleted.get::<Option<String>, _>("resolved_path").as_deref(),
        Some("music/deleted_long_ago.mp3"),
        "the path it wanted is recorded even though nothing is there"
    );
    assert!(deleted.get::<Option<String>, _>("found").is_none());

    // A URL and an absolute path: recorded, classified, unresolved.
    assert_eq!(rows[3].get::<String, _>("target_kind"), "url");
    assert!(rows[3].get::<Option<String>, _>("resolved_path").is_none());
    assert_eq!(rows[4].get::<String, _>("target_kind"), "absolute");
    assert!(rows[4].get::<Option<String>, _>("resolved_path").is_none());

    // Windows separators in a relative target still resolve.
    assert_eq!(
        rows[5].get::<String, _>("target_raw"),
        "..\\music\\tea_earl_grey.wav"
    );
    assert!(rows[5].get::<Option<String>, _>("found").is_some());

    // Six entries across both playlists name a path inside the tree
    // (four here plus two in the Latin-1 one); five of those paths
    // actually hold a file — `deleted_long_ago.mp3` resolves and finds
    // nothing. The scan records the first number because it is a fact
    // about the playlist text; the second is the join above, and the
    // gap between them is the whole reason the two are not one column.
    assert_eq!(s.playlist_entries_in_tree, 6, "across both playlists");
    let have_any: i64 = sqlx::query(
        "SELECT COUNT(*) AS n FROM media_playlist_entries e
           JOIN media_files f ON f.id = e.resolved_path",
    )
    .fetch_one(db.pool())
    .await?
    .get("n");
    assert_eq!(have_any, 5);
    Ok(())
}

#[tokio::test]
async fn a_latin1_playlist_decodes_and_keeps_duplicate_entries() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await?;

    let rows = sqlx::query(
        "SELECT position, target_raw, ext_title FROM media_playlist_entries
          WHERE playlist_id LIKE 'playlists/caf%' ORDER BY position",
    )
    .fetch_all(db.pool())
    .await?;
    assert_eq!(rows.len(), 2, "the same track twice is two entries");
    assert_eq!(
        rows[0].get::<String, _>("target_raw"),
        rows[1].get::<String, _>("target_raw")
    );
    assert_eq!(
        rows[0].get::<Option<String>, _>("ext_title").as_deref(),
        Some("Café Ambience"),
        "Latin-1 bytes should decode rather than fail"
    );
    Ok(())
}

#[tokio::test]
async fn hls_manifests_are_skipped_and_counted() -> Result<()> {
    let h = Harness::new();
    let s = h.scan().await?;
    let db = h.db().await?;

    assert_eq!(s.hls_skipped, 1, "the stream manifest should be recognized");
    let n: i64 = sqlx::query("SELECT COUNT(*) AS n FROM media_playlists WHERE id LIKE '%.m3u8'")
        .fetch_one(db.pool())
        .await?
        .get("n");
    assert_eq!(n, 0, "an HLS manifest must not be recorded as a playlist");
    Ok(())
}

// ── Rescan ──────────────────────────────────────────────────────────

#[tokio::test]
async fn a_rescan_reuses_hashes_and_is_idempotent() -> Result<()> {
    let h = Harness::new();
    let first = h.scan().await?;
    let db = h.db().await?;
    let files_before = files(&db).await?;
    let items_before = items(&db).await?;

    let second = h.scan().await?;
    // Nothing changed on disk, so the Unison cursor should carry every
    // file and no bytes should be re-read.
    assert_eq!(second.reused, first.entries_scanned);
    assert_eq!(second.hashed, 0, "no file should be rehashed: {second:?}");
    assert_eq!(second.items, 0, "no item should be re-identified");

    let db = h.db().await?;
    assert_eq!(files(&db).await?, files_before, "path rows must be stable");
    assert_eq!(
        items(&db).await?.len(),
        items_before.len(),
        "item rows must be stable"
    );
    // Playlists are rebuilt from scratch each scan, so they must not
    // accumulate duplicates.
    let n: i64 = sqlx::query("SELECT COUNT(*) AS n FROM media_playlist_entries")
        .fetch_one(db.pool())
        .await?
        .get("n");
    assert_eq!(n, first.playlist_entries as i64);
    Ok(())
}

/// A scan that dies partway must leave its rescan cursors behind.
///
/// This is the difference between resuming and restarting on a large
/// library, and it is why the path tables are reconciled at the end of
/// a scan rather than truncated at the start. The failure is induced
/// with a malformed ignore glob, which makes `walk_files` error at
/// exactly the point an interrupt would — after the cache is loaded,
/// before any row is written.
#[tokio::test]
async fn a_failed_scan_leaves_the_rescan_cursors_intact() -> Result<()> {
    let h = Harness::new();
    let first = h.scan().await?;
    let db = h.db().await?;
    let before = files(&db).await?;
    assert!(!before.is_empty());

    let err = h
        .scan_with(|o| download::FetchOptions {
            // An unclosed character class: rejected when the override
            // set is built, which is inside the walk.
            ignore: vec!["[".to_string()],
            ..o
        })
        .await;
    assert!(err.is_err(), "the malformed pattern should fail the scan");

    // Truncating up front would have emptied this before the failure.
    let db = h.db().await?;
    assert_eq!(
        files(&db).await?,
        before,
        "a failed scan must not discard the path table"
    );

    // …and the point of keeping them: the next scan reads no bytes.
    let after = h.scan().await?;
    assert_eq!(after.reused, first.entries_scanned);
    assert_eq!(
        after.hashed, 0,
        "cursors survived, so nothing should be rehashed: {after:?}"
    );
    Ok(())
}

/// The other half of the same trade: reconciliation still happens, it
/// just happens at the end.
#[tokio::test]
async fn deletions_are_reconciled_without_a_clock() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("corpus");
    copy_tree(&fixture_dir(), &root)?;
    let work = tempfile::tempdir()?;
    let raw_dir = work.path().join("raw");
    std::fs::create_dir_all(&raw_dir)?;

    let first = scan_root(&root, &raw_dir).await?;
    assert_eq!(first.removed, 0);

    std::fs::remove_file(root.join("music/untagged_hum.mp3"))?;
    std::fs::remove_file(root.join("playlists/bridge_ambience.m3u"))?;
    // Both scans use the same pinned `NOW`, deliberately: a
    // `WHERE last_seen_at <> ?` sweep would delete nothing here, which
    // is exactly why reconciliation is a set difference instead.
    let second = scan_root(&root, &raw_dir).await?;
    assert_eq!(second.removed, 2, "one file and one playlist: {second:?}");

    let db = RawDb::open(&download::db_path_for(&raw_dir)).await?;
    let f = files(&db).await?;
    assert!(!f.contains_key("music/untagged_hum.mp3"));
    let n: i64 = sqlx::query(
        "SELECT COUNT(*) AS n FROM media_playlist_entries
          WHERE playlist_id = 'playlists/bridge_ambience.m3u'",
    )
    .fetch_one(db.pool())
    .await?
    .get("n");
    assert_eq!(n, 0, "a vanished playlist takes its entries with it");
    Ok(())
}

/// Shortening a playlist must drop its tail. Entries are keyed
/// `<path>#<position>`, so an upsert alone would leave the rows for
/// positions that no longer exist.
#[tokio::test]
async fn a_shortened_playlist_loses_its_trailing_entries() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("corpus");
    copy_tree(&fixture_dir(), &root)?;
    let work = tempfile::tempdir()?;
    let raw_dir = work.path().join("raw");
    std::fs::create_dir_all(&raw_dir)?;

    scan_root(&root, &raw_dir).await?;
    let pl = root.join("playlists/bridge_ambience.m3u");
    std::fs::write(
        &pl,
        "#EXTM3U\n#PLAYLIST:Bridge Ambience\n../music/ode_to_spot.mp3\n",
    )?;
    scan_root(&root, &raw_dir).await?;

    let db = RawDb::open(&download::db_path_for(&raw_dir)).await?;
    let rows = sqlx::query(
        "SELECT position FROM media_playlist_entries
          WHERE playlist_id = 'playlists/bridge_ambience.m3u' ORDER BY position",
    )
    .fetch_all(db.pool())
    .await?;
    assert_eq!(rows.len(), 1, "five stale entries should be gone");
    Ok(())
}

#[tokio::test]
async fn force_rehash_re_reads_everything_without_changing_a_row() -> Result<()> {
    let h = Harness::new();
    let first = h.scan().await?;
    let db = h.db().await?;
    let before = files(&db).await?;

    let forced = h
        .scan_with(|o| download::FetchOptions {
            force_rehash: true,
            ..o
        })
        .await?;
    assert_eq!(forced.hashed, first.entries_scanned, "every file re-read");
    assert_eq!(forced.reused, 0);

    let db = h.db().await?;
    assert_eq!(
        files(&db).await?,
        before,
        "re-reading unchanged bytes must produce identical rows"
    );
    Ok(())
}

#[tokio::test]
async fn a_deleted_file_disappears_from_the_path_table_but_the_item_remains() -> Result<()> {
    // Scan a copy of the corpus so a file can be removed.
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("corpus");
    copy_tree(&fixture_dir(), &root)?;

    let work = tempfile::tempdir()?;
    let raw_dir = work.path().join("raw");
    std::fs::create_dir_all(&raw_dir)?;

    scan_root(&root, &raw_dir).await?;
    let db = RawDb::open(&download::db_path_for(&raw_dir)).await?;
    let hash = files(&db).await?["music/untagged_hum.mp3"].clone();

    std::fs::remove_file(root.join("music/untagged_hum.mp3"))?;
    scan_root(&root, &raw_dir).await?;

    let db = RawDb::open(&download::db_path_for(&raw_dir)).await?;
    assert!(
        !files(&db).await?.contains_key("music/untagged_hum.mp3"),
        "the path row should fall out with the truncate"
    );
    // The item survives: it is keyed on content, which has no notion of
    // "no longer present", and keeping it preserves `first_seen_at`.
    assert!(
        items(&db).await?.contains_key(&hash),
        "the item row should remain (see DOWNLOAD.md §Orphaned items)"
    );
    Ok(())
}

/// One scan, six kinds of edit, one rescan — and an exact accounting of
/// what moved.
///
/// The individual behaviors have their own tests above; this one exists
/// because they interact, and because the numbers are the claim. If
/// unchanged files were quietly being re-read, or a retag were creating
/// a second payload group, every narrower test would still pass and
/// only the arithmetic here would break.
#[tokio::test]
async fn a_rescan_after_edits_changes_exactly_what_it_should() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("corpus");
    copy_tree(&fixture_dir(), &root)?;
    let work = tempfile::tempdir()?;
    let raw_dir = work.path().join("raw");
    std::fs::create_dir_all(&raw_dir)?;

    // ── Scan 1: the baseline ─────────────────────────────────────────
    let first = scan_root(&root, &raw_dir).await?;
    let db = RawDb::open(&download::db_path_for(&raw_dir)).await?;
    let files_before = files(&db).await?;
    let items_before = items(&db).await?;
    let hum_payload_before = payload_of(&db, "music/untagged_hum.mp3").await?.0;
    assert!(hum_payload_before.is_some());
    assert_eq!(
        first.hashed, first.entries_scanned,
        "first scan reads everything"
    );
    assert_eq!(first.removed, 0);

    // ── Six edits ────────────────────────────────────────────────────

    // (1) Retag: prepend an ID3v2 block, which is what every modern
    //     tagger does. It shifts every byte of audio in the file, so
    //     this is also the strongest form of the payload-hash claim.
    let hum = root.join("music/untagged_hum.mp3");
    let mut tagged = id3v2_with_title("Warp Core Hum (Live)");
    tagged.extend_from_slice(&std::fs::read(&hum)?);
    std::fs::write(&hum, &tagged)?;

    // (2) Touch without editing: mtime moves, bytes do not. The cursor
    //     misses, so the file is re-read — and must produce the same
    //     hash and no new item.
    let flac = root.join("music/warp_core_hum.flac");
    let f = std::fs::File::options().write(true).open(&flac)?;
    f.set_times(std::fs::FileTimes::new().set_modified(
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000),
    ))?;
    drop(f);

    // (3) Add a copy: a new path holding content we already know.
    std::fs::copy(
        root.join("photos/bridge.jpg"),
        root.join("photos/bridge_copy.jpg"),
    )?;

    // (4) Add something genuinely new.
    let mut fresh = std::fs::read(root.join("music/tea_earl_grey.wav"))?;
    *fresh.last_mut().unwrap() ^= 0xff; // the last byte is inside `data`
    std::fs::write(root.join("music/new_track.wav"), &fresh)?;

    // (5) Delete a file.
    std::fs::remove_file(root.join("music/corrupt.mp3"))?;

    // (6) Shorten a playlist.
    std::fs::write(
        root.join("playlists/café_deck_ten.m3u"),
        "#EXTM3U\n../music/tea_earl_grey.wav\n",
    )?;

    // ── Scan 2: the accounting ───────────────────────────────────────
    let second = scan_root(&root, &raw_dir).await?;
    assert_eq!(
        second.files_seen,
        first.files_seen - 1 + 2,
        "one deleted, two added"
    );
    // The whole point of the cursor: only the files whose stat changed
    // are read. Everything else is skipped without a read.
    //
    // Five, not four: the shortened playlist from (6) is a file whose
    // bytes changed like any other. It was always being hashed — the
    // playlist pass did it separately — but did not use to be counted
    // here, and is now hashed once by the scan rather than twice.
    assert_eq!(
        second.hashed, 5,
        "retagged + touched + copied + new + the edited playlist: {second:?}"
    );
    assert_eq!(second.reused, second.entries_scanned - 5);
    // Two of those four hold content we had not seen: the retagged MP3
    // and the new WAV. The copy and the touched file do not.
    assert_eq!(second.items, 2, "{second:?}");
    assert_eq!(second.removed, 1, "the deleted file's path row");

    let db = RawDb::open(&download::db_path_for(&raw_dir)).await?;
    let files_after = files(&db).await?;
    let items_after = items(&db).await?;

    // (1) Retag: new file hash, SAME payload hash — a new row that is
    //     recognizably the same recording.
    assert_ne!(
        files_after["music/untagged_hum.mp3"], files_before["music/untagged_hum.mp3"],
        "the file changed"
    );
    assert_eq!(
        payload_of(&db, "music/untagged_hum.mp3").await?.0,
        hum_payload_before,
        "…but the audio did not"
    );
    let title: Option<String> = sqlx::query(
        "SELECT a.title AS t FROM media_files f JOIN media_audio a ON a.blake3 = f.blake3
          WHERE f.id = 'music/untagged_hum.mp3'",
    )
    .fetch_one(db.pool())
    .await?
    .get("t");
    assert_eq!(title.as_deref(), Some("Warp Core Hum (Live)"));

    // (2) Touched: re-read, identical hash, no new item.
    assert_eq!(
        files_after["music/warp_core_hum.flac"], files_before["music/warp_core_hum.flac"],
        "re-reading unchanged bytes must not change the row"
    );

    // (3) Copy: a second path onto one item.
    assert_eq!(
        files_after["photos/bridge_copy.jpg"], files_after["photos/bridge.jpg"],
        "a copy is the same item"
    );

    // (4) New file: a new item with its own payload.
    assert!(files_after.contains_key("music/new_track.wav"));
    assert_ne!(
        payload_of(&db, "music/new_track.wav").await?.0,
        payload_of(&db, "music/tea_earl_grey.wav").await?.0,
        "a changed sample is a different recording"
    );

    // (5) Deleted: the path is gone; the item is not.
    assert!(!files_after.contains_key("music/corrupt.mp3"));
    assert!(
        items_after.contains_key(&files_before["music/corrupt.mp3"]),
        "content-keyed rows survive their last path (see DOWNLOAD.md)"
    );

    // Items only ever grow: two added, none removed — including the
    // now-orphaned pre-retag version of the MP3.
    assert_eq!(items_after.len(), items_before.len() + 2);

    // (6) Playlist: rewritten, not merged. The old positions are gone.
    let n: i64 = sqlx::query(
        "SELECT COUNT(*) AS n FROM media_playlist_entries
          WHERE playlist_id LIKE 'playlists/caf%'",
    )
    .fetch_one(db.pool())
    .await?
    .get("n");
    assert_eq!(n, 1, "a shortened playlist loses its tail");

    // ── Scan 3: settled ──────────────────────────────────────────────
    let third = scan_root(&root, &raw_dir).await?;
    assert_eq!(third.hashed, 0, "nothing left to read: {third:?}");
    assert_eq!(third.items, 0);
    assert_eq!(third.removed, 0);
    let db = RawDb::open(&download::db_path_for(&raw_dir)).await?;
    assert_eq!(files(&db).await?, files_after, "a settled tree is stable");
    Ok(())
}

/// A minimal ID3v2.4 tag carrying one `TIT2` frame.
///
/// Sizes are *syncsafe* — seven bits per byte — which is the detail
/// that makes a hand-built tag either work or land the reader in the
/// middle of the audio.
fn id3v2_with_title(title: &str) -> Vec<u8> {
    fn syncsafe(n: u32) -> [u8; 4] {
        [
            ((n >> 21) & 0x7f) as u8,
            ((n >> 14) & 0x7f) as u8,
            ((n >> 7) & 0x7f) as u8,
            (n & 0x7f) as u8,
        ]
    }
    let text = {
        let mut t = vec![0x03u8]; // UTF-8
        t.extend_from_slice(title.as_bytes());
        t
    };
    let mut frame = b"TIT2".to_vec();
    frame.extend_from_slice(&syncsafe(text.len() as u32));
    frame.extend_from_slice(&[0x00, 0x00]); // flags
    frame.extend_from_slice(&text);

    let mut tag = b"ID3\x04\x00\x00".to_vec();
    tag.extend_from_slice(&syncsafe(frame.len() as u32));
    tag.extend_from_slice(&frame);
    tag
}

/// Scan a caller-owned tree, for the tests that mutate the corpus.
async fn scan_root(
    root: &std::path::Path,
    raw_dir: &std::path::Path,
) -> Result<download::FetchSummary> {
    let db = RawDb::open(&download::db_path_for(raw_dir)).await?;
    let cache = FingerprintCache::open(&raw_dir.join("fingerprints.sqlite")).await?;
    download::fetch(download::FetchOptions {
        db,
        source_name: STANZA.to_string(),
        root: root.to_path_buf(),
        cache,
        ignore: vec![],
        max_bytes: None,
        payload_max_bytes: None,
        playlists: true,
        skip_dataless: true,
        force_rehash: false,
        now: NOW.to_string(),
        progress: datalib_etl::progress::Progress::noop(),
    })
    .await
}

fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dst = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &dst)?;
        } else {
            std::fs::copy(entry.path(), &dst)?;
        }
    }
    Ok(())
}
