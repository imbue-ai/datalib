//! Read a mirrored catalog at one commit: every image, with everything
//! render shows of it. Lightroom keeps one image's facts in a dozen
//! tables keyed by `id_local`; each is read once and joined here.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_photo_schema::{Person, Place, Rect};

/// The tables render reads, all of them whole: a change to any one
/// renders the catalog again.
pub const TABLES: &[&str] = &[
    "Adobe_images",
    "Adobe_AdditionalMetadata",
    "Adobe_variablesTable",
    "AgLibraryFile",
    "AgLibraryFolder",
    "AgLibraryRootFolder",
    "AgLibraryIPTC",
    "AgHarvestedExifMetadata",
    "AgHarvestedIptcMetadata",
    "AgInternedExifCameraModel",
    "AgInternedExifLens",
    "AgInternedIptcCity",
    "AgInternedIptcState",
    "AgInternedIptcCountry",
    "AgInternedIptcLocation",
    "AgInternedIptcCreator",
    "AgLibraryKeyword",
    "AgLibraryKeywordImage",
    "AgLibraryFace",
    "AgLibraryKeywordFace",
    "AgLibraryCollection",
    "AgLibraryCollectionImage",
    "AgVideoInfo",
];

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// Where Lightroom last had the catalog open.
    pub catalog_path: Option<String>,
    /// Whether the mirror holds the XMP packets, which `skip_xmp` drops.
    pub xmp_mirrored: bool,
    /// By `id_local`, in catalog order.
    pub images: BTreeMap<i64, ImageRow>,
}

#[derive(Debug, Clone, Default)]
pub struct ImageRow {
    pub id_local: i64,
    pub id_global: String,
    pub capture_time: Option<String>,
    /// Seconds since 2001-01-01 UTC, Apple's reference date.
    pub touch_time: Option<f64>,
    pub rating: Option<f64>,
    pub pick: Option<f64>,
    pub color_label: Option<String>,
    pub file_format: Option<String>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    /// A virtual copy's name; `None` for the master.
    pub copy_name: Option<String>,
    pub file_uuid: String,
    pub base_name: String,
    pub extension: Option<String>,
    pub folder: Option<String>,
    pub root_absolute: Option<String>,
    pub root_relative: Option<String>,
    pub caption: Option<String>,
    pub copyright: Option<String>,
    pub gps: Option<(f64, f64)>,
    pub iso: Option<f64>,
    /// APEX values, as Lightroom stores them.
    pub shutter_apex: Option<f64>,
    pub aperture_apex: Option<f64>,
    pub focal_length: Option<f64>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub place: Place,
    pub creator: Option<String>,
    pub keywords: Vec<Vec<String>>,
    pub people: Vec<Person>,
    pub albums: Vec<String>,
    pub xmp: Option<Vec<u8>>,
    pub duration: Option<String>,
}

pub async fn load(pool: &SqlitePool) -> Result<Catalog> {
    let schema = table_sql(pool).await?;
    let has = |t: &str| schema.contains_key(t);
    let xmp_mirrored = schema
        .get("Adobe_AdditionalMetadata")
        .is_some_and(|sql| sql.contains("\"xmp\""));

    let mut images = load_images(pool).await?;
    if has("AgLibraryIPTC") {
        load_iptc(pool, &mut images).await?;
    }
    if has("AgHarvestedExifMetadata") {
        load_exif(pool, &mut images).await?;
    }
    if has("AgHarvestedIptcMetadata") {
        load_place(pool, &mut images).await?;
    }
    if has("AgLibraryKeywordImage") {
        load_keywords(
            pool,
            &mut images,
            has("AgLibraryFace") && has("AgLibraryKeywordFace"),
        )
        .await?;
    }
    if has("AgLibraryCollectionImage") {
        load_albums(pool, &mut images).await?;
    }
    if xmp_mirrored {
        load_xmp(pool, &mut images).await?;
    }
    if has("AgVideoInfo") {
        load_video(pool, &mut images).await?;
    }
    let catalog_path = if has("Adobe_variablesTable") {
        sqlx::query_scalar(
            "SELECT CAST(value AS TEXT) FROM Adobe_variablesTable \
              WHERE name = 'Adobe_lastScannedCatalogPath' LIMIT 1",
        )
        .fetch_optional(pool)
        .await
        .context("read where the catalog was last opened")?
        .flatten()
    } else {
        None
    };
    Ok(Catalog {
        catalog_path,
        xmp_mirrored,
        images,
    })
}

/// Each table's `CREATE TABLE`, by name.
async fn table_sql(pool: &SqlitePool) -> Result<HashMap<String, String>> {
    let rows = sqlx::query("SELECT name, sql FROM sqlite_master WHERE type = 'table'")
        .fetch_all(pool)
        .await
        .context("list the mirror's tables")?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.get::<String, _>("name"),
                r.get::<Option<String>, _>("sql").unwrap_or_default(),
            )
        })
        .collect())
}

async fn load_images(pool: &SqlitePool) -> Result<BTreeMap<i64, ImageRow>> {
    let rows = sqlx::query(
        "SELECT CAST(i.id_local AS INTEGER) AS id_local, CAST(i.id_global AS TEXT) AS id_global, \
                CAST(i.captureTime AS TEXT) AS capture_time, \
                CAST(i.touchTime AS REAL) AS touch_time, CAST(i.rating AS REAL) AS rating, \
                CAST(i.pick AS REAL) AS pick, CAST(i.colorLabels AS TEXT) AS color_label, \
                CAST(i.fileFormat AS TEXT) AS file_format, \
                CAST(i.fileWidth AS REAL) AS width, CAST(i.fileHeight AS REAL) AS height, \
                CAST(i.copyName AS TEXT) AS copy_name, \
                CAST(f.id_global AS TEXT) AS file_uuid, CAST(f.baseName AS TEXT) AS base_name, \
                CAST(f.extension AS TEXT) AS extension, \
                CAST(fo.pathFromRoot AS TEXT) AS folder, \
                CAST(rf.absolutePath AS TEXT) AS root_absolute, \
                CAST(rf.relativePathFromCatalog AS TEXT) AS root_relative \
           FROM Adobe_images i \
           JOIN AgLibraryFile f ON f.id_local = i.rootFile \
           LEFT JOIN AgLibraryFolder fo ON fo.id_local = f.folder \
           LEFT JOIN AgLibraryRootFolder rf ON rf.id_local = fo.rootFolder",
    )
    .fetch_all(pool)
    .await
    .context("read Adobe_images")?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let id_local: i64 = r.get("id_local");
            let image = ImageRow {
                id_local,
                id_global: r.get("id_global"),
                capture_time: non_empty(r.get("capture_time")),
                touch_time: r.get("touch_time"),
                rating: r.get("rating"),
                pick: r.get("pick"),
                color_label: non_empty(r.get("color_label")),
                file_format: non_empty(r.get("file_format")),
                width: r.get("width"),
                height: r.get("height"),
                copy_name: non_empty(r.get("copy_name")),
                file_uuid: r.get("file_uuid"),
                base_name: r.get("base_name"),
                extension: non_empty(r.get("extension")),
                folder: non_empty(r.get("folder")),
                root_absolute: non_empty(r.get("root_absolute")),
                root_relative: non_empty(r.get("root_relative")),
                ..ImageRow::default()
            };
            (id_local, image)
        })
        .collect())
}

async fn load_iptc(pool: &SqlitePool, images: &mut BTreeMap<i64, ImageRow>) -> Result<()> {
    let rows = sqlx::query(
        "SELECT CAST(image AS INTEGER) AS image, CAST(caption AS TEXT) AS caption, \
                CAST(copyright AS TEXT) AS copyright \
           FROM AgLibraryIPTC",
    )
    .fetch_all(pool)
    .await
    .context("read AgLibraryIPTC")?;
    for r in rows {
        if let Some(image) = images.get_mut(&r.get::<i64, _>("image")) {
            image.caption = non_empty(r.get("caption"));
            image.copyright = non_empty(r.get("copyright"));
        }
    }
    Ok(())
}

async fn load_exif(pool: &SqlitePool, images: &mut BTreeMap<i64, ImageRow>) -> Result<()> {
    let rows = sqlx::query(
        "SELECT CAST(e.image AS INTEGER) AS image, CAST(e.hasGPS AS INTEGER) AS has_gps, \
                CAST(e.gpsLatitude AS REAL) AS lat, CAST(e.gpsLongitude AS REAL) AS lng, \
                CAST(e.isoSpeedRating AS REAL) AS iso, CAST(e.shutterSpeed AS REAL) AS shutter, \
                CAST(e.aperture AS REAL) AS aperture, CAST(e.focalLength AS REAL) AS focal, \
                CAST(cm.value AS TEXT) AS camera, CAST(l.value AS TEXT) AS lens \
           FROM AgHarvestedExifMetadata e \
           LEFT JOIN AgInternedExifCameraModel cm ON cm.id_local = e.cameraModelRef \
           LEFT JOIN AgInternedExifLens l ON l.id_local = e.lensRef",
    )
    .fetch_all(pool)
    .await
    .context("read AgHarvestedExifMetadata")?;
    for r in rows {
        if let Some(image) = images.get_mut(&r.get::<i64, _>("image")) {
            let has_gps = r.get::<Option<i64>, _>("has_gps") == Some(1);
            let lat: Option<f64> = r.get("lat");
            let lng: Option<f64> = r.get("lng");
            image.gps = match (has_gps, lat, lng) {
                (true, Some(lat), Some(lng)) => Some((lat, lng)),
                _ => None,
            };
            image.iso = r.get("iso");
            image.shutter_apex = r.get("shutter");
            image.aperture_apex = r.get("aperture");
            image.focal_length = r.get("focal");
            image.camera = non_empty(r.get("camera"));
            image.lens = non_empty(r.get("lens"));
        }
    }
    Ok(())
}

async fn load_place(pool: &SqlitePool, images: &mut BTreeMap<i64, ImageRow>) -> Result<()> {
    let rows = sqlx::query(
        "SELECT CAST(h.image AS INTEGER) AS image, \
                CAST(ci.value AS TEXT) AS city, CAST(st.value AS TEXT) AS state, \
                CAST(co.value AS TEXT) AS country, CAST(lo.value AS TEXT) AS location, \
                CAST(cr.value AS TEXT) AS creator \
           FROM AgHarvestedIptcMetadata h \
           LEFT JOIN AgInternedIptcCity ci ON ci.id_local = h.cityRef \
           LEFT JOIN AgInternedIptcState st ON st.id_local = h.stateRef \
           LEFT JOIN AgInternedIptcCountry co ON co.id_local = h.countryRef \
           LEFT JOIN AgInternedIptcLocation lo ON lo.id_local = h.locationRef \
           LEFT JOIN AgInternedIptcCreator cr ON cr.id_local = h.creatorRef",
    )
    .fetch_all(pool)
    .await
    .context("read AgHarvestedIptcMetadata")?;
    for r in rows {
        if let Some(image) = images.get_mut(&r.get::<i64, _>("image")) {
            image.place = Place {
                location: non_empty(r.get("location")),
                city: non_empty(r.get("city")),
                state: non_empty(r.get("state")),
                country: non_empty(r.get("country")),
            };
            image.creator = non_empty(r.get("creator"));
        }
    }
    Ok(())
}

struct Keyword {
    name: Option<String>,
    parent: Option<i64>,
    person: bool,
}

/// Keywords as paths from the top of the tree, and people: a face named
/// with a person keyword, or the keyword alone where no face was drawn.
async fn load_keywords(
    pool: &SqlitePool,
    images: &mut BTreeMap<i64, ImageRow>,
    faces: bool,
) -> Result<()> {
    let tree: HashMap<i64, Keyword> = sqlx::query(
        "SELECT CAST(id_local AS INTEGER) AS id, CAST(name AS TEXT) AS name, \
                CAST(parent AS INTEGER) AS parent, CAST(keywordType AS TEXT) AS kind \
           FROM AgLibraryKeyword",
    )
    .fetch_all(pool)
    .await
    .context("read AgLibraryKeyword")?
    .into_iter()
    .map(|r| {
        (
            r.get::<i64, _>("id"),
            Keyword {
                name: non_empty(r.get("name")),
                parent: r.get("parent"),
                person: r.get::<Option<String>, _>("kind").as_deref() == Some("person"),
            },
        )
    })
    .collect();
    let path = |mut id: i64| {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        while let Some(k) = tree.get(&id) {
            if !seen.insert(id) {
                break;
            }
            if let Some(name) = &k.name {
                out.push(name.clone());
            }
            match k.parent {
                Some(p) => id = p,
                None => break,
            }
        }
        out.reverse();
        out
    };

    if faces {
        let rows = sqlx::query(
            "SELECT CAST(f.image AS INTEGER) AS image, CAST(kf.tag AS INTEGER) AS tag, \
                    CAST(f.tl_x AS REAL) AS left_, CAST(f.tl_y AS REAL) AS top, \
                    CAST(f.br_x AS REAL) AS right_, CAST(f.br_y AS REAL) AS bottom \
               FROM AgLibraryFace f JOIN AgLibraryKeywordFace kf ON kf.face = f.id_local \
              WHERE COALESCE(CAST(kf.userPick AS REAL), 0) = 1 \
                AND COALESCE(CAST(kf.userReject AS REAL), 0) = 0 \
              ORDER BY f.image, f.id_local",
        )
        .fetch_all(pool)
        .await
        .context("read AgLibraryFace")?;
        for r in rows {
            let Some(image) = images.get_mut(&r.get::<i64, _>("image")) else {
                continue;
            };
            let Some(name) = tree
                .get(&r.get::<i64, _>("tag"))
                .and_then(|k| k.name.clone())
            else {
                continue;
            };
            let corners: [Option<f64>; 4] = [
                r.get("left_"),
                r.get("top"),
                r.get("right_"),
                r.get("bottom"),
            ];
            let face = match corners {
                [Some(left), Some(top), Some(right), Some(bottom)] => Some(Rect {
                    left,
                    top,
                    right,
                    bottom,
                }),
                _ => None,
            };
            image.people.push(Person { name, face });
        }
    }

    let rows = sqlx::query(
        "SELECT CAST(image AS INTEGER) AS image, CAST(tag AS INTEGER) AS tag \
           FROM AgLibraryKeywordImage ORDER BY image, tag",
    )
    .fetch_all(pool)
    .await
    .context("read AgLibraryKeywordImage")?;
    for r in rows {
        let Some(image) = images.get_mut(&r.get::<i64, _>("image")) else {
            continue;
        };
        let tag: i64 = r.get("tag");
        let Some(keyword) = tree.get(&tag) else {
            continue;
        };
        match (&keyword.name, keyword.person) {
            (Some(name), true) => {
                if !image.people.iter().any(|p| &p.name == name) {
                    image.people.push(Person {
                        name: name.clone(),
                        face: None,
                    });
                }
            }
            (Some(_), false) => image.keywords.push(path(tag)),
            (None, _) => {}
        }
    }
    for image in images.values_mut() {
        image.keywords.sort();
        image.keywords.dedup();
    }
    Ok(())
}

/// Collections a person made: not smart collections, which are saved
/// searches with no members, and not the quick collection.
async fn load_albums(pool: &SqlitePool, images: &mut BTreeMap<i64, ImageRow>) -> Result<()> {
    let rows = sqlx::query(
        "SELECT CAST(ci.image AS INTEGER) AS image, CAST(c.name AS TEXT) AS name \
           FROM AgLibraryCollectionImage ci \
           JOIN AgLibraryCollection c ON c.id_local = ci.collection \
          WHERE c.creationId = 'com.adobe.ag.library.collection' \
            AND COALESCE(CAST(c.systemOnly AS REAL), 0) = 0 \
          ORDER BY ci.image, c.name",
    )
    .fetch_all(pool)
    .await
    .context("read AgLibraryCollectionImage")?;
    for r in rows {
        if let (Some(image), Some(name)) = (
            images.get_mut(&r.get::<i64, _>("image")),
            non_empty(r.get("name")),
        ) {
            if !image.albums.contains(&name) {
                image.albums.push(name);
            }
        }
    }
    Ok(())
}

async fn load_xmp(pool: &SqlitePool, images: &mut BTreeMap<i64, ImageRow>) -> Result<()> {
    let rows = sqlx::query(
        "SELECT CAST(image AS INTEGER) AS image, xmp FROM Adobe_AdditionalMetadata \
          WHERE xmp IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .context("read Adobe_AdditionalMetadata")?;
    for r in rows {
        if let Some(image) = images.get_mut(&r.get::<i64, _>("image")) {
            image.xmp = r
                .try_get::<Option<Vec<u8>>, _>("xmp")
                .context("read an XMP packet")?;
        }
    }
    Ok(())
}

async fn load_video(pool: &SqlitePool, images: &mut BTreeMap<i64, ImageRow>) -> Result<()> {
    let rows = sqlx::query(
        "SELECT CAST(image AS INTEGER) AS image, CAST(duration AS TEXT) AS duration \
           FROM AgVideoInfo",
    )
    .fetch_all(pool)
    .await
    .context("read AgVideoInfo")?;
    for r in rows {
        if let Some(image) = images.get_mut(&r.get::<i64, _>("image")) {
            image.duration = non_empty(r.get("duration"));
        }
    }
    Ok(())
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}
