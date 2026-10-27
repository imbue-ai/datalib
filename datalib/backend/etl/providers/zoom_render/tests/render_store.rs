//! Meetings written into a real raw store the way the download writes
//! them, then read back and rendered: the store reads (JSONB payloads)
//! and the incremental pass are what the unit tests cannot reach.

use std::collections::HashSet;
use std::path::Path;

use datalib_etl::doltlite_raw::commit_run;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::RawRange;
use datalib_etl_zoom::ingest::db::{self, payload};
use datalib_etl_zoom::ingest::schema_raw::{
    ParticipantsRow, RecordingRow, ReportedMeetingRow, SummaryRow, UserRow,
};
use datalib_etl_zoom::ingest::RawDb;
use datalib_etl_zoom_render::processor::render_store;
use serde_json::{json, Value};

const BRIEFING: &str = "nCC1701+D/briefing==";
const ENGINEERING: &str = "Eng1neering+Status==";
const HOLODECK: &str = "//Holodeck3Session==";

fn recording(uuid: &str, topic: &str, start: &str, files: Value) -> RecordingRow {
    RecordingRow {
        start_time: Some(start.to_string()),
        id_and_payload: payload(
            uuid,
            &json!({"uuid": uuid, "id": 85017010, "host_id": "z8PicardUid0001", "topic": topic,
                    "start_time": start, "duration": 45,
                    "share_url": "https://zoom.us/rec/share/senior-staff-briefing",
                    "recording_files": files}),
        )
        .unwrap(),
    }
}

async fn seed(store: &Path, summary: &str) {
    let db = RawDb::open(store).await.unwrap();
    let mut tx = db.pool().begin().await.unwrap();
    db::store_rows(
        &mut tx,
        &[UserRow {
            id_and_payload: payload(
                "z8PicardUid0001",
                &json!({"id": "z8PicardUid0001", "email": "picard@enterprise.example"}),
            )
            .unwrap(),
        }],
    )
    .await
    .unwrap();
    db::store_listed(
        &mut tx,
        &[
            recording(
                BRIEFING,
                "Senior staff briefing",
                "2026-10-05T09:00:00Z",
                json!([{"id": "a-vtt", "file_type": "TRANSCRIPT", "recording_type": "audio_transcript",
                        "download_url": "https://zoom.us/rec/download/a-vtt"},
                       {"id": "a-chat", "file_type": "CHAT", "recording_type": "chat_file",
                        "download_url": "https://zoom.us/rec/download/a-chat"}]),
            ),
            recording(
                ENGINEERING,
                "Engineering status",
                "2026-09-30T14:00:00Z",
                json!([]),
            ),
        ],
    )
    .await
    .unwrap();
    db::store_listed(
        &mut tx,
        &[ReportedMeetingRow {
            start_time: Some("2026-09-10T20:00:00Z".to_string()),
            id_and_payload: payload(
                HOLODECK,
                &json!({"uuid": HOLODECK, "topic": "Holodeck 3: Dixon Hill",
                        "start_time": "2026-09-10T20:00:00Z", "duration": 90,
                        "user_name": "Jean-Luc Picard"}),
            )
            .unwrap(),
        }],
    )
    .await
    .unwrap();
    db::store_rows(
        &mut tx,
        &[
            ParticipantsRow {
                id_and_payload: payload(
                    BRIEFING,
                    &json!({"participants": [
                        {"name": "Jean-Luc Picard", "user_email": "picard@enterprise.example", "duration": 2700},
                        {"name": "William Riker", "user_email": "riker@enterprise.example", "duration": 2700}]}),
                )
                .unwrap(),
            },
            ParticipantsRow {
                id_and_payload: payload(
                    HOLODECK,
                    &json!({"participants": [], "unavailable": "Meeting does not exist"}),
                )
                .unwrap(),
            },
        ],
    )
    .await
    .unwrap();
    db::store_rows(
        &mut tx,
        &[SummaryRow {
            id_and_payload: payload(
                BRIEFING,
                &json!({"summary_created_time": "2026-10-05T09:50:00Z",
                        "summary_content": summary}),
            )
            .unwrap(),
        }],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    commit_run(db.pool(), "test fixture").await.unwrap();
    db.close().await;
}

async fn render(
    store: &Path,
    root: &Path,
    cursor: Option<&str>,
    stale: Option<&HashSet<String>>,
) -> (Vec<RenderedMarkdown>, String) {
    let db = RawDb::open_reader(store, None)
        .await
        .unwrap()
        .expect("committed");
    let head = db.pin().unwrap().commit().to_string();
    let range = RawRange {
        cursor,
        pin: Some(&head),
        stale,
    };
    let mut docs = Vec::new();
    let mut on_doc = |md: RenderedMarkdown| {
        docs.push(md);
        Ok(())
    };
    render_store(&db, root, "zoom", range, &Default::default(), &mut on_doc)
        .await
        .unwrap();
    db.close().await;
    (docs, head)
}

fn doc<'a>(docs: &'a [RenderedMarkdown], uuid: &str) -> &'a RenderedMarkdown {
    let key = datalib_etl_zoom_render::ids::meeting("zoom", uuid).uuid;
    docs.iter()
        .find(|d| d.markdown_uuid == key)
        .unwrap_or_else(|| panic!("no document for {uuid}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_meeting_renders_with_who_came_and_its_summary() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("ingest").join("entities.doltlite_db");
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    seed(
        &store,
        "## Key takeaways\n- An anomaly lies off the port bow.",
    )
    .await;

    let (docs, _) = render(&store, dir.path(), None, None).await;
    assert_eq!(docs.len(), 3);

    let briefing = doc(&docs, BRIEFING);
    let md = std::fs::read_to_string(&briefing.md_path).unwrap();
    assert!(md.contains("Senior staff briefing"), "{md}");
    assert!(md.contains("An anomaly lies off the port bow."), "{md}");
    assert!(
        md.contains("[William Riker](mailto:riker@enterprise.example)"),
        "{md}"
    );
    assert!(
        md.contains("Zoom keeps:** audio transcript, chat file"),
        "{md}"
    );
    assert!(!md.contains("/blob/"), "no recording file is linked: {md}");
    let kinds: HashSet<&str> = briefing.rows.iter().map(|r| r.kind.as_str()).collect();
    assert!(kinds.contains("Zoom Meeting Details"), "{kinds:?}");
    assert!(kinds.contains("Zoom Meeting Summary"), "{kinds:?}");
    assert!(briefing.rows.iter().any(|r| r.is_document));

    let holodeck = doc(&docs, HOLODECK);
    let md = std::fs::read_to_string(&holodeck.md_path).unwrap();
    assert!(md.contains("Zoom would not say who attended"), "{md}");
    assert!(
        md.contains("Jean-Luc Picard"),
        "the report names the host: {md}"
    );
}

/// Only the meeting whose rows moved renders again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_render_redoes_only_the_meeting_that_changed() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("ingest").join("entities.doltlite_db");
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    seed(&store, "## Key takeaways\n- An anomaly.").await;
    let (_, first) = render(&store, dir.path(), None, None).await;

    let none = HashSet::new();
    let (docs, _) = render(&store, dir.path(), Some(&first), Some(&none)).await;
    assert!(docs.is_empty(), "nothing moved");

    seed(&store, "## Key takeaways\n- Two anomalies.").await;
    let (docs, _) = render(&store, dir.path(), Some(&first), Some(&none)).await;
    assert_eq!(docs.len(), 1);
    let md = std::fs::read_to_string(&doc(&docs, BRIEFING).md_path).unwrap();
    assert!(md.contains("Two anomalies."), "{md}");
}
