use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::{Arc, Mutex};

use crate::problems::{self, Reason, ScopeKind, Severity, Stage};
use crate::*;

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buf {
    fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn check_set<T>(all: &[T])
where
    T: Copy
        + std::fmt::Debug
        + PartialEq
        + serde::Serialize
        + serde::de::DeserializeOwned
        + std::str::FromStr
        + Into<&'static str>,
{
    for &v in all {
        let word: &'static str = v.into();
        assert_eq!(serde_json::to_value(v).unwrap(), word, "{v:?}");
        assert_eq!(
            serde_json::from_value::<T>(serde_json::Value::from(word)).unwrap(),
            v
        );
        assert_eq!(word.parse::<T>().ok(), Some(v), "{v:?}");
    }
    assert!("no such word".parse::<T>().is_err());
    assert!(serde_json::from_str::<T>("\"no such word\"").is_err());
}

/// Every closed set has one spelling, which serde, `as_str` and
/// `FromStr` all use; a second spelling would read back as unknown.
#[test]
fn every_closed_set_reads_back_what_it_writes() {
    check_set(Verb::ALL);
    check_set(FailureKind::ALL);
    check_set(LogLevel::ALL);
    check_set(Severity::ALL);
    check_set(Stage::ALL);
    check_set(problems::Outcome::ALL);
    check_set(Reason::ALL);
    check_set(ScopeKind::ALL);
    assert_eq!(
        <Reason as strum::VariantArray>::VARIANTS,
        Reason::ALL,
        "the table derive reads the same list"
    );
}

fn one_of_each_event() -> Vec<Event> {
    vec![
        Event::Metric {
            name: "rows_upserted_total".into(),
            labels: BTreeMap::from([("table".into(), "messages".into())]),
            value: 1234,
        },
        Event::Metric {
            name: "queued".into(),
            labels: BTreeMap::new(),
            value: -1,
        },
        Event::ProgressLength { total: Some(42) },
        Event::ProgressLength { total: None },
        Event::ProgressInc { delta: 1 },
        Event::ProgressMessage {
            msg: "fetching page 3".into(),
        },
        Event::log(LogLevel::Warn, "careful"),
        Event::Log {
            level: LogLevel::Debug,
            msg: "hello".into(),
            ts: Some("2364-04-11T10:00:00+01:00".into()),
            target: Some("me::fetch".into()),
            thread: Some("worker".into()),
            fields: Some(serde_json::json!({"page": 3}).as_object().unwrap().clone()),
        },
        Event::Hint {
            msg: "sign in again".into(),
        },
        Event::Checkpoint {
            version: "a1b2c3".into(),
            rows: Some(340),
        },
        Event::Checkpoint {
            version: "a1b2c3".into(),
            rows: None,
        },
        Event::Capabilities {
            streams_output: true,
            version: None,
        },
        Event::Capabilities {
            streams_output: false,
            version: Some("ae2d52f0".into()),
        },
    ]
}

/// `Event::NAMES` is what `read_line` recognizes, so it must name every
/// kind as serde writes it.
#[test]
fn the_event_names_are_the_wire_words() {
    let mut written: Vec<String> = one_of_each_event()
        .iter()
        .map(|e| {
            serde_json::to_value(e).unwrap()["event"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    written.sort();
    written.dedup();
    let mut names: Vec<&str> = Event::NAMES.to_vec();
    names.sort();
    assert_eq!(written, names);
}

fn problem() -> Problem {
    Problem {
        severity: Some(Severity::Warning),
        sample: "HTTP 404".into(),
        upstream_entity_kind: Some("message".into()),
        upstream_id: Some("C01/1712345678.000100".into()),
        ..Problem::new(
            Stage::Fetch,
            ScopeKind::Entity,
            "messages:C01/1712345678.000100",
            problems::Outcome::Dropped,
            Reason::FetchFailed,
        )
    }
}

fn every_covered() -> Vec<Covered> {
    vec![
        Covered::All,
        Covered::Listings,
        Covered::Records {
            table: "messages".into(),
            all_but: Some(vec!["C01/1".into()]),
            only: None,
        },
        Covered::Records {
            table: "messages".into(),
            all_but: None,
            only: Some(vec!["C01/2".into()]),
        },
        Covered::Files {
            under: "notes".into(),
            all_but: None,
        },
    ]
}

fn outcome() -> Outcome {
    Outcome {
        outputs: vec![
            Output {
                rows: Some(12),
                ..Output::versioned("weather/ingest", "v1")
            },
            Output {
                path: "weather/ingest".into(),
                version: None,
                rows: None,
            },
        ],
        needs_rerun: true,
        store_bytes: Some(StoreBytes {
            before: 6_600_000_000,
            after: 3_100_000_000,
        }),
        warnings: vec!["`period` no longer does anything; delete it".into()],
        failure: Some(FailureKind::RateLimited),
    }
}

/// What the emitter prints, `read_line` reads back as the same line.
#[test]
fn every_line_the_emitter_prints_reads_back() {
    let buf = Buf::default();
    let out = Emitter::to_writer("me", buf.clone());
    let events = one_of_each_event();
    for e in &events {
        out.event(e).unwrap();
    }
    out.problem(&problem()).unwrap();
    for c in every_covered() {
        out.covered(&c).unwrap();
    }
    out.outcome(&outcome()).unwrap();
    out.outcome(&Outcome::default()).unwrap();

    let mut expected: Vec<Line> = events
        .into_iter()
        .map(|event| Line::Event {
            step: "me".into(),
            event,
        })
        .collect();
    expected.push(Line::Problem(problem()));
    expected.extend(every_covered().into_iter().map(Line::Covered));
    expected.push(Line::Outcome(outcome()));
    expected.push(Line::Outcome(Outcome::default()));
    let read: Vec<Line> = buf
        .lines()
        .iter()
        .map(|l| read_line(l).unwrap_or_else(|e| panic!("{l}: {e}")))
        .collect();
    assert_eq!(read, expected);
}

/// The lines `step_protocol.md` shows, as written there.
#[test]
fn the_documents_example_lines_read() {
    for line in [
        r#"{"event":"metric","step":"me","name":"rows_upserted_total","labels":{"table":"messages"},"value":1234}"#,
        r#"{"event":"metric","step":"me","name":"queued","value":17}"#,
        r#"{"event":"progress_message","step":"me","msg":"fetching page 3"}"#,
        r#"{"event":"log","step":"me","level":"info","msg":"hello","target":"me::fetch","fields":{"page":3}}"#,
        r#"{"event":"progress_length","step":"me","total":42}"#,
        r#"{"event":"progress_inc","step":"me","delta":1}"#,
        r#"{"event":"checkpoint","step":"me","version":"a1b2c3","rows":340}"#,
        r#"{"event":"capabilities","step":"me","streams_output":true}"#,
        r#"{"event":"capabilities","step":"me","version":"ae2d52f0"}"#,
    ] {
        assert!(matches!(read_line(line), Ok(Line::Event { .. })), "{line}");
    }
    let problem = r#"{"event":"problem","stage":"fetch","scope_kind":"entity","scope_key":"messages:C01/1712345678.000100","outcome":"dropped","reason":"fetch_failed","severity":"warning","sample":"HTTP 404","upstream_entity_kind":"message","upstream_id":"C01/1712345678.000100"}"#;
    assert_eq!(read_line(problem), Ok(Line::Problem(self::problem())));
    let covered = r#"{"event":"covered","kind":"records","table":"messages","all_but":["C01/1712345999.000200"]}"#;
    assert!(matches!(
        read_line(covered),
        Ok(Line::Covered(Covered::Records { .. }))
    ));
    for line in [
        r#"{"event":"outcome","outputs":[{"path":"weather/ingest","version":"2026-07-21T06:00Z-a1b2","rows":12}]}"#,
        r#"{"event":"outcome","failure":"rate_limited","outputs":[{"path":"weather/ingest","version":"2026-07-21T05:00Z-9f3c"}]}"#,
        r#"{"event":"outcome","needs_rerun":true,"outputs":[]}"#,
        r#"{"event":"outcome","warnings":["`period` no longer does anything; delete it"],"outputs":[]}"#,
        r#"{"event":"outcome","store_bytes":{"before":6600000000,"after":3100000000},"outputs":[]}"#,
        r#"{"event":"outcome","failure":"cancelled"}"#,
    ] {
        assert!(matches!(read_line(line), Ok(Line::Outcome(_))), "{line}");
    }
}

/// A line that is not the protocol's is plain text, and a line naming a
/// kind that does not read as one says which.
#[test]
fn what_is_not_a_protocol_line() {
    for text in [
        "hello",
        "",
        "123",
        "[1]",
        r#"{"level":"INFO","fields":{"message":"a tracing line"}}"#,
        r#"{"event":"no_such_kind","step":"me"}"#,
        r#"{"event":7}"#,
        // The runner's own report of a step, which a step cannot send.
        r#"{"event":"step_finish","step":"me","status":"succeeded"}"#,
    ] {
        assert_eq!(read_line(text), Ok(Line::Text), "{text}");
    }
    for (line, event) in [
        (r#"{"event":"progress_inc","delta":1}"#, "progress_inc"),
        (
            r#"{"event":"log","step":"me","level":"WARN","msg":"x"}"#,
            "log",
        ),
        (
            r#"{"event":"metric","step":"me","name":"x","value":1.5}"#,
            "metric",
        ),
        (r#"{"event":"outcome","failure":"bored"}"#, "outcome"),
        (r#"{"event":"problem","stage":"fetch"}"#, "problem"),
        (r#"{"event":"covered","kind":"some"}"#, "covered"),
    ] {
        assert_eq!(read_line(line).unwrap_err().event, event, "{line}");
    }
}

/// Lines printed from many threads at once each arrive whole.
#[test]
fn threads_printing_at_once_do_not_mix_their_lines() {
    let buf = Buf::default();
    let out = Emitter::to_writer("me", buf.clone());
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let out = out.clone();
            std::thread::spawn(move || {
                for i in 0..200 {
                    out.progress_message(format!("thread {t} line {i} {}", "x".repeat(500)))
                        .unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let lines = buf.lines();
    assert_eq!(lines.len(), 8 * 200);
    for l in &lines {
        assert!(matches!(read_line(l), Ok(Line::Event { .. })), "{l}");
    }
}

fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |name| map.get(name).map(OsString::from)
}

#[test]
fn an_invocation_reads_every_variable_and_the_verb() {
    let inv = Invocation::from_parts(
        vars(&[
            (env::DATALIB_DAG_STEP, "report/build"),
            (env::DATALIB_DAG_DATA_ROOT, "/data"),
            (env::DATALIB_DAG_RUN_ID, "r1"),
            (env::DATALIB_DAG_ATTEMPT, "2"),
            (env::DATALIB_DAG_NOW, "2364-04-11T10:00:00+01:00"),
            (env::DATALIB_DAG_PARAMS, r#"{"greeting":"hi"}"#),
            (env::DATALIB_DAG_INPUTS, "a/make\nb/make\n"),
            (env::DATALIB_DAG_CHANGED_INPUTS, "a/make"),
            (env::DATALIB_DAG_FAILED_INPUTS, "b/make"),
            (env::DATALIB_READS, r#"{"a/make":"f1:abc:def"}"#),
        ]),
        vec!["--station".into(), "KSFO".into(), "--reset".into()],
    )
    .unwrap();
    assert_eq!(inv.step, "report/build");
    assert_eq!(inv.output_dir(), std::path::Path::new("/data/report/build"));
    assert_eq!(inv.run_id.as_deref(), Some("r1"));
    assert_eq!(inv.attempt, Some(2));
    assert_eq!(inv.inputs, ["a/make", "b/make"]);
    assert!(inv.is_changed("a/make") && !inv.is_changed("b/make"));
    assert!(inv.is_failed("b/make") && !inv.is_failed("a/make"));
    assert_eq!(
        inv.read("a/make"),
        Some(Read {
            fingerprint: "f1",
            version: "abc:def"
        })
    );
    assert_eq!(inv.read("b/make"), None);
    assert_eq!(inv.verb, Some(Verb::Reset));
    assert_eq!(inv.args, ["--station", "KSFO"]);

    #[derive(serde::Deserialize)]
    struct P {
        greeting: String,
    }
    assert_eq!(inv.params::<P>().unwrap().greeting, "hi");
}

/// A step run by hand has only its id and data root; params it does not
/// have read as `{}`, and a key the step does not take is named.
#[test]
fn a_step_run_by_hand_needs_only_its_id_and_root() {
    let base = [
        (env::DATALIB_DAG_STEP, "g/s"),
        (env::DATALIB_DAG_DATA_ROOT, "/data"),
    ];
    let inv = Invocation::from_parts(vars(&base), vec!["--verbose".into()]).unwrap();
    assert_eq!(
        (inv.run_id.as_deref(), inv.attempt, inv.now.as_deref()),
        (None, None, None)
    );
    assert!(inv.inputs.is_empty() && inv.reads.is_empty());
    assert_eq!(
        (inv.verb, inv.args.as_slice()),
        (None, ["--verbose".to_string()].as_slice())
    );

    #[derive(serde::Deserialize, Default, Debug, PartialEq)]
    #[serde(default, deny_unknown_fields)]
    struct P {
        units: String,
    }
    assert_eq!(inv.params::<P>().unwrap(), P::default());
    let typo = Invocation {
        params_json: Some(r#"{"unitz":"metric"}"#.into()),
        ..inv
    };
    let e = typo.params::<P>().unwrap_err().to_string();
    assert!(
        e.starts_with("DATALIB_DAG_PARAMS: unknown field `unitz`"),
        "{e}"
    );

    for missing in [env::DATALIB_DAG_STEP, env::DATALIB_DAG_DATA_ROOT] {
        let some: Vec<_> = base
            .iter()
            .copied()
            .filter(|(k, _)| *k != missing)
            .collect();
        let e = Invocation::from_parts(vars(&some), vec![]).unwrap_err();
        assert_eq!(e.variable, missing);
    }
    for (bad, value) in [
        (env::DATALIB_DAG_ATTEMPT, "first"),
        (env::DATALIB_READS, "[1]"),
    ] {
        let mut some = base.to_vec();
        some.push((bad, value));
        assert_eq!(
            Invocation::from_parts(vars(&some), vec![])
                .unwrap_err()
                .variable,
            bad
        );
    }
}

#[test]
fn only_a_last_argument_is_a_verb() {
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        Verb::split_args(args(&["--check", "x"])),
        (args(&["--check", "x"]), None)
    );
    assert_eq!(
        Verb::split_args(args(&["x", "--prepare"])),
        (args(&["x"]), Some(Verb::Prepare))
    );
    assert_eq!(
        Verb::split_args(args(&["--frobnicate"])),
        (args(&["--frobnicate"]), None)
    );
    for &v in Verb::ALL {
        assert_eq!(Verb::split_args(vec![v.flag()]).1, Some(v));
    }
}

#[test]
fn a_tree_version_follows_paths_and_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("out");
    let empty = tree_version(&root).unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    assert_eq!(
        tree_version(&root).unwrap(),
        empty,
        "an empty tree is a missing one"
    );
    std::fs::write(root.join("sub/a"), "one").unwrap();
    std::fs::write(root.join("b"), "two").unwrap();
    let first = tree_version(&root).unwrap();
    assert_eq!(first.len(), 32);
    assert_eq!(
        tree_version(&root).unwrap(),
        first,
        "the same tree, the same version"
    );
    std::fs::write(root.join("b"), "too").unwrap();
    let changed = tree_version(&root).unwrap();
    assert_ne!(changed, first);
    std::fs::rename(root.join("b"), root.join("c")).unwrap();
    assert_ne!(tree_version(&root).unwrap(), changed, "a rename moves it");
}

/// The README's step is the example binary's source, as written.
#[test]
fn the_readme_holds_the_example_as_written() {
    let readme = include_str!("../README.md");
    let example = include_str!("../examples/minimal_step.rs");
    assert!(
        readme.contains(&format!("```rust\n{example}```")),
        "README.md's step differs from examples/minimal_step.rs"
    );
}

fn step_protocol_doc() -> String {
    const REL: &str = "docs/dev/step_protocol.md";
    let r = runfiles::Runfiles::create().expect("runfiles tree");
    let path = r
        .rlocation(format!("_main/{REL}"))
        .unwrap_or_else(|| panic!("rlocation for {REL}"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The protocol doc's variable table names what the runner sets and
/// nothing else. Its other table is the knobs `datalib-step` reads
/// (`DATALIB_STEP_*`), which the runner only hands on.
#[test]
fn the_step_protocol_doc_names_the_same_variables() {
    let doc = step_protocol_doc();
    let table: std::collections::BTreeSet<&str> = doc
        .lines()
        .filter(|l| l.starts_with("| `DATALIB_"))
        .filter_map(|l| l.trim_start_matches("| `").split('`').next())
        .filter(|name| !name.starts_with("DATALIB_STEP_"))
        .collect();
    let ours: std::collections::BTreeSet<&str> = env::ALL.iter().copied().collect();
    assert_eq!(
        table, ours,
        "docs/dev/step_protocol.md's variable table and env::ALL disagree"
    );
}

#[test]
fn a_name_is_its_own_value() {
    assert_eq!(env::DATALIB_DAG_STEP, "DATALIB_DAG_STEP");
    assert_eq!(env::DATALIB_READS, "DATALIB_READS");
}
