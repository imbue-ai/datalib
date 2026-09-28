//! A config that still has a `qmd_index` step is rewritten by the server
//! itself, at boot and on a hand edit, with the old
//! text kept as `config.toml.bak`.

use datalib_http::ApiToken;
use std::path::Path;
use std::time::{Duration, Instant};

const SHARED_QMD_INDEX: &str = r#"# my sources
[[groups]]
id = "mail"
type = "email"

[[steps]]
group = "mail"
function = "ingest"
[steps.params.mbox]
path = "/nowhere/mail.mbox"

[[steps]]
group = "mail"
function = "render_markdown"
inputs = ["mail/ingest"]

[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "qmd_index"
inputs = ["mail/render_markdown"]
"#;

async fn boot(root: &Path) -> datalib_http::AppState {
    datalib_http::build_state(
        root.to_path_buf(),
        None,
        None,
        ApiToken::from_value("config-upgrade-test-token", root),
    )
    .await
    .expect("the server boots")
}

fn is_upgraded(text: &str) -> bool {
    text.contains("function = \"qmd_aggregator\"")
        && text.contains("function = \"keyword_index\"")
        && text.contains("function = \"embed\"")
        && !text.contains("function = \"qmd_index\"")
}

#[tokio::test]
async fn boot_upgrades_the_config_and_keeps_the_old_one() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.toml");
    std::fs::write(&config, SHARED_QMD_INDEX).unwrap();

    let _state = boot(tmp.path()).await;

    let text = std::fs::read_to_string(&config).unwrap();
    assert!(is_upgraded(&text), "{text}");
    assert!(
        text.starts_with("# my sources"),
        "a text edit keeps the file: {text}"
    );
    let check = datalib_dag::config::check_text(&text);
    assert!(check.is_clean(), "{:?}\n{text}", check.diagnostics);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("config.toml.bak")).unwrap(),
        SHARED_QMD_INDEX
    );
}

/// The file a person or an agent writes while the server runs is upgraded
/// too, not only the one it found at boot.
#[tokio::test]
async fn a_hand_edit_in_the_old_shape_is_upgraded_while_running() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.toml");
    let _state = boot(tmp.path()).await;

    // Boot does not wait for the root watcher to be listening, so the edit
    // is made again, now and then, until it is seen.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut next_edit = Instant::now();
    loop {
        let text = std::fs::read_to_string(&config).unwrap_or_default();
        if is_upgraded(&text) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the config was never upgraded:\n{text}"
        );
        if Instant::now() >= next_edit {
            std::fs::write(&config, SHARED_QMD_INDEX).unwrap();
            next_edit = Instant::now() + Duration::from_secs(1);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("config.toml.bak")).unwrap(),
        SHARED_QMD_INDEX
    );
}
