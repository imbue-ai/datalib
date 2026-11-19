//! `datalib-step embed-prompts` end to end, the real binary on the TNG
//! fixture's prompts: each comes back in order under its hash, and its
//! vector is the one the fixture's embed put in the inference cache for
//! that hash. The cache may have been made on another device (Metal on a
//! mac, the CPU on CI), so the two are held to the devices' agreement,
//! not to the bit.
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::Value;
use sqlx::{Connection, Row};

/// Enough prompts to fill more than one of the command's groups.
const PROMPTS: usize = 80;

fn runfile(var: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(var).unwrap_or_else(|| panic!("{var} is set")))
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum();
    let norm = |v: &[f32]| v.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    dot / (norm(a) * norm(b))
}

/// The fixture's first `PROMPTS` prompts, as `(hash, prompt)`.
fn fixture_prompts() -> Vec<(String, String)> {
    std::fs::read_to_string(runfile("SEARCH_PROMPTS"))
        .unwrap()
        .lines()
        .take(PROMPTS)
        .map(|line| {
            let (hash, prompt) = line.split_once('\t').unwrap();
            (hash.to_string(), serde_json::from_str(prompt).unwrap())
        })
        .collect()
}

async fn cached_vector(cache: &mut sqlx::SqliteConnection, hash: &str) -> Vec<f32> {
    let model = &datalib_runtime::models::EMBED_MODELS[0];
    let key = format!("{}@{}", model.file, model.sha256);
    let row = sqlx::query("SELECT vector FROM vectors WHERE model = ? AND hash = ?")
        .bind(key)
        .bind(hash)
        .fetch_one(cache)
        .await
        .unwrap_or_else(|e| panic!("no cached vector for {hash}: {e}"));
    let bytes: Vec<u8> = row.get(0);
    let (words, _) = bytes.as_chunks::<4>();
    words.iter().map(|w| f32::from_le_bytes(*w)).collect()
}

#[tokio::test]
async fn embed_prompts_gives_the_vectors_the_embed_step_cached() {
    let prompts = fixture_prompts();
    assert_eq!(prompts.len(), PROMPTS, "the fixture has too few prompts");
    let input: String = prompts
        .iter()
        .map(|(hash, prompt)| format!("{}\n", serde_json::json!({"hash": hash, "prompt": prompt})))
        .collect();

    let mut child = Command::new(runfile("DATALIB_STEP_BIN"))
        .args(["embed-prompts", "--cpu", "--model"])
        .arg(runfile("EMBED_MODEL"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "embed-prompts failed: {stderr}");
    assert!(stderr.contains("embedding on CPU"), "{stderr}");

    let got: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(got.len(), PROMPTS);

    let td = tempfile::tempdir().unwrap();
    let copy = td.path().join("inference_cache.sqlite");
    std::fs::copy(runfile("SEARCH_INDEX_CACHE"), &copy).unwrap();
    let mut cache =
        sqlx::SqliteConnection::connect(&format!("sqlite://{}?mode=ro", copy.display()))
            .await
            .unwrap();
    for ((hash, _), line) in prompts.iter().zip(&got) {
        assert_eq!(line["hash"], hash.as_str());
        let vector: Vec<f32> = serde_json::from_value(line["vector"].clone()).unwrap();
        assert_eq!(vector.len(), 768);
        let c = cosine(&vector, &cached_vector(&mut cache, hash).await);
        assert!(c > 0.999, "cosine {c} for {hash}");
    }
    cache.close().await.unwrap();
}

#[test]
fn embed_prompts_refuses_a_line_that_is_not_a_prompt() {
    let mut child = Command::new(runfile("DATALIB_STEP_BIN"))
        .args(["embed-prompts", "--cpu", "--model"])
        .arg(runfile("EMBED_MODEL"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"hello\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("line 1"), "{stderr}");
}
