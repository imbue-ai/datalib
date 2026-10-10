//! `GET /api/pipeline/disk` — free space on the data root's disk, and the
//! config's `[disk_space]` lines between which the loop holds every step.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_dag::disk_space::Space;
use datalib_http::router;
use tower::ServiceExt;

use crate::support::{state, TEST_TOKEN};

async fn disk(app: &axum::Router) -> serde_json::Value {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/pipeline/disk")
                .header("x-datalib-token", TEST_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Before the first look the number is null, not zero — a zero would read
/// as a full disk — and the floor is read from the config as written.
/// Once a look lands under the floor the answer says `low`, the word the
/// status bar's toast keys on.
#[tokio::test]
async fn the_answer_names_the_floor_and_says_when_the_disk_is_under_it() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(
        td.path().join("config.toml"),
        "[disk_space]\npause_below_bytes = \"10 GB\"\nresume_at_bytes = \"15 GB\"\n",
    )
    .unwrap();
    let s = state(td.path()).await;
    let usage = s.usage.clone();
    let app = router(s);

    let v = disk(&app).await;
    assert_eq!(v["available_bytes"], serde_json::Value::Null);
    assert_eq!(v["pause_below_bytes"], 10_000_000_000u64);
    assert_eq!(v["resume_at_bytes"], 15_000_000_000u64);
    assert_eq!(v["low"], false);

    let space = Space {
        available: 3_000_000_000,
        total: 500_000_000_000,
    };
    let floor = datalib_dag::disk_space::DiskFloor {
        pause_below: 10_000_000_000,
        resume_at: 15_000_000_000,
    };
    usage
        .free
        .observe(space, floor, "2026-10-10T10:00:00-07:00")
        .await;
    let v = disk(&app).await;
    assert_eq!(v["available_bytes"], 3_000_000_000u64);
    assert_eq!(v["total_bytes"], 500_000_000_000u64);
    assert_eq!(v["low"], true);
    assert_eq!(v["history"].as_array().unwrap().len(), 1);
}
