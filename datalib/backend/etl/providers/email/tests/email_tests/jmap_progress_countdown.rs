//! The Fastmail (JMAP) half of what `progress_countdown` asserts for
//! Gmail: the download announces a total big enough to count down from,
//! and every phase's ticks add up to it.
//!
//! This path used to announce `5` — its phase count — so a mailbox of
//! forty thousand messages counted down from four. `Email/query` is
//! already sent `calculateTotal: true` and its `total` was read only to
//! decide when to stop paginating. The total announced now is exact: the
//! emails the store owes, then the `.eml`s it lacks.

use std::sync::Arc;

use datalib_etl::progress::Progress;
use datalib_etl_email::ingest::FetchOptions;

use crate::jmap_tape::{Account, Tape, HOST};
use crate::support::{Mirror, Recorder};

/// Enough to cross `Email/get`'s batch of 50, so the test also covers a
/// run whose ticks arrive in more than one chunk.
const MESSAGES: usize = 60;

/// Session, mailboxes, emails, blobs — the four coarse ticks the outer
/// bar makes whatever a run turns out to hold. Mirrors `ingest::PHASES`,
/// which is private.
const PHASES: u64 = 4;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fastmail_enumeration_counts_down_from_its_message_count() {
    let m = Mirror::new();
    write_fixtures(&m.playback);

    let recorder = Recorder::default();
    let summary = m
        .run(|db| {
            let mut opts = FetchOptions::new(db);
            opts.hostname = HOST.to_string();
            opts.progress = Progress::new(Arc::new(recorder.clone()));
            datalib_etl_email::ingest::fetch(opts)
        })
        .await;

    let summary = summary.expect("jmap fetch under playback");
    assert_eq!(summary.emails_upserted, MESSAGES);
    assert_eq!(
        summary.blobs_downloaded, MESSAGES,
        "every message should have contributed one `.eml` download: {summary:?}",
    );

    let announced = recorder.announcements();
    // The old behaviour, and the thing this test exists to catch: the
    // only total ever announced was the phase count.
    assert!(
        announced.iter().any(|t| *t > PHASES),
        "the run never announced a total bigger than its {PHASES} phases, \
         so \"N queued\" counts down from four however big the mailbox is \
         (announced: {announced:?})",
    );
    // Messages fetched, plus one `.eml` download each, plus the
    // phases. Exact rather than a lower bound: a total that overshoots
    // leaves the chip stuck above zero when the run ends.
    let expected = PHASES + MESSAGES as u64 * 2;
    let last = *announced.last().expect("a total was announced");
    assert_eq!(
        last, expected,
        "the last total announced was {last}, not {expected} \
         ({PHASES} phases + {MESSAGES} messages + {MESSAGES} blobs); \
         the chip would not reach zero (announced: {announced:?})",
    );
    assert_eq!(
        recorder.final_done(),
        expected,
        "the run ticked {} of the {expected} it announced, so \"N queued\" \
         ends the run above zero",
        recorder.final_done(),
    );
}

fn write_fixtures(out: &std::path::Path) {
    let ids: Vec<String> = (0..MESSAGES).map(|i| format!("M{i:04}")).collect();
    let emails: Vec<(&str, &[&str])> = ids.iter().map(|id| (id.as_str(), &["MB1"][..])).collect();
    let account = Account::new(&[("MB1", "Inbox")], &emails);
    let tape = Tape::new(out);
    tape.serve(&account);
    for batch in ids.chunks(50) {
        tape.gets(
            &account,
            &batch.iter().map(String::as_str).collect::<Vec<_>>(),
        );
    }
}
