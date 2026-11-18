//! Every chunk the search cuts fits the model that embeds it: its whole
//! prompt, with the title, facts line or envelope in front, within the
//! model's context, however few bytes the text packs into a token. Cut and
//! counted by the model's own tokenizer.

use std::path::PathBuf;

use datalib_embedder::{Tokenizer, CONTEXT_TOKENS};
use datalib_search_index::{IndexDoc, IndexSection};

fn model() -> PathBuf {
    let rel = std::env::var("EMBED_MODEL_RLOC").expect("EMBED_MODEL_RLOC unset");
    let p = runfiles::Runfiles::create()
        .expect("runfiles")
        .rlocation(&rel)
        .unwrap_or_else(|| panic!("{rel} not in runfiles"));
    assert!(p.exists(), "{rel} resolved to a missing path {p:?}");
    p
}

/// A string of `n` characters from `alphabet`, the same every run.
fn noise(alphabet: &str, n: usize) -> String {
    let chars: Vec<char> = alphabet.chars().collect();
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            chars[(x % chars.len() as u64) as usize]
        })
        .collect()
}

/// Texts that cost far more tokens a byte than prose: what a calendar
/// invite, a log or an attachment's encoding puts in a document.
fn dense_texts() -> Vec<(&'static str, String)> {
    let b64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    vec![
        (
            "meeting links",
            (0..100)
                .map(|i| format!("https://meet.example.test/j/{i:08}?pwd={} ", noise(b64, 22)))
                .collect(),
        ),
        (
            "dial-in codes",
            (0..250)
                .map(|i| format!("+1 650-555-{:04},,{:07}# ", i % 10_000, i * 7_919))
                .collect(),
        ),
        ("base64", noise(b64, 5_000)),
        ("hex", noise("0123456789abcdef", 5_000)),
        (
            "japanese",
            "会議の議事録：転送装置の再調整について。".repeat(100),
        ),
        ("emoji", "🚀✅🙂🔧🛰️".repeat(400)),
    ]
}

/// A long dense text cut by the byte budget the chunker used to have ran
/// past the model's context, so the model read only its start. Each kind
/// of text is tried as a page, a message with a long envelope, and a page
/// with a facts line, which every chunk repeats.
#[test]
fn every_chunk_of_dense_text_fits_the_model() {
    let tokenizer = Tokenizer::open(&model()).unwrap();
    let ends = |text: &str| tokenizer.ends(text);
    let envelope = format!(
        "Data — to {} — Tue 3 Mar 2026 14:05",
        (0..40)
            .map(|i| format!("crew{i}@enterprise.test"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let facts = "Source: calendar\nKind: Recurring Event\nCalendar: Engineering";
    for (kind, text) in dense_texts() {
        let shapes = [
            (None, None, text.clone()),
            (
                None,
                Some(envelope.clone()),
                format!("{envelope}\n\n{text}"),
            ),
            (Some(facts), None, format!("{facts}\n\n{text}")),
        ];
        for (head, lead, body) in shapes {
            let doc = IndexDoc {
                source_id: "engineering".into(),
                markdown_uuid: "d".into(),
                content_hash: "h".into(),
                title: format!("Warp core diagnostics ({kind})"),
                head: head.map(str::to_string),
                sections: vec![IndexSection {
                    uuid: lead.as_ref().map(|_| "m1".to_string()),
                    text: body,
                    lead: lead.clone(),
                }],
            };
            let chunks = doc.prompted(&ends).unwrap();
            assert!(chunks.len() > 1, "{kind}: {} chunks", chunks.len());
            for (i, c) in chunks.iter().enumerate() {
                let tokens = tokenizer.ends(&c.prompt).unwrap().len() + 2;
                assert!(
                    tokens <= CONTEXT_TOKENS as usize,
                    "{kind} chunk {i}: {tokens} tokens, past the model's {CONTEXT_TOKENS}"
                );
            }
        }
    }
}
