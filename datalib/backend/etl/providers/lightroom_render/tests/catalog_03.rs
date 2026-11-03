//! Catalog 03 of lightroom_db_diff, mirrored the way the ingest step
//! mirrors it and read back as photos: what each photo's row, terms and
//! `NormalizedPhoto` say, checked against what the catalog holds.

use std::collections::BTreeMap;
use std::path::PathBuf;

use datalib_etl::doltlite_raw as dr;
use datalib_etl::progress::Progress;
use datalib_etl_lightroom::ingest::{self, mirror, FetchOptions, MirrorOptions};
use datalib_etl_lightroom_render::photo::{self, Built, Context};
use datalib_etl_lightroom_render::read;
use datalib_photo_schema::{MediaKind, OffsetFrom, Pictures};
use datalib_schema::search_terms::SearchTermKind;

fn catalog() -> PathBuf {
    let p = PathBuf::from(
        std::env::var("LIGHTROOM_CATALOG_03").expect("LIGHTROOM_CATALOG_03 names the catalog"),
    );
    assert!(p.exists(), "catalog missing at {}", p.display());
    p
}

/// Every photo, by file name.
async fn photos(zone: Option<&str>) -> BTreeMap<String, Built> {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("entities.doltlite_db");
    let pool = mirror::open_mirror(&store).await.unwrap();
    ingest::fetch(FetchOptions {
        mirror_path: store.clone(),
        pool: Some(pool.clone()),
        options: MirrorOptions {
            stable_key_columns: vec!["id_global".to_string()],
            ..MirrorOptions::new(catalog())
        },
        progress: Progress::noop(),
    })
    .await
    .unwrap();
    dr::commit_run(&pool, "lightroom: catalog 03")
        .await
        .unwrap();
    pool.close().await;

    let reader = dr::open_reader(&store, None)
        .await
        .unwrap()
        .expect("a commit");
    let catalog = read::load(reader.pool()).await.unwrap();
    reader.close().await;
    assert!(catalog.xmp_mirrored);
    assert!(
        catalog
            .catalog_path
            .as_deref()
            .is_some_and(|p| p.ends_with("test_catalog_more_face_tags_gps_edit.lrcat")),
        "{:?}",
        catalog.catalog_path
    );

    let zone = zone.map(|z| datalib_time::NamedZone::get(z).unwrap());
    catalog
        .images
        .values()
        .map(|image| {
            let built = photo::build(
                image,
                &Context {
                    source_id: "lr",
                    qmd_path: "lr/render_markdown/xx/doc.md",
                    catalog: catalog.catalog_path.as_deref(),
                    zone: zone.as_ref(),
                },
            );
            (built.photo.file_name.clone(), built)
        })
        .collect()
}

fn terms(b: &Built, kind: SearchTermKind) -> Vec<String> {
    let row = b.row.as_ref().expect("a row");
    let mut out: Vec<String> = row
        .supplied_terms()
        .unwrap()
        .into_iter()
        .filter(|t| t.kind == kind)
        .map(|t| t.value)
        .collect();
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn every_photo_becomes_a_document_row() {
    let photos = photos(None).await;
    assert_eq!(photos.len(), 7, "{:?}", photos.keys().collect::<Vec<_>>());
    for (name, b) in &photos {
        let row = b
            .row
            .as_ref()
            .unwrap_or_else(|| panic!("{name}: {:?}", b.problems));
        assert!(row.is_document);
        assert_eq!(row.kind, "Photo");
        assert_eq!(row.item_count, Some(1));
        assert_eq!(row.markdown_uuid.as_deref(), Some(row.uuid.as_str()));
        assert_eq!(row.image, Some(format!("datalib:photo/{}", row.uuid)));
        assert_eq!(b.photo.kind, MediaKind::Photo);
    }
}

/// The Golden Gate photo: its offset from the XMP, its title only the
/// XMP holds, Lightroom's own place names, the cells of its fix, its
/// exposure out of APEX, its keyword and album.
#[tokio::test(flavor = "multi_thread")]
async fn the_golden_gate_photo_says_what_the_catalog_holds() {
    let photos = photos(None).await;
    let b = &photos["GoldenGateBridge-001.jpg"];
    let p = &b.photo;
    assert_eq!(p.captured_at.as_deref(), Some("2007-01-01T16:41:39-08:00"));
    assert_eq!(p.offset_from, Some(OffsetFrom::DateTimeOriginal));
    assert_eq!(p.title.as_deref(), Some("Golden Gate Bridge 001"));
    assert_eq!(p.headline.as_deref(), Some("Golden Gate Bridge"));
    assert_eq!(
        p.caption.as_deref(),
        Some("The Golden Gate Bridge in San Francisco, CA at sunset.  Taken from Marin.")
    );
    assert_eq!(p.place.city.as_deref(), Some("San Francisco"));
    assert_eq!(p.place.location.as_deref(), Some("Golden Gate Bridge"));
    assert_eq!(p.camera.as_deref(), Some("DSC-W100"));
    assert_eq!(p.exposure.aperture, Some(2.8));
    let shutter = p.exposure.shutter_seconds.unwrap();
    assert!((shutter - 1.0 / 400.0).abs() < 1e-5, "{shutter}");
    assert_eq!(p.color_label.as_deref(), Some("Red"));
    assert_eq!(p.albums, ["Landmarks"]);
    assert!(p.keywords.contains(&vec!["golden gate bridge".to_string()]));
    let geo = p.geo.expect("a fix");
    assert!((geo.latitude - 37.8275).abs() < 1e-3);
    assert_eq!(p.h3.len(), 4);
    assert!(b.problems.is_empty(), "{:?}", b.problems);

    let row = b.row.as_ref().unwrap();
    assert_eq!(
        row.conversation_name.as_deref(),
        Some("Golden Gate Bridge 001")
    );
    assert_eq!(row.created_at, p.captured_at);
    assert_eq!(
        row.touched_at, p.captured_at,
        "newest taken, not newest edited"
    );
    assert!(row
        .modified_at
        .as_deref()
        .is_some_and(|m| m.starts_with("2026-11-02")));
    assert_eq!(row.latitude, Some(geo.latitude));
    let places = terms(b, SearchTermKind::Place);
    assert!(places.contains(&"San Francisco".to_string()), "{places:?}");
    let located = row.h3.expect("a location");
    for cell in &p.h3 {
        assert!(
            !places.contains(&cell.cell),
            "cells are not terms: {places:?}"
        );
        let cell = datalib_h3::parse(&cell.cell).unwrap();
        assert!(datalib_h3::finest_range(cell).contains(&located));
    }
    assert_eq!(
        terms(b, SearchTermKind::Caption),
        [p.caption.clone().unwrap()]
    );
    assert!(terms(b, SearchTermKind::Label).contains(&"Landmarks".to_string()));

    match &p.pictures {
        Pictures::Lightroom { file_uuid, .. } => {
            assert_eq!(file_uuid, "90A53874-ACC1-45EE-A843-81BFFE893F0A")
        }
    }
    assert!(p
        .original
        .relative_to_catalog
        .as_deref()
        .is_some_and(|r| r.ends_with("test_photos/GoldenGateBridge-001.jpg")));
}

/// People tagged on faces carry their rectangles; a keyword's parents
/// are labels too; the quick collection is not an album.
#[tokio::test(flavor = "multi_thread")]
async fn faces_keyword_trees_and_albums() {
    let photos = photos(None).await;
    let band = &photos["1_Live_Krone_2013_dth_1.jpg"].photo;
    let mut names: Vec<&str> = band.people.iter().map(|p| p.name.as_str()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "Andreas Meurer",
            "Andreas von Holst",
            "Campino",
            "Michael Breitkopf",
            "Ritchie"
        ]
    );
    assert!(band.people.iter().all(|p| p.face.is_some()));
    assert_eq!(band.creator.as_deref(), Some("Schlappal"));
    assert_eq!(band.offset_from, Some(OffsetFrom::CreateDate));
    assert_eq!(band.albums, ["Helden"], "the quick collection is left out");

    let king = &photos["Martin_Luther_King,_Jr..jpg"];
    assert!(king.photo.keywords.contains(&vec![
        "Nobelpris".to_string(),
        "Nobelpristagare".to_string(),
        "Fred".to_string()
    ]));
    let labels = terms(king, SearchTermKind::Label);
    for label in ["Nobelpris", "Fred", "Martin Luther King Jr.", "Helden"] {
        assert!(labels.contains(&label.to_string()), "{label}: {labels:?}");
    }
}

/// A photo whose metadata records no offset is read as UTC and carries a
/// warning, until the source names a zone.
#[tokio::test(flavor = "multi_thread")]
async fn a_photo_with_no_recorded_offset_uses_the_configured_zone() {
    let assumed = photos(None).await;
    let hesse = &assumed["Hermann_Hesse_2.jpg"];
    assert_eq!(hesse.photo.offset_from, Some(OffsetFrom::AssumedUtc));
    assert_eq!(hesse.problems.len(), 1, "{:?}", hesse.problems);

    let configured = photos(Some("Europe/Berlin")).await;
    let hesse = &configured["Hermann_Hesse_2.jpg"];
    assert_eq!(hesse.photo.offset_from, Some(OffsetFrom::ConfiguredZone));
    assert_eq!(
        hesse.photo.captured_at.as_deref(),
        Some("2020-06-01T01:49:28+02:00")
    );
    assert!(hesse.problems.is_empty());
}
