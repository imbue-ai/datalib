//! Smoke test for the issue-#41 config model BEFORE fanning out to all 16
//! providers: prove that an internally-tagged **newtype** enum over the real
//! `SlackConfig`/`EmailConfig` crates, each composing a nested `common:`
//! (composition, no `flatten`), round-trips through `serde_yaml` and that the
//! orchestrator-side `normalize()` (fold defaults + resolve paths) produces the
//! expected self-contained tree.
//!
//! This is throwaway scaffolding — the real `Config`/`SourceConfig`/`normalize`
//! land in `ingest_config`. Kept tiny and standalone so a `serde_yaml` surprise
//! shows up here, cheaply, rather than after touching every provider.

use std::path::{Path, PathBuf};

use frankweiler_etl_email_config::EmailConfig;
use frankweiler_etl_slack_config::SlackConfig;
use frankweiler_source_common::{Defaults, SourceCommon};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Config {
    data_root: PathBuf,
    #[serde(default)]
    defaults: Defaults,
    #[serde(default)]
    sources: Vec<SourceEntry>,
}

#[derive(Debug, Deserialize)]
struct SourceEntry {
    name: String,
    #[serde(default = "default_true")]
    enabled: bool,
    source: SourceConfig,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SourceConfig {
    SlackApi(SlackConfig),
    Email(EmailConfig),
}

impl SourceConfig {
    fn common_mut(&mut self) -> &mut SourceCommon {
        match self {
            SourceConfig::SlackApi(c) => &mut c.common,
            SourceConfig::Email(c) => &mut c.common,
        }
    }
}

fn default_true() -> bool {
    true
}

/// The single locus of mechanism, mirrored from the real `ingest_config`.
fn normalize(cfg: &mut Config) {
    for entry in &mut cfg.sources {
        let common = entry.source.common_mut();
        common.fold_defaults(&cfg.defaults);
        common.resolve_paths(&cfg.data_root, &entry.name);
    }
}

const YAML: &str = r#"
data_root: /data
defaults:
  blob_size_limit_bytes: 5000000
  extract_params:
    maximum_sequential_failed_requests: 50
sources:
  - name: slack
    source:
      type: slack_api
      common:
        blob_size_limit_bytes: 1000000
        extract_params:
          maximum_sequential_failed_requests: 100
      sync:
        media: true
        channels: ["proj"]
  - name: gmail
    enabled: false
    source:
      type: email
      common:
        input_path: /exports/mail.mbox
      outlink_format: gmail
"#;

#[test]
fn newtype_union_with_nested_common_round_trips_and_normalizes() {
    let mut cfg: Config = serde_yaml::from_str(YAML).expect("parse");
    normalize(&mut cfg);

    assert_eq!(cfg.sources.len(), 2);

    // --- slack: newtype tag dispatch + source-wins override + path default ---
    let slack = &cfg.sources[0];
    assert_eq!(slack.name, "slack");
    assert!(slack.enabled); // default true
    let SourceConfig::SlackApi(sc) = &slack.source else {
        panic!("expected slack_api, got {:?}", slack.source);
    };
    assert!(sc.sync.as_ref().unwrap().media);
    assert_eq!(sc.common.blob_size_limit_bytes, Some(1_000_000)); // source wins
    assert_eq!(sc.common.extract_params.max_sequential_failures(), 100); // source wins
    assert_eq!(sc.common.raw_path(), Path::new("/data/raw/slack")); // defaulted
    assert!(sc.common.input_path.is_none()); // API source, stays None

    // --- gmail: other arm, enabled override, default fold-through, explicit input ---
    let gmail = &cfg.sources[1];
    assert_eq!(gmail.name, "gmail");
    assert!(!gmail.enabled);
    let SourceConfig::Email(ec) = &gmail.source else {
        panic!("expected email, got {:?}", gmail.source);
    };
    assert_eq!(ec.common.blob_size_limit_bytes, Some(5_000_000)); // folded from defaults
    assert_eq!(ec.common.extract_params.max_sequential_failures(), 50); // folded from defaults
    assert_eq!(
        ec.common.input_or_raw_path(),
        Path::new("/exports/mail.mbox")
    );
    assert_eq!(ec.common.raw_path(), Path::new("/data/raw/gmail"));
    assert!(ec.outlink_format.is_some());
}

#[test]
fn unknown_type_is_rejected() {
    let bad = r#"
data_root: /data
sources:
  - name: x
    source:
      type: not_a_real_provider
"#;
    assert!(serde_yaml::from_str::<Config>(bad).is_err());
}
