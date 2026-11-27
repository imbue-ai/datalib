//! Read a mirrored catalog at one commit: every image, with everything
//! render shows of it. Lightroom keeps one image's facts in a dozen
//! tables keyed by `id_local`; each is read once and joined here.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_photo_schema::{Person, Place, Rect};
use datalib_sql::{sql, sql_id, Sql};

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
    "AgLibraryPublishedCollection",
    "AgLibraryPublishedCollectionImage",
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

/// A keyword or a collection a photo is filed under, with the keywords or
/// sets above it, top first: each its table's `id_local` and its name.
#[derive(Debug, Clone, PartialEq)]
pub struct Filed {
    /// The table the ids are in: `AgLibraryKeyword`, `AgLibraryCollection`
    /// or `AgLibraryPublishedCollection`.
    pub table: &'static str,
    pub path: Vec<(i64, String)>,
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
    /// The `AgLibraryFolder` id of each level of `folder`, top first, which
    /// a rename or a move inside Lightroom keeps; `None` for a level the
    /// catalog has no row for.
    pub folder_ids: Vec<Option<String>>,
    /// The root folder's `AgLibraryRootFolder` id.
    pub root_id: Option<String>,
    /// The root folder's `id_local`, which a level is looked up under.
    pub root_local: Option<i64>,
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
    /// Each collection it is in, as its path from the top: the sets
    /// above it, then its own name.
    pub albums: Vec<Vec<String>>,
    /// Each keyword and collection it is filed under, by the catalog's
    /// ids, which a rename keeps.
    pub filed: Vec<Filed>,
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
    load_folder_ids(pool, &mut images).await?;
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
    for tables in [COLLECTIONS, PUBLISHED] {
        if has(tables.collections) && has(tables.members) {
            load_albums(pool, &mut images, tables).await?;
        }
    }
    for image in images.values_mut() {
        image.albums.sort();
        image.albums.dedup();
        image
            .filed
            .sort_by(|a, b| (a.table, &a.path).cmp(&(b.table, &b.path)));
        image.filed.dedup();
    }
    if xmp_mirrored {
        load_xmp(pool, &mut images).await?;
    }
    if has("AgVideoInfo") {
        load_video(pool, &mut images).await?;
    }
    let catalog_path = if has("Adobe_variablesTable") {
        sql!(
            "lightroom_render.read_last_scanned_catalog_path",
            "SELECT CAST(value AS TEXT) FROM Adobe_variablesTable \
              WHERE name = 'Adobe_lastScannedCatalogPath' LIMIT 1"
        )
        .query_scalar()
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
    let rows = sql!(
        "lightroom_render.list_table_sql",
        "SELECT name, sql FROM sqlite_master WHERE type = 'table'"
    )
    .query()
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
    let rows = sql!(
        "lightroom_render.list_images",
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
                CAST(rf.id_global AS TEXT) AS root_id, \
                CAST(fo.rootFolder AS INTEGER) AS root_local, \
                CAST(rf.absolutePath AS TEXT) AS root_absolute, \
                CAST(rf.relativePathFromCatalog AS TEXT) AS root_relative \
           FROM Adobe_images i \
           JOIN AgLibraryFile f ON f.id_local = i.rootFile \
           LEFT JOIN AgLibraryFolder fo ON fo.id_local = f.folder \
           LEFT JOIN AgLibraryRootFolder rf ON rf.id_local = fo.rootFolder"
    )
    .query()
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
                root_id: non_empty(r.get("root_id")),
                root_local: r.get("root_local"),
                root_absolute: non_empty(r.get("root_absolute")),
                root_relative: non_empty(r.get("root_relative")),
                ..ImageRow::default()
            };
            (id_local, image)
        })
        .collect())
}

/// Each level of each photo's folder, by its `AgLibraryFolder` row: the
/// catalog keeps one per folder it knows, keyed by its root and its path
/// under it (`2369/`, `2369/Risa/`).
async fn load_folder_ids(pool: &SqlitePool, images: &mut BTreeMap<i64, ImageRow>) -> Result<()> {
    let rows = sql!(
        "lightroom_render.list_folder_ids",
        "SELECT CAST(rootFolder AS INTEGER) AS root, CAST(pathFromRoot AS TEXT) AS path, \
                CAST(id_global AS TEXT) AS id \
           FROM AgLibraryFolder"
    )
    .query()
    .fetch_all(pool)
    .await
    .context("read AgLibraryFolder")?;
    let ids: HashMap<(i64, String), String> = rows
        .into_iter()
        .filter_map(|r| {
            let root: Option<i64> = r.get("root");
            let path: Option<String> = r.get("path");
            let id: Option<String> = r.get("id");
            Some((
                (root?, path.unwrap_or_default()),
                id.filter(|i| !i.is_empty())?,
            ))
        })
        .collect();
    for image in images.values_mut() {
        let Some(root) = image.root_local else {
            continue;
        };
        image.folder_ids = folder_levels(image.folder.as_deref())
            .map(|(_, path)| ids.get(&(root, path)).cloned())
            .collect();
    }
    Ok(())
}

/// Each level of a folder path under its root, top first: its name, and
/// its path from the root as Lightroom writes it, with a trailing `/`.
pub fn folder_levels(folder: Option<&str>) -> impl Iterator<Item = (&str, String)> {
    let mut path = String::new();
    folder
        .unwrap_or("")
        .split('/')
        .filter(|p| !p.is_empty())
        .map(move |part| {
            path.push_str(part);
            path.push('/');
            (part, path.clone())
        })
}

async fn load_iptc(pool: &SqlitePool, images: &mut BTreeMap<i64, ImageRow>) -> Result<()> {
    let rows = sql!(
        "lightroom_render.list_iptc_captions",
        "SELECT CAST(image AS INTEGER) AS image, CAST(caption AS TEXT) AS caption, \
                CAST(copyright AS TEXT) AS copyright \
           FROM AgLibraryIPTC"
    )
    .query()
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
    let rows = sql!(
        "lightroom_render.list_exif",
        "SELECT CAST(e.image AS INTEGER) AS image, CAST(e.hasGPS AS INTEGER) AS has_gps, \
                CAST(e.gpsLatitude AS REAL) AS lat, CAST(e.gpsLongitude AS REAL) AS lng, \
                CAST(e.isoSpeedRating AS REAL) AS iso, CAST(e.shutterSpeed AS REAL) AS shutter, \
                CAST(e.aperture AS REAL) AS aperture, CAST(e.focalLength AS REAL) AS focal, \
                CAST(cm.value AS TEXT) AS camera, CAST(l.value AS TEXT) AS lens \
           FROM AgHarvestedExifMetadata e \
           LEFT JOIN AgInternedExifCameraModel cm ON cm.id_local = e.cameraModelRef \
           LEFT JOIN AgInternedExifLens l ON l.id_local = e.lensRef"
    )
    .query()
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
    let rows = sql!(
        "lightroom_render.list_iptc_places",
        "SELECT CAST(h.image AS INTEGER) AS image, \
                CAST(ci.value AS TEXT) AS city, CAST(st.value AS TEXT) AS state, \
                CAST(co.value AS TEXT) AS country, CAST(lo.value AS TEXT) AS location, \
                CAST(cr.value AS TEXT) AS creator \
           FROM AgHarvestedIptcMetadata h \
           LEFT JOIN AgInternedIptcCity ci ON ci.id_local = h.cityRef \
           LEFT JOIN AgInternedIptcState st ON st.id_local = h.stateRef \
           LEFT JOIN AgInternedIptcCountry co ON co.id_local = h.countryRef \
           LEFT JOIN AgInternedIptcLocation lo ON lo.id_local = h.locationRef \
           LEFT JOIN AgInternedIptcCreator cr ON cr.id_local = h.creatorRef"
    )
    .query()
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
    let tree: HashMap<i64, Keyword> = sql!(
        "lightroom_render.list_keywords",
        "SELECT CAST(id_local AS INTEGER) AS id, CAST(name AS TEXT) AS name, \
                CAST(parent AS INTEGER) AS parent, CAST(keywordType AS TEXT) AS kind \
           FROM AgLibraryKeyword"
    )
    .query()
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
    let path = |id: i64| {
        named_path(id, |id| {
            tree.get(&id).map(|k| (k.name.as_deref(), k.parent))
        })
    };

    if faces {
        let rows = sql!(
            "lightroom_render.list_face_regions",
            "SELECT CAST(f.image AS INTEGER) AS image, CAST(kf.tag AS INTEGER) AS tag, \
                    CAST(f.tl_x AS REAL) AS left_, CAST(f.tl_y AS REAL) AS top, \
                    CAST(f.br_x AS REAL) AS right_, CAST(f.br_y AS REAL) AS bottom \
               FROM AgLibraryFace f JOIN AgLibraryKeywordFace kf ON kf.face = f.id_local \
              WHERE COALESCE(CAST(kf.userPick AS REAL), 0) = 1 \
                AND COALESCE(CAST(kf.userReject AS REAL), 0) = 0 \
              ORDER BY f.image, f.id_local"
        )
        .query()
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

    let rows = sql!(
        "lightroom_render.list_image_keywords",
        "SELECT CAST(image AS INTEGER) AS image, CAST(tag AS INTEGER) AS tag \
           FROM AgLibraryKeywordImage ORDER BY image, tag"
    )
    .query()
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
            (Some(_), false) => {
                let path = path(tag);
                image
                    .keywords
                    .push(path.iter().map(|(_, n)| n.clone()).collect());
                image.filed.push(Filed {
                    table: "AgLibraryKeyword",
                    path,
                });
            }
            (None, _) => {}
        }
    }
    for image in images.values_mut() {
        image.keywords.sort();
        image.keywords.dedup();
    }
    Ok(())
}

/// A tree of collections and the table of their members. Lightroom keeps
/// two with the same shape: the library's collections (with their sets,
/// the quick collection and saved books, slideshows, prints and web
/// galleries) and the publish services' (Flickr, Hard Drive, …).
#[derive(Clone, Copy)]
struct CollectionTables {
    collections: &'static str,
    members: &'static str,
}

const COLLECTIONS: CollectionTables = CollectionTables {
    collections: "AgLibraryCollection",
    members: "AgLibraryCollectionImage",
};

const PUBLISHED: CollectionTables = CollectionTables {
    collections: "AgLibraryPublishedCollection",
    members: "AgLibraryPublishedCollectionImage",
};

/// What Lightroom names the quick collection, a system collection; it
/// shows it as "Quick Collection".
const QUICK_COLLECTION: &str = "quick collection";

struct Collection {
    name: Option<String>,
    parent: Option<i64>,
}

/// Every collection an image is a member of, by its path. A smart
/// collection is a saved search with no members, so it never appears.
async fn load_albums(
    pool: &SqlitePool,
    images: &mut BTreeMap<i64, ImageRow>,
    tables: CollectionTables,
) -> Result<()> {
    // Both table names are the constants above.
    let tree: HashMap<i64, Collection> = Sql::dynamic(
        sql_id!("lightroom_render.list_collections"),
        format!(
            "SELECT CAST(id_local AS INTEGER) AS id, CAST(name AS TEXT) AS name, \
                CAST(parent AS INTEGER) AS parent, \
                COALESCE(CAST(systemOnly AS REAL), 0.0) AS system \
           FROM {}",
            tables.collections
        ),
    )
    .query()
    .fetch_all(pool)
    .await
    .with_context(|| format!("read {}", tables.collections))?
    .into_iter()
    .filter_map(|r| {
        let name = non_empty(r.get("name"));
        let name = if r.get::<f64, _>("system") == 0.0 {
            name
        } else if name.as_deref() == Some(QUICK_COLLECTION) {
            Some("Quick Collection".to_string())
        } else {
            return None;
        };
        Some((
            r.get::<i64, _>("id"),
            Collection {
                name,
                parent: r.get("parent"),
            },
        ))
    })
    .collect();
    let path = |id: i64| {
        named_path(id, |id| {
            tree.get(&id).map(|c| (c.name.as_deref(), c.parent))
        })
    };
    let rows = Sql::dynamic(
        sql_id!("lightroom_render.list_collection_members"),
        format!(
            "SELECT DISTINCT CAST(image AS INTEGER) AS image, \
                CAST(collection AS INTEGER) AS collection FROM {}",
            tables.members
        ),
    )
    .query()
    .fetch_all(pool)
    .await
    .with_context(|| format!("read {}", tables.members))?;
    for r in rows {
        let collection: i64 = r.get("collection");
        if !tree.contains_key(&collection) {
            continue;
        }
        let path = path(collection);
        if let (Some(image), false) = (images.get_mut(&r.get::<i64, _>("image")), path.is_empty()) {
            image
                .albums
                .push(path.iter().map(|(_, n)| n.clone()).collect());
            if !image
                .filed
                .iter()
                .any(|f| f.table == tables.collections && f.path == path)
            {
                image.filed.push(Filed {
                    table: tables.collections,
                    path,
                });
            }
        }
    }
    Ok(())
}

/// The named entries from the top of a tree down to `id`, by following
/// each entry's parent. One with no name (Lightroom's hidden root
/// keyword) is walked through; a loop stops the walk.
fn named_path<'t>(
    mut id: i64,
    entry: impl Fn(i64) -> Option<(Option<&'t str>, Option<i64>)>,
) -> Vec<(i64, String)> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    while let Some((name, parent)) = entry(id) {
        if !seen.insert(id) {
            break;
        }
        if let Some(name) = name {
            out.push((id, name.to_string()));
        }
        match parent {
            Some(p) => id = p,
            None => break,
        }
    }
    out.reverse();
    out
}

async fn load_xmp(pool: &SqlitePool, images: &mut BTreeMap<i64, ImageRow>) -> Result<()> {
    let rows = sql!(
        "lightroom_render.list_xmp",
        "SELECT CAST(image AS INTEGER) AS image, xmp FROM Adobe_AdditionalMetadata \
          WHERE xmp IS NOT NULL"
    )
    .query()
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
    let rows = sql!(
        "lightroom_render.list_video_durations",
        "SELECT CAST(image AS INTEGER) AS image, CAST(duration AS TEXT) AS duration \
           FROM AgVideoInfo"
    )
    .query()
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

#[cfg(test)]
mod tests {
    use super::*;

    use datalib_sql::{test_sql, test_sql_id, Sql};
    /// A collection inside a set is named by its path; the quick
    /// collection is one; a smart collection, with no members, and any
    /// other system collection are not; a publish service's collections
    /// are read the same way from their own tables.
    #[tokio::test]
    async fn every_collection_with_members_is_an_album_by_its_path() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        for table in ["AgLibraryCollection", "AgLibraryPublishedCollection"] {
            Sql::dynamic(
                test_sql_id!(),
                format!(
                    "CREATE TABLE {table} (id_local INTEGER PRIMARY KEY, creationId, \
                 name, parent INTEGER, systemOnly)"
                ),
            )
            .query()
            .execute(&pool)
            .await
            .unwrap();
            Sql::dynamic(
                test_sql_id!(),
                format!(
                    "CREATE TABLE {table}Image (id_local INTEGER PRIMARY KEY, \
                 collection INTEGER, image INTEGER)"
                ),
            )
            .query()
            .execute(&pool)
            .await
            .unwrap();
        }
        test_sql!(
            "INSERT INTO AgLibraryCollection VALUES \
               (1, 'com.adobe.ag.library.group', 'Shore leave', NULL, '0.0'), \
               (2, 'com.adobe.ag.library.collection', 'Risa', 1, '0.0'), \
               (3, 'com.adobe.ag.library.collection', 'quick collection', NULL, '1.0'), \
               (4, 'com.adobe.ag.library.smart_collection', 'Five Stars', NULL, '0.0'), \
               (5, 'com.adobe.ag.library.collection', 'hidden', NULL, '1.0'); \
             INSERT INTO AgLibraryCollectionImage VALUES (1, 2, 10), (2, 3, 10), (3, 5, 10); \
             INSERT INTO AgLibraryPublishedCollection VALUES \
               (1, 'com.adobe.ag.library.group', 'Flickr', NULL, '0.0'), \
               (2, 'com.adobe.ag.library.collection', 'Photostream', 1, '0.0'); \
             INSERT INTO AgLibraryPublishedCollectionImage VALUES (1, 2, 10), (2, 2, 10);"
        )
        .query()
        .execute(&pool)
        .await
        .unwrap();
        let mut images: BTreeMap<i64, ImageRow> = [(10, ImageRow::default())].into();
        load_albums(&pool, &mut images, COLLECTIONS).await.unwrap();
        load_albums(&pool, &mut images, PUBLISHED).await.unwrap();
        let filed: Vec<(&str, Vec<i64>)> = images[&10]
            .filed
            .iter()
            .map(|f| (f.table, f.path.iter().map(|(id, _)| *id).collect()))
            .collect();
        assert_eq!(
            filed,
            [
                ("AgLibraryCollection", vec![1, 2]),
                ("AgLibraryCollection", vec![3]),
                ("AgLibraryPublishedCollection", vec![1, 2]),
            ],
            "each collection by its ids, the set above it first"
        );
        let mut albums = images[&10].albums.clone();
        albums.sort();
        assert_eq!(
            albums,
            [
                vec!["Flickr".to_string(), "Photostream".to_string()],
                vec!["Quick Collection".to_string()],
                vec!["Shore leave".to_string(), "Risa".to_string()],
            ]
        );
    }
}

#[cfg(test)]
mod folder_tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    use datalib_sql::test_sql;
    /// Each level of a photo's folder is found by its root and its path
    /// from it, and a level with no row is none.
    #[tokio::test]
    async fn each_level_of_a_folder_is_found_by_its_root_and_path() {
        // One connection: each connection to `:memory:` is a database of its own.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        test_sql!(
            "CREATE TABLE AgLibraryFolder (id_local INTEGER, id_global TEXT, \
             pathFromRoot TEXT, rootFolder INTEGER)"
        )
        .query()
        .execute(&pool)
        .await
        .unwrap();
        test_sql!(
            "INSERT INTO AgLibraryFolder VALUES \
             (1, 'F-ROOT', '', 10), (2, 'F-2369', '2369/', 10), \
             (3, 'F-RISA', '2369/Risa/', 10), (4, 'F-ELSEWHERE', '2369/', 11)"
        )
        .query()
        .execute(&pool)
        .await
        .unwrap();
        let image = |folder: &str| ImageRow {
            root_local: Some(10),
            folder: Some(folder.into()),
            ..ImageRow::default()
        };
        let mut images = BTreeMap::from([(1, image("2369/Risa/")), (2, image("2369/Vulcan/"))]);
        load_folder_ids(&pool, &mut images).await.unwrap();
        let id = |s: &str| Some(s.to_string());
        assert_eq!(images[&1].folder_ids, vec![id("F-2369"), id("F-RISA")]);
        assert_eq!(images[&2].folder_ids, vec![id("F-2369"), None]);
        pool.close().await;
    }
}
