//! The reader against an index and cache the writers filled.

use std::collections::HashMap;
use std::sync::Arc;

use datalib_search_index::{
    chunk::{chunk_hash, four_bytes_a_token},
    ChunkedDoc, Index, IndexDoc, IndexSection, InferenceCache, KeptRows, Reader,
};

fn doc(source: &str, uuid: &str, sections: &[(Option<&str>, &str)]) -> IndexDoc {
    IndexDoc {
        source_id: source.into(),
        markdown_uuid: uuid.into(),
        content_hash: format!("hash-{uuid}"),
        title: "Ten Forward".into(),
        head: None,
        sections: sections
            .iter()
            .map(|(uuid, text)| IndexSection {
                uuid: uuid.map(str::to_string),
                text: text.to_string(),
                lead: None,
            })
            .collect(),
    }
}

fn thread() -> IndexDoc {
    doc(
        "mail",
        "thread",
        &[
            (None, "# Ten Forward\n"),
            (Some("m1"), "the warp core is stable\n"),
            (Some("m2"), "deck twelve reports a coolant leak\n"),
        ],
    )
}

struct Fixture {
    dir: tempfile::TempDir,
    reader: Reader,
}

impl Fixture {
    fn index_path(&self) -> std::path::PathBuf {
        self.dir.path().join("index.sqlite")
    }

    fn cache_path(&self) -> std::path::PathBuf {
        self.dir.path().join("cache.sqlite")
    }

    /// Indexes `docs`, cuts them as their `embed` would, and caches
    /// `vectors`, as a sync does after the reader has read.
    async fn sync(&self, docs: &[IndexDoc], vectors: &[(String, Vec<f32>)]) {
        let index = Index::open(&self.index_path()).await.unwrap();
        index.put(docs).await.unwrap();
        index.close().await;
        let cache = InferenceCache::open(&self.cache_path()).await.unwrap();
        cache.file_chunks(&cut(docs)).await.unwrap();
        cache
            .put(&datalib_search_index::model_key(), vectors)
            .await
            .unwrap();
        cache.close().await;
    }
}

fn cut(docs: &[IndexDoc]) -> Vec<ChunkedDoc> {
    docs.iter()
        .map(|d| ChunkedDoc::from_prompted(d, &d.prompted(&four_bytes_a_token).unwrap()))
        .collect()
}

/// The hash of `doc`'s one chunk.
fn only_chunk(doc: &IndexDoc) -> String {
    let prompted = doc.prompted(&four_bytes_a_token).unwrap();
    assert_eq!(prompted.len(), 1, "{} is one chunk", doc.markdown_uuid);
    prompted[0].hash.clone()
}

/// `docs` in the keyword index, each cut into chunks as its `embed` cuts
/// it, and `vectors` cached.
async fn fixture(docs: &[IndexDoc], vectors: &[(String, Vec<f32>)]) -> Fixture {
    fixture_cut(docs, docs, vectors).await
}

/// [`fixture`] with only `cut` cut into chunks: a document its `embed`
/// has not reached, or one cut from another rendering.
async fn fixture_cut(
    docs: &[IndexDoc],
    cut: &[IndexDoc],
    vectors: &[(String, Vec<f32>)],
) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let index_path = dir.path().join("index.sqlite");
    let cache_path = dir.path().join("cache.sqlite");
    let index = Index::open(&index_path).await.unwrap();
    index.put(docs).await.unwrap();
    index.close().await;
    let cache = InferenceCache::open(&cache_path).await.unwrap();
    cache.file_chunks(&self::cut(cut)).await.unwrap();
    cache
        .put(&datalib_search_index::model_key(), vectors)
        .await
        .unwrap();
    cache.close().await;
    let reader = Reader::open(&index_path, &cache_path)
        .await
        .unwrap()
        .unwrap();
    Fixture { dir, reader }
}

/// `v` as search holds it: in half precision.
fn half(v: &[f32]) -> Vec<u16> {
    v.iter()
        .map(|x| datalib_search_index::half::from_f32(*x))
        .collect()
}

/// Every word, each whole, in one message.
fn all_of(words: &[&str]) -> String {
    let terms: Vec<String> = words.iter().map(|w| format!("\"{w}\"")).collect();
    format!("{{title body}} : ({})", terms.join(" AND "))
}

fn ids(hits: &[datalib_search_index::MessageHit]) -> Vec<&str> {
    hits.iter().map(|h| h.markdown_uuid.as_str()).collect()
}

/// A hit is a message holding every word, and names that message: a
/// thread that says the words in different messages is not one.
#[tokio::test]
async fn a_hit_is_a_message_holding_every_word() {
    let t = thread();
    let both = doc(
        "mail",
        "both",
        &[
            (Some("b0"), "tea\n"),
            (Some("b1"), "the warp core coolant\n"),
        ],
    );
    let f = fixture(&[t, both], &[]).await;
    let spread = f
        .reader
        .words(&all_of(&["warp", "coolant"]), None, None, 10)
        .await
        .unwrap();
    assert_eq!(ids(&spread), ["both"]);
    let one = f
        .reader
        .words(&all_of(&["coolant"]), None, None, 10)
        .await
        .unwrap();
    let thread_hit = one.iter().find(|h| h.markdown_uuid == "thread").unwrap();
    assert_eq!(
        thread_hit.uuid.as_deref(),
        Some("m2"),
        "the message saying it"
    );
    f.reader.close().await;
}

/// A document answers once, at its best message, and the cut counts
/// documents; `among` and an excluded word narrow what is ranked.
#[tokio::test]
async fn a_document_answers_once_at_its_best_message() {
    let many = doc(
        "mail",
        "many",
        &[
            (Some("m1"), "warp\n"),
            (Some("m2"), "warp warp warp\n"),
            (Some("m3"), "warp and a breach\n"),
        ],
    );
    let other = doc("mail", "other", &[(Some("o1"), "warp drive\n")]);
    let f = fixture(&[many.clone(), other], &[]).await;
    let hits = f
        .reader
        .words(&all_of(&["warp"]), None, None, 10)
        .await
        .unwrap();
    let mut found = ids(&hits);
    found.sort();
    assert_eq!(found, ["many", "other"]);
    let best = hits.iter().find(|h| h.markdown_uuid == "many").unwrap();
    assert_eq!(best.uuid.as_deref(), Some("m2"), "warp warp warp");
    assert_eq!(
        f.reader
            .words(&all_of(&["warp"]), None, None, 1)
            .await
            .unwrap()
            .len(),
        1
    );
    let kept = only_document("other");
    let narrowed = f
        .reader
        .words(&all_of(&["warp"]), None, Some(&kept), 10)
        .await
        .unwrap();
    assert_eq!(ids(&narrowed), ["other"]);
    let not_breach = f
        .reader
        .words("{title body} : (\"breach\" NOT \"warp\")", None, None, 10)
        .await
        .unwrap();
    assert!(not_breach.is_empty(), "the breach is said with warp");
    f.reader.close().await;
}

#[tokio::test]
async fn a_source_left_out_is_not_matched() {
    let other = doc("slack", "chat", &[(Some("c1"), "warp drive online\n")]);
    let f = fixture(&[thread(), other], &[]).await;
    let only_slack = ["slack".to_string()];
    let found = f
        .reader
        .words(&all_of(&["warp"]), Some(&only_slack), None, 10)
        .await
        .unwrap();
    assert_eq!(ids(&found), ["chat"]);
    f.reader.close().await;
}

/// A chunk with a cached vector is held, unit length, with the messages it
/// covers; one the cache lacks is not, and neither is a document left with
/// no chunk.
#[tokio::test]
async fn held_vectors_are_the_cached_chunks_with_their_messages() {
    let t = thread();
    let text: String = t.sections.iter().map(|s| s.text.as_str()).collect();
    let hash = chunk_hash(&t.title, &text);
    let unembedded = doc("mail", "draft", &[(Some("d1"), "not yet embedded\n")]);
    let f = fixture(&[t, unembedded], &[(hash, vec![3.0, 4.0])]).await;
    let held = f
        .reader
        .held(&datalib_search_index::model_key())
        .await
        .unwrap();
    assert_eq!(held.dim, 2);
    let embedded: Vec<_> = held.embedded().collect();
    assert_eq!(embedded.len(), 1);
    let d = embedded[0];
    assert_eq!(d.markdown_uuid, "thread");
    assert_eq!(d.vector(0), half(&[0.6, 0.8]));
    let covered: Vec<&str> = d.covered(0).collect();
    assert_eq!(covered, ["m1", "m2"]);
    let other_model = f.reader.held("another-model").await.unwrap();
    assert!(other_model.docs.is_empty());
    f.reader.close().await;
}

/// Reading again after a sync reads only the documents that changed: one
/// whose rendering has not moved is the one held, though its vector in the
/// cache now says otherwise, where a full read takes the cache's.
#[tokio::test]
async fn a_reload_reads_only_the_documents_that_changed() {
    let t = thread();
    let chat = doc("chat", "chat", &[(Some("c1"), "tea, Earl Grey\n")]);
    let (thread_hash, chat_hash) = (only_chunk(&t), only_chunk(&chat));
    let model = datalib_search_index::model_key();
    let f = fixture(
        std::slice::from_ref(&t),
        &[(thread_hash.clone(), vec![3.0, 4.0])],
    )
    .await;
    let before = f.reader.held(&model).await.unwrap();
    f.sync(
        &[chat],
        &[(thread_hash, vec![4.0, 3.0]), (chat_hash, vec![0.0, 2.0])],
    )
    .await;
    let after = f.reader.held_after(&model, Some(&before)).await.unwrap();
    assert!(Arc::ptr_eq(&before.docs["thread"], &after.docs["thread"]));
    assert_eq!(
        after.docs["thread"].vector(0),
        half(&[0.6, 0.8]),
        "not read"
    );
    assert_eq!(
        after.docs["chat"].vector(0),
        half(&[0.0, 1.0]),
        "read: it is new"
    );
    let full = f.reader.held(&model).await.unwrap();
    assert_eq!(
        full.docs["thread"].vector(0),
        half(&[0.8, 0.6]),
        "a full read reads it"
    );
    f.reader.close().await;
}

/// A conversation that grows by a message is drawn again, and only its new
/// chunk's vector is read: the first chunk's is the one held before, though
/// the cache now says otherwise.
#[tokio::test]
async fn a_grown_conversation_reads_only_its_new_chunk() {
    let model = datalib_search_index::model_key();
    let mut chat = doc("chat", "chat", &[]);
    chat.sections = vec![message("m1", "warp core")];
    let mut grown = chat.clone();
    grown.content_hash = "a later rendering".into();
    grown.sections.push(message("m2", "coolant leak"));
    let first = chat.prompted(&four_bytes_a_token).unwrap()[0].hash.clone();
    let both = grown.prompted(&four_bytes_a_token).unwrap();
    assert_eq!(both.len(), 2, "a message starts a chunk of its own");
    assert_eq!(both[0].hash, first, "the first chunk did not move");
    let f = fixture(
        std::slice::from_ref(&chat),
        &[(first.clone(), vec![3.0, 4.0])],
    )
    .await;
    let before = f.reader.held(&model).await.unwrap();
    f.sync(
        &[grown],
        &[
            (first, vec![4.0, 3.0]),
            (both[1].hash.clone(), vec![0.0, 2.0]),
        ],
    )
    .await;
    let after = f.reader.held_after(&model, Some(&before)).await.unwrap();
    let d = &after.docs["chat"];
    assert_eq!(d.chunks.len(), 2);
    assert_eq!(
        d.vector(0),
        half(&[0.6, 0.8]),
        "kept from the earlier version"
    );
    assert_eq!(d.vector(1), half(&[0.0, 1.0]), "read: it is new");
    f.reader.close().await;
}

/// What a reload holds is what reading everything again holds: a document
/// gone, one drawn again into a new chunk, one added, one not embedded
/// yet, and one whose chunk was waiting for a vector that has now come.
#[tokio::test]
async fn a_reload_holds_what_a_full_read_holds() {
    let model = datalib_search_index::model_key();
    let thread = thread();
    let gone = doc("mail", "gone", &[(Some("g1"), "a shuttle departs\n")]);
    let chat = doc("chat", "chat", &[(Some("c1"), "tea, Earl Grey\n")]);
    let waiting = doc("chat", "waiting", &[(Some("w1"), "red alert\n")]);
    let first = [&thread, &gone, &chat].map(|d| (only_chunk(d), vec![1.0, 2.0]));
    let f = fixture(
        &[thread.clone(), gone, chat.clone(), waiting.clone()],
        &first,
    )
    .await;
    let before = f.reader.held(&model).await.unwrap();
    let embedded = |held: &datalib_search_index::Held| -> Vec<String> {
        held.embedded().map(|d| d.markdown_uuid.clone()).collect()
    };
    assert_eq!(embedded(&before), ["chat", "gone", "thread"]);

    let index = Index::open(&f.index_path()).await.unwrap();
    index.remove(&["gone".to_string()]).await.unwrap();
    index.close().await;
    let cache = InferenceCache::open(&f.cache_path()).await.unwrap();
    cache.forget_chunked(&["gone".to_string()]).await.unwrap();
    assert_eq!(cache.sweep(&model).await.unwrap(), 1);
    cache.close().await;
    let mut redrawn = chat.clone();
    redrawn.content_hash = "a later rendering".into();
    redrawn.sections[0].text = "tea, Earl Grey, hot\n".into();
    let fresh = doc("mail", "fresh", &[(Some("f1"), "engage\n")]);
    let owed = doc("mail", "owed", &[(Some("o1"), "not yet embedded\n")]);
    f.sync(
        &[redrawn.clone(), fresh.clone(), owed],
        &[
            (only_chunk(&redrawn), vec![0.0, 3.0]),
            (only_chunk(&fresh), vec![5.0, 0.0]),
            (only_chunk(&waiting), vec![0.0, 1.0]),
        ],
    )
    .await;

    let after = f.reader.held_after(&model, Some(&before)).await.unwrap();
    let full = f.reader.held(&model).await.unwrap();
    assert_eq!(embedded(&full), ["chat", "fresh", "thread", "waiting"]);
    assert_eq!(after, full);
    assert!(Arc::ptr_eq(&before.docs["thread"], &after.docs["thread"]));
    f.reader.close().await;
}

/// A full read takes the cache's vectors a page at a time: one past the
/// first page is held too.
#[tokio::test]
async fn a_full_read_reads_past_the_first_page() {
    let n = usize::try_from(datalib_search_index::read::SCAN_PAGE).unwrap() + 5;
    let docs: Vec<IndexDoc> = (0..n)
        .map(|i| {
            doc(
                "mail",
                &format!("d{i}"),
                &[(Some("m"), &format!("note {i}\n"))],
            )
        })
        .collect();
    let vectors: Vec<(String, Vec<f32>)> = docs
        .iter()
        .map(|d| (only_chunk(d), vec![1.0, 0.0]))
        .collect();
    let f = fixture(&docs, &vectors).await;
    let held = f
        .reader
        .held(&datalib_search_index::model_key())
        .await
        .unwrap();
    assert_eq!(held.chunks(), n);
    f.reader.close().await;
}

/// The full read walks the primary key in order, and nothing else: no
/// sort, and no detour through the keys' index to the rows.
#[tokio::test]
async fn a_full_read_walks_the_vectors_in_key_order() {
    let f = fixture(&[thread()], &[]).await;
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(f.cache_path()))
        .await
        .unwrap();
    let explain = format!(
        "EXPLAIN QUERY PLAN {}",
        datalib_search_index::read::SCAN_SQL
    );
    // A constant statement of the crate under test.
    let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(explain))
        .bind("m")
        .bind("")
        .bind(1)
        .fetch_all(&pool)
        .await
        .unwrap();
    pool.close().await;
    let plan: Vec<String> = plan.into_iter().map(|(.., d)| d).collect();
    assert_eq!(
        plan,
        ["SEARCH vectors USING PRIMARY KEY (model=? AND hash>?)"],
        "{plan:?}"
    );
    f.reader.close().await;
}

/// A reload that builds a few documents reads their vectors by key, and
/// holds what a full read, one pass over the table, holds.
#[tokio::test]
async fn a_few_documents_read_by_key_hold_what_a_full_read_holds() {
    let model = datalib_search_index::model_key();
    let docs: Vec<IndexDoc> = (0..6)
        .map(|i| {
            doc(
                "mail",
                &format!("d{i}"),
                &[(Some("m"), &format!("note {i}\n"))],
            )
        })
        .collect();
    let vectors: Vec<(String, Vec<f32>)> = docs
        .iter()
        .enumerate()
        .map(|(i, d)| (only_chunk(d), vec![i as f32, 1.0]))
        .collect();
    let f = fixture(&docs, &vectors).await;
    let before = f.reader.held(&model).await.unwrap();
    let mut redrawn = docs[2].clone();
    redrawn.content_hash = "a later rendering".into();
    redrawn.sections[0].text = "a later note\n".into();
    f.sync(
        std::slice::from_ref(&redrawn),
        &[(only_chunk(&redrawn), vec![3.0, -4.0])],
    )
    .await;
    let after = f.reader.held_after(&model, Some(&before)).await.unwrap();
    let full = f.reader.held(&model).await.unwrap();
    assert_eq!(after.docs["d2"].vector(0), half(&[0.6, -0.8]));
    assert_eq!(after, full);
    f.reader.close().await;
}

/// A document is embedded when every chunk of it has a vector; one the
/// index does not hold is not answered, and the whole index is counted.
#[tokio::test]
async fn a_document_is_embedded_when_every_chunk_has_a_vector() {
    let t = thread();
    let text: String = t.sections.iter().map(|s| s.text.as_str()).collect();
    let hash = chunk_hash(&t.title, &text);
    let unembedded = doc("mail", "draft", &[(Some("d1"), "not yet embedded\n")]);
    let f = fixture(&[t, unembedded], &[(hash, vec![3.0, 4.0])]).await;
    let model = datalib_search_index::model_key();
    let asked = ["thread", "draft", "never-indexed"].map(String::from);
    let states = f.reader.states(&model, &asked).await.unwrap();
    assert_eq!(states.len(), 2, "{states:?}");
    assert!(states["thread"]);
    assert!(!states["draft"]);
    assert_eq!(f.reader.coverage(&model).await.unwrap(), (2, 1));
    assert_eq!(f.reader.coverage("another-model").await.unwrap(), (2, 0));
    f.reader.close().await;
}

/// A document counts as embedded only when its chunks were cut from the
/// rendering the keyword index holds: one its `embed` has not reached,
/// or reached at an older rendering, is neither held nor embedded,
/// though the old chunk's vector is cached.
#[tokio::test]
async fn only_chunks_cut_from_the_indexed_rendering_count() {
    let t = thread();
    let text: String = t.sections.iter().map(|s| s.text.as_str()).collect();
    let vector = (chunk_hash(&t.title, &text), vec![3.0, 4.0]);
    let mut older = t.clone();
    older.content_hash = "an older rendering".into();
    let model = datalib_search_index::model_key();
    let asked = ["thread".to_string()];
    for cut in [vec![], vec![older]] {
        let f = fixture_cut(
            std::slice::from_ref(&t),
            &cut,
            std::slice::from_ref(&vector),
        )
        .await;
        assert!(f.reader.held(&model).await.unwrap().docs.is_empty());
        assert_eq!(
            f.reader.states(&model, &asked).await.unwrap(),
            HashMap::from([("thread".to_string(), false)])
        );
        assert_eq!(f.reader.coverage(&model).await.unwrap(), (1, 0));
        f.reader.close().await;
    }
    let f = fixture(&[t], std::slice::from_ref(&vector)).await;
    assert_eq!(f.reader.coverage(&model).await.unwrap(), (1, 1));
    f.reader.close().await;
}

/// A message of `text`, drawn with an envelope: it starts a chunk of its
/// own.
fn message(uuid: &str, text: &str) -> IndexSection {
    IndexSection {
        uuid: Some(uuid.into()),
        text: format!("{text}\n"),
        lead: Some(format!("**{uuid}** wrote")),
    }
}

/// A document is embedded only once every chunk of it has a vector, not
/// when some do; and every source's documents are answered and counted.
#[tokio::test]
async fn a_document_is_embedded_only_once_every_chunk_is() {
    let mut two = doc("mail", "two", &[]);
    two.sections = vec![message("m1", "warp core"), message("m2", "coolant leak")];
    let halves = two.prompted(&four_bytes_a_token).unwrap();
    assert_eq!(halves.len(), 2, "a message starts a chunk of its own");
    let chat = doc("chat", "chat", &[(Some("c1"), "tea, Earl Grey\n")]);
    let chat_chunk = chat.prompted(&four_bytes_a_token).unwrap()[0].hash.clone();
    let model = datalib_search_index::model_key();
    let asked = ["two", "chat"].map(String::from);

    let first = (halves[0].hash.clone(), vec![1.0, 0.0]);
    let tea = (chat_chunk, vec![0.0, 1.0]);
    let f = fixture(&[two.clone(), chat.clone()], &[first.clone(), tea.clone()]).await;
    let states = f.reader.states(&model, &asked).await.unwrap();
    assert_eq!(
        states,
        HashMap::from([("two".to_string(), false), ("chat".to_string(), true)])
    );
    assert_eq!(f.reader.coverage(&model).await.unwrap(), (2, 1));
    f.reader.close().await;

    let second = (halves[1].hash.clone(), vec![0.6, 0.8]);
    let f = fixture(&[two, chat], &[first, second, tea]).await;
    let states = f.reader.states(&model, &asked).await.unwrap();
    assert!(states.values().all(|embedded| *embedded), "{states:?}");
    assert_eq!(f.reader.coverage(&model).await.unwrap(), (2, 2));
    f.reader.close().await;
}

/// A page's rows can ask about more documents than one statement binds:
/// those past the first batch are answered too.
#[tokio::test]
async fn states_are_answered_past_the_first_batch() {
    let held = ["held-a", "held-b"];
    let docs: Vec<IndexDoc> = held
        .iter()
        .map(|u| doc("mail", u, &[(Some("m"), "warp\n")]))
        .collect();
    let f = fixture(&docs, &[]).await;
    let mut asked: Vec<String> = (0..10_005).map(|i| format!("absent-{i}")).collect();
    asked.extend(held.map(String::from));
    let states = f
        .reader
        .states(&datalib_search_index::model_key(), &asked)
        .await
        .unwrap();
    assert_eq!(
        states,
        HashMap::from([("held-a".to_string(), false), ("held-b".to_string(), false)])
    );
    f.reader.close().await;
}

#[tokio::test]
async fn no_index_reads_as_none() {
    let dir = tempfile::tempdir().unwrap();
    let reader = Reader::open(&dir.path().join("a"), &dir.path().join("b"))
        .await
        .unwrap();
    assert!(reader.is_none());
}

/// The grid index's file holds the keyword index's tables before any
/// keyword pass has run: the reader takes it as no index yet, not as an
/// error, until a pass starts the index's generation.
#[tokio::test]
async fn a_grid_index_no_keyword_pass_has_run_on_is_no_index_yet() {
    let dir = tempfile::tempdir().unwrap();
    let grid = dir.path().join("db.sqlite");
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&grid)
        .create_if_missing(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .unwrap();
    for ddl in datalib_search_index::KEYWORD_DDL {
        sqlx::query(ddl).execute(&pool).await.unwrap();
    }
    pool.close().await;
    let cache = dir.path().join("cache.sqlite");
    assert!(Reader::open(&grid, &cache).await.unwrap().is_none());
    datalib_search_index::Index::open(&grid)
        .await
        .unwrap()
        .close()
        .await;
    assert!(Reader::open(&grid, &cache).await.unwrap().is_some());
}

fn page_term(word: &str) -> String {
    format!("{{title body}} : (\"{word}\")")
}

fn page_terms(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| page_term(w)).collect()
}

/// A page's heading and its rows: no one piece says every word.
fn page(uuid: &str) -> IndexDoc {
    doc(
        "airvisual",
        uuid,
        &[
            (None, "# Air quality\n"),
            (Some("kitchen"), "Kitchen: 12 µg/m³\n"),
            (Some("bedroom"), "Bedroom: 8 µg/m³ air\n"),
        ],
    )
}

/// The same words said as messages, each with its envelope.
fn conversation() -> IndexDoc {
    let mut c = page("chat");
    c.source_id = "slack".into();
    for s in &mut c.sections {
        s.lead = Some("Data — Tue 3 Mar 2026 14:05".into());
    }
    c
}

/// A page holds the words between its pieces, and lands on the piece
/// holding the most of them; a
/// conversation that says them in different messages is not found.
#[tokio::test]
async fn a_page_holds_the_words_between_its_pieces() {
    let f = fixture(&[page("page"), conversation()], &[]).await;
    let hits = f
        .reader
        .page_words(&page_terms(&["bedroom", "air", "quality"]), None, None, 10)
        .await
        .unwrap();
    assert_eq!(ids(&hits), ["page"]);
    assert_eq!(hits[0].uuid.as_deref(), Some("bedroom"));
    let missing = f
        .reader
        .page_words(&page_terms(&["bedroom", "tribble"]), None, None, 10)
        .await
        .unwrap();
    assert!(missing.is_empty(), "a page lacking a word: {missing:?}");
    f.reader.close().await;
}

/// The rows a search keeps, and `sources`, narrow the pages read.
#[tokio::test]
async fn the_kept_rows_and_the_sources_narrow_the_pages() {
    let f = fixture(&[page("page"), page("other")], &[]).await;
    let terms = page_terms(&["bedroom", "quality"]);
    let kept = only_document("other");
    let narrowed = f
        .reader
        .page_words(&terms, None, Some(&kept), 10)
        .await
        .unwrap();
    assert_eq!(ids(&narrowed), ["other"]);
    let elsewhere = f
        .reader
        .page_words(&terms, Some(&["slack".to_string()]), None, 10)
        .await
        .unwrap();
    assert!(elsewhere.is_empty(), "{elsewhere:?}");
    f.reader.close().await;
}

/// A page's heading is no row's, so it is the page's own row: a filter
/// that keeps that row and one of the page's rows finds the page by its
/// heading's words, and one that keeps only the other row does not.
/// Before, any filter that kept one of a page's rows lost the words of
/// every piece that was no row's, such as a Drive file's whole text.
#[tokio::test]
async fn a_piece_that_is_no_rows_is_its_documents_row() {
    let f = fixture(&[page("page")], &[]).await;
    let keep = |rows: Vec<&'static str>| KeptRows {
        whole: Vec::new(),
        messages: rows,
    };
    for (kept, found) in [
        (keep(vec!["bedroom", "page"]), vec!["page"]),
        (keep(vec!["bedroom"]), vec![]),
    ] {
        let heading = f
            .reader
            .words(&all_of(&["quality"]), None, Some(&kept), 10)
            .await
            .unwrap();
        assert_eq!(ids(&heading), found, "words, keeping {:?}", kept.messages);
        let across = f
            .reader
            .page_words(&page_terms(&["bedroom", "quality"]), None, Some(&kept), 10)
            .await
            .unwrap();
        assert_eq!(
            ids(&across),
            found,
            "page words, keeping {:?}",
            kept.messages
        );
    }
    f.reader.close().await;
}

/// What a search keeps when its filters keep `document` whole and
/// nothing else.
fn only_document(document: &str) -> KeptRows<'_> {
    KeptRows {
        whole: vec![document],
        messages: Vec::new(),
    }
}
