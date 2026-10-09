//! Local-filesystem vCard ingest.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::warn;

use datalib_etl::control::DownloadControl;
use datalib_etl::progress::Progress;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl_files::file_checkpoint;
use datalib_etl_files::fingerprint_cache::FingerprintCache;
use datalib_etl_files::fsscan;

use super::api::{split_vcards, vcard_fn, vcard_n_family_given, vcard_rev, vcard_uid};
use super::db::{addressbook_pk, RawDb};
use super::schema_raw::{synthesized_name_uid_nth, ContactRow};

pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    pub input_path: PathBuf,
    /// Host-wide fingerprint cache — the shared answer to "did this
    /// file change?", so an unchanged `.vcf` costs a `stat`.
    pub cache: FingerprintCache,
    /// Overrides the synthetic `account_id`. Defaults to `"local"`.
    pub account_id_override: Option<String>,
    pub progress: Progress,
    pub control: DownloadControl,
}

#[derive(Debug, Default, Clone)]
pub struct FetchSummary {
    pub addressbooks: usize,
    pub contacts_new: usize,
    pub contacts_updated: usize,
    /// Contacts dropped: those a re-read `.vcf` file no longer carried, and
    /// every contact of a file that is gone.
    pub contacts_deleted: usize,
    /// `.vcf` files that are gone, their address books with them.
    pub files_removed: usize,
    /// `.vcf` files whose contents matched the resume cursor and were
    /// skipped without re-parsing.
    pub files_skipped: usize,
    pub errors: usize,
}

/// `file_checkpoint` scope for the local-`.vcf` resume cursor. One
/// contacts DB serves one source, so a single feed name suffices; each
/// `.vcf` file is namespaced by its canonical path within the scope.
const CHECKPOINT_SCOPE: &str = "contacts/vcf";

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    run_problems::collecting(&pool, &stop, |found| read_folder(opts, found)).await
}

async fn read_folder(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let db = opts.db.clone();

    let account_id = opts
        .account_id_override
        .clone()
        .unwrap_or_else(|| "local".to_string());
    let server_url = format!("file://{}", opts.input_path.display());
    db.upsert_account(&account_id, &server_url, None, None)
        .await?;

    // One scan answers both questions at once: which `.vcf` files are
    // there, and which have changed since this source last finished
    // with them. The walk, the hashing, and the decision not to re-hash
    // what the host cache can vouch for all live in the utility.
    let scan = fsscan::scan(
        &opts.cache,
        &opts.input_path,
        &fsscan::ScanOptions::default(),
        |p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("vcf")),
    )
    .await?;

    let mut summary = FetchSummary::default();
    summary.errors += scan.errors.len();
    for e in &scan.errors {
        warn!(event = "contacts_vcf_walk_error", path = %e.path.display(), error = %e.error, "an entry of the vcf directory could not be walked");
    }

    let prev = file_checkpoint::load_cursor(db.pool(), CHECKPOINT_SCOPE).await?;
    let changes = scan.changes_since(&prev);
    summary.files_skipped = changes.unchanged;
    opts.progress.set_length(Some(scan.files.len() as u64));
    opts.progress.inc(changes.unchanged as u64);

    let mut read: BTreeSet<&str> = BTreeSet::new();
    scan.report_problems(&found, "files");
    for f in changes.needs_reading_by_path() {
        opts.progress
            .set_message(&format!("ingesting {}", f.path.display()));
        match ingest_one(
            &db,
            &scan.given_resolved,
            &f.path,
            &account_id,
            &mut summary,
        )
        .await
        {
            Ok(()) => {
                // Stamp only after a clean ingest, so a crash mid-file
                // leaves no cursor and the next run re-ingests it.
                file_checkpoint::record_file_pool(db.pool(), CHECKPOINT_SCOPE, f).await?;
                read.insert(f.rel.as_str());
            }
            // Not stamped, so the next run reads it again.
            Err(e) => {
                summary.errors += 1;
                found.listing(&format!("vcf {}", f.rel), format!("{e:#}"));
            }
        }
        opts.progress.inc(1);
    }

    // A `.vcf` file is a whole address book, so a file that is gone takes
    // its address book with it.
    for rel in changes.gone_by_path(&read) {
        let href = relative_href(&scan.given_resolved, &scan.root.join(rel));
        let book_id = addressbook_pk(&account_id, &href);
        summary.contacts_deleted += db
            .delete_file_addressbook(&book_id, CHECKPOINT_SCOPE, rel)
            .await?;
        summary.files_removed += 1;
    }
    Ok(summary)
}

async fn ingest_one(
    db: &RawDb,
    root: &Path,
    file: &Path,
    account_id: &str,
    summary: &mut FetchSummary,
) -> Result<()> {
    let body = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let cards = whole_cards(&body)?;
    let label = addressbook_label(file);
    let book_href = relative_href(root, file);
    let book_id = addressbook_pk(account_id, &book_href);
    db.upsert_addressbook(account_id, &book_href, Some(&label), None, None)
        .await?;
    summary.addressbooks += 1;

    let existing = db.contact_uids(&book_id).await?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut rows: Vec<ContactRow> = Vec::new();
    // How many cards of each synthesized name this file has had so far,
    // so a second "John Smith" gets an id of his own.
    let mut synth_seen: HashMap<String, usize> = HashMap::new();
    for (idx, block) in cards.into_iter().enumerate() {
        let href = if idx == 0 {
            book_href.clone()
        } else {
            format!("{book_href}#{idx}")
        };
        let uid = contact_uid(file, &label, idx, &block, &mut synth_seen);
        seen.insert(uid.clone());
        if existing.contains(&uid) {
            summary.contacts_updated += 1;
        } else {
            summary.contacts_new += 1;
        }
        rows.push(ContactRow::new(
            book_id.clone(),
            uid,
            href,
            None,
            vcard_fn(&block),
            vcard_rev(&block),
            &block,
        ));
    }
    db.upsert_contacts(&rows).await?;
    // A `.vcf` file is the whole address book: a card it no longer
    // carries was deleted, and the mirror says so.
    let mut gone: Vec<String> = existing.difference(&seen).cloned().collect();
    gone.sort();
    summary.contacts_deleted += gone.len();
    db.delete_contacts_by_uid(&book_id, &gone).await?;
    Ok(())
}

/// The file's cards, or why it cannot stand for its whole address book.
/// vCard has no envelope that could say "no cards", so a file holding none
/// is unreadable rather than empty: deleting the file is what empties a
/// book.
fn whole_cards(body: &str) -> Result<Vec<String>> {
    let cards = split_vcards(body);
    if cards.is_empty() {
        bail!("the file holds no vCard (BEGIN:VCARD … END:VCARD)");
    }
    let begun = body
        .split(['\r', '\n'])
        .filter(|l| l.trim().eq_ignore_ascii_case("BEGIN:VCARD"))
        .count();
    if begun != cards.len() {
        bail!("a vCard in the file has no END:VCARD: a copy cut off part-way");
    }
    Ok(cards)
}

/// Stable id for one vCard, in priority order:
fn contact_uid(
    file: &Path,
    label: &str,
    idx: usize,
    block: &str,
    synth_seen: &mut HashMap<String, usize>,
) -> String {
    if let Some(uid) = vcard_uid(block) {
        return uid;
    }
    // No UID: try first + last name. Fall back to `FN` (whole formatted
    // name in the "given" slot) when the structured `N` line is absent.
    let (family, given) = vcard_n_family_given(block)
        .or_else(|| vcard_fn(block).map(|fnv| (String::new(), fnv)))
        .unwrap_or_default();
    if given.trim().is_empty() && family.trim().is_empty() {
        warn!(
            event = "contacts_vcf_nameless_contact",
            path = %file.display(),
            index = idx,
            "vCard has no UID and no name; keying on file position — \
             object permanence not available for this contact",
        );
        return format!("{label}:{}:{idx}", file_stem_or_anon(file));
    }
    let nth = synth_seen
        .entry(synthesized_name_uid_nth(&given, &family, 1))
        .or_insert(0);
    *nth += 1;
    synthesized_name_uid_nth(&given, &family, *nth)
}

fn addressbook_label(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "default".to_string())
}

fn file_stem_or_anon(path: &Path) -> &str {
    path.file_stem().and_then(|s| s.to_str()).unwrap_or("anon")
}

fn relative_href(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .ok()
        .and_then(|p| p.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            file.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("contacts.vcf")
                .to_string()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fingerprint cache in a throwaway directory, so no test ever
    /// touches — or is influenced by — the host's real one. The temp
    /// dir is leaked deliberately: its lifetime should be the test
    /// process, and a guard threaded through every constructor would
    /// only obscure what these tests are about.
    async fn test_cache() -> FingerprintCache {
        let d = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        FingerprintCache::open(&d.path().join("fpcache.sqlite"))
            .await
            .unwrap()
    }

    fn options(db: &RawDb, input: &Path, cache: FingerprintCache) -> FetchOptions {
        FetchOptions {
            db: db.clone(),
            input_path: input.to_path_buf(),
            cache,
            account_id_override: None,
            progress: Progress::default(),
            control: DownloadControl::default(),
        }
    }

    async fn contact_count(db: &RawDb) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM contacts")
            .fetch_one(db.pool())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn fetch_walks_directory_and_writes_rows() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Bridge.vcf"),
            "BEGIN:VCARD\nVERSION:3.0\nUID:picard\nFN:Jean-Luc Picard\nEND:VCARD\n\
             BEGIN:VCARD\nVERSION:3.0\nUID:riker\nFN:William Riker\nEND:VCARD\n",
        )
        .unwrap();
        let db_path = dir.path().join("c.doltlite_db");
        let cache = test_cache().await;
        let db = RawDb::open(&db_path).await.unwrap();
        let opts = || options(&db, dir.path(), cache.clone());
        let summary = fetch(opts()).await.unwrap();
        assert_eq!(summary.contacts_new, 2);
        assert_eq!(summary.addressbooks, 1);
        assert_eq!(summary.files_skipped, 0);

        let n = contact_count(&db).await;
        assert_eq!(n, 2);

        // Second run over the unchanged file skips it via the resume
        // cursor — no contacts re-classified as new or updated.
        let again = fetch(opts()).await.unwrap();
        assert_eq!(again.files_skipped, 1);
        assert_eq!(again.contacts_new, 0);
        assert_eq!(again.contacts_updated, 0);
        db.close().await;
    }

    #[tokio::test]
    async fn fetch_reingests_when_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let vcf = dir.path().join("Bridge.vcf");
        std::fs::write(
            &vcf,
            "BEGIN:VCARD\nVERSION:3.0\nUID:picard\nFN:Jean-Luc Picard\nEND:VCARD\n",
        )
        .unwrap();
        let db_path = dir.path().join("c.doltlite_db");
        let cache = test_cache().await;
        let db = RawDb::open(&db_path).await.unwrap();
        let opts = || options(&db, dir.path(), cache.clone());
        let first = fetch(opts()).await.unwrap();
        assert_eq!(first.contacts_new, 1);

        // Rewrite with an extra contact: size changes, so the cursor
        // misses and the whole file re-ingests. The pre-existing contact
        // reports as an update, the added one as new.
        std::fs::write(
            &vcf,
            "BEGIN:VCARD\nVERSION:3.0\nUID:picard\nFN:Jean-Luc Picard\nEND:VCARD\n\
             BEGIN:VCARD\nVERSION:3.0\nUID:riker\nFN:William Riker\nEND:VCARD\n",
        )
        .unwrap();
        let second = fetch(opts()).await.unwrap();
        assert_eq!(second.files_skipped, 0);
        assert_eq!(second.contacts_new, 1);
        assert_eq!(second.contacts_updated, 1);
        assert_eq!(second.contacts_deleted, 0);

        // Rewrite without the first contact: the file is the whole
        // address book, so a card it no longer carries is gone from the
        // mirror, not left behind as if nothing happened.
        std::fs::write(
            &vcf,
            "BEGIN:VCARD\nVERSION:3.0\nUID:riker\nFN:William Riker\nEND:VCARD\n",
        )
        .unwrap();
        let third = fetch(opts()).await.unwrap();
        assert_eq!(third.contacts_deleted, 1);
        assert_eq!(third.contacts_updated, 1);
        let book_id = addressbook_pk("local", &relative_href(dir.path(), &vcf));
        let left = db.contact_uids(&book_id).await.unwrap();
        assert_eq!(left, HashSet::from(["riker".to_string()]));
        db.close().await;
    }

    const BRIDGE: &str = "BEGIN:VCARD\nVERSION:3.0\nUID:picard\nFN:Jean-Luc Picard\nEND:VCARD\n";
    const BORG: &str = "BEGIN:VCARD\nVERSION:3.0\nUID:locutus\nFN:Locutus\nEND:VCARD\n\
         BEGIN:VCARD\nVERSION:3.0\nUID:hugh\nFN:Hugh\nEND:VCARD\n";

    async fn uids(db: &RawDb) -> Vec<String> {
        sqlx::query_scalar("SELECT uid FROM contacts ORDER BY uid")
            .fetch_all(db.pool())
            .await
            .unwrap()
    }

    async fn addressbook_hrefs(db: &RawDb) -> Vec<String> {
        sqlx::query_scalar("SELECT href FROM addressbooks ORDER BY href")
            .fetch_all(db.pool())
            .await
            .unwrap()
    }

    /// #898: deleting a whole `.vcf` file left its contacts in the store.
    #[tokio::test]
    async fn a_deleted_file_takes_its_contacts_with_it() {
        let input = tempfile::tempdir().unwrap();
        std::fs::write(input.path().join("Bridge.vcf"), BRIDGE).unwrap();
        std::fs::write(input.path().join("Borg.vcf"), BORG).unwrap();
        let store = tempfile::tempdir().unwrap();
        let db = RawDb::open(&store.path().join("c.doltlite_db"))
            .await
            .unwrap();
        let cache = test_cache().await;
        let opts = || options(&db, input.path(), cache.clone());
        fetch(opts()).await.unwrap();
        assert_eq!(uids(&db).await, vec!["hugh", "locutus", "picard"]);

        std::fs::remove_file(input.path().join("Borg.vcf")).unwrap();
        let second = fetch(opts()).await.unwrap();
        assert_eq!(second.contacts_deleted, 2);
        assert_eq!(second.files_removed, 1);
        assert_eq!(uids(&db).await, vec!["picard"]);
        assert_eq!(addressbook_hrefs(&db).await, vec!["Bridge.vcf"]);

        // The file's cursor entry went with it, so it is not removed twice.
        let third = fetch(opts()).await.unwrap();
        assert_eq!(third.files_removed, 0);
        assert_eq!(third.files_skipped, 1);
        db.close().await;
    }

    /// A moved file is an address book at its new path, and none at the old
    /// — reached through a symlink, because keying against the unresolved
    /// input once gave `Borg.vcf` and `unimatrix/Borg.vcf` one key, and the
    /// move deleted what it had just written.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_moved_file_keeps_its_contacts_under_the_new_path() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let input = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &input).unwrap();
        std::fs::write(real.join("Borg.vcf"), BORG).unwrap();
        let db = RawDb::open(&tmp.path().join("c.doltlite_db"))
            .await
            .unwrap();
        let cache = test_cache().await;
        let opts = || options(&db, &input, cache.clone());
        fetch(opts()).await.unwrap();

        std::fs::create_dir(real.join("unimatrix")).unwrap();
        std::fs::rename(real.join("Borg.vcf"), real.join("unimatrix/Borg.vcf")).unwrap();
        fetch(opts()).await.unwrap();
        assert_eq!(uids(&db).await, vec!["hugh", "locutus"]);
        assert_eq!(addressbook_hrefs(&db).await, vec!["unimatrix/Borg.vcf"]);
        db.close().await;
    }

    /// A walk that could not read an entry cannot tell a deleted file from
    /// one it failed to see, so nothing is deleted and the run says why.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_walk_error_deletes_nothing() {
        let input = tempfile::tempdir().unwrap();
        std::fs::write(input.path().join("Bridge.vcf"), BRIDGE).unwrap();
        std::fs::write(input.path().join("Borg.vcf"), BORG).unwrap();
        let store = tempfile::tempdir().unwrap();
        let db = RawDb::open(&store.path().join("c.doltlite_db"))
            .await
            .unwrap();
        let cache = test_cache().await;
        let opts = || options(&db, input.path(), cache.clone());
        fetch(opts()).await.unwrap();

        std::fs::remove_file(input.path().join("Borg.vcf")).unwrap();
        std::os::unix::fs::symlink(
            input.path().join("nowhere"),
            input.path().join("Dangling.vcf"),
        )
        .unwrap();
        let second = fetch(opts()).await.unwrap();
        assert_eq!(second.contacts_deleted, 0);
        assert_eq!(uids(&db).await, vec!["hugh", "locutus", "picard"]);
        let problems: Vec<String> = sqlx::query_scalar("SELECT scope_key FROM problems")
            .fetch_all(db.pool())
            .await
            .unwrap();
        assert_eq!(problems, vec!["listing:files"]);

        // Once the walk completes, the deletion happens and the problem clears.
        std::fs::remove_file(input.path().join("Dangling.vcf")).unwrap();
        let third = fetch(opts()).await.unwrap();
        assert_eq!(third.contacts_deleted, 2);
        assert_eq!(uids(&db).await, vec!["picard"]);
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM problems")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(left, 0);
        db.close().await;
    }

    // Google's vCard export carries no `UID:` — identity rides the
    // first+last name instead. An edit to a *non-name* field must keep
    // the same row (the ship-of-Theseus property the synthesized id
    // buys us), not fork into a new contact.
    #[tokio::test]
    async fn synthesized_name_id_survives_field_edit_for_uidless_cards() {
        let dir = tempfile::tempdir().unwrap();
        let vcf = dir.path().join("Google.vcf");
        std::fs::write(
            &vcf,
            "BEGIN:VCARD\nVERSION:3.0\nFN:Ada Lovelace\nN:Lovelace;Ada;;;\nEMAIL:ada@x.org\nEND:VCARD\n",
        )
        .unwrap();
        let db_path = dir.path().join("c.doltlite_db");
        let cache = test_cache().await;
        let db = RawDb::open(&db_path).await.unwrap();
        let opts = || options(&db, dir.path(), cache.clone());
        let first = fetch(opts()).await.unwrap();
        assert_eq!(first.contacts_new, 1);

        // Edit the email (and grow the file so the resume cursor misses
        // and re-ingests). Name is unchanged → same synthesized id →
        // update, not a second row.
        std::fs::write(
            &vcf,
            "BEGIN:VCARD\nVERSION:3.0\nFN:Ada Lovelace\nN:Lovelace;Ada;;;\nEMAIL:ada.lovelace@analytical.org\nEND:VCARD\n",
        )
        .unwrap();
        let second = fetch(opts()).await.unwrap();
        assert_eq!(second.files_skipped, 0);
        assert_eq!(second.contacts_new, 0);
        assert_eq!(second.contacts_updated, 1);

        let n = contact_count(&db).await;
        assert_eq!(n, 1, "edited contact stayed one row, not two");
        db.close().await;
    }

    /// Two UID-less cards sharing a first+last name used to collapse onto
    /// one synthesized id, and one of the two people was lost.
    #[tokio::test]
    async fn same_name_uidless_cards_stay_two_rows() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Google.vcf"),
            "BEGIN:VCARD\nVERSION:3.0\nFN:John Smith\nN:Smith;John;;;\nEMAIL:john1@x.org\nEND:VCARD\n\
             BEGIN:VCARD\nVERSION:3.0\nFN:John Smith\nN:Smith;John;;;\nEMAIL:john2@x.org\nEND:VCARD\n",
        )
        .unwrap();
        let db_path = dir.path().join("c.doltlite_db");
        let db = RawDb::open(&db_path).await.unwrap();
        let summary = fetch(options(&db, dir.path(), test_cache().await))
            .await
            .unwrap();
        assert_eq!(summary.addressbooks, 1);

        let n = contact_count(&db).await;
        assert_eq!(n, 2, "same-name cards are two people");
        db.close().await;
    }

    /// A file that will not read is a row, and is read again next run.
    #[tokio::test]
    async fn a_file_that_will_not_read_is_a_row() {
        let input = tempfile::tempdir().unwrap();
        let path = input.path().join("Borg.vcf");
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        let store = tempfile::tempdir().unwrap();
        let db = RawDb::open(&store.path().join("c.doltlite_db"))
            .await
            .unwrap();
        let cache = test_cache().await;
        let opts = || options(&db, input.path(), cache.clone());
        fetch(opts()).await.unwrap();
        let problems = || async {
            sqlx::query_scalar::<_, String>("SELECT scope_key FROM problems")
                .fetch_all(db.pool())
                .await
                .unwrap()
        };
        assert_eq!(problems().await, vec!["listing:vcf Borg.vcf"]);

        std::fs::write(&path, BORG).unwrap();
        fetch(opts()).await.unwrap();
        assert!(problems().await.is_empty());
        assert_eq!(uids(&db).await, vec!["hugh", "locutus"]);
        db.close().await;
    }

    /// A `.vcf` rewritten to nothing (0 bytes, text that holds no vCard, a
    /// copy cut off inside a card) read as an address book with fewer
    /// cards, and every card it lost was deleted. vCard has no envelope
    /// that could say "no cards", so a file holding none never empties its
    /// book; deleting the file does.
    #[tokio::test]
    async fn a_file_that_is_recognizably_nothing_deletes_nothing() {
        let input = tempfile::tempdir().unwrap();
        let path = input.path().join("Borg.vcf");
        std::fs::write(&path, BORG).unwrap();
        let store = tempfile::tempdir().unwrap();
        let db = RawDb::open(&store.path().join("c.doltlite_db"))
            .await
            .unwrap();
        let cache = test_cache().await;
        let opts = || options(&db, input.path(), cache.clone());
        fetch(opts()).await.unwrap();
        assert_eq!(uids(&db).await, vec!["hugh", "locutus"]);

        let cut_off = &BORG[..BORG.rfind("END:VCARD").unwrap()];
        for (what, body) in [
            ("an empty file", ""),
            ("text that holds no vCard", "We are the Borg.\n"),
            ("a copy cut off inside a card", cut_off),
        ] {
            std::fs::write(&path, body).unwrap();
            let s = fetch(opts()).await.unwrap();
            assert_eq!(s.contacts_deleted, 0, "{what} deleted contacts");
            assert_eq!(uids(&db).await, vec!["hugh", "locutus"], "{what}");
            let problems: Vec<String> = sqlx::query_scalar("SELECT scope_key FROM problems")
                .fetch_all(db.pool())
                .await
                .unwrap();
            assert_eq!(problems, vec!["listing:vcf Borg.vcf"], "{what}");
        }
        db.close().await;
    }

    // A card with neither UID nor name keeps file-position identity so
    // distinct nameless cards don't collapse into a single row.
    #[tokio::test]
    async fn nameless_uidless_cards_stay_distinct() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Google.vcf"),
            "BEGIN:VCARD\nVERSION:3.0\nEMAIL:a@x.org\nCATEGORIES:myContacts\nEND:VCARD\n\
             BEGIN:VCARD\nVERSION:3.0\nEMAIL:b@x.org\nCATEGORIES:myContacts\nEND:VCARD\n",
        )
        .unwrap();
        let db_path = dir.path().join("c.doltlite_db");
        let db = RawDb::open(&db_path).await.unwrap();
        let summary = fetch(options(&db, dir.path(), test_cache().await))
            .await
            .unwrap();
        assert_eq!(summary.contacts_new, 2);

        let n = contact_count(&db).await;
        assert_eq!(n, 2, "two nameless cards stayed distinct rows");
        db.close().await;
    }
}
