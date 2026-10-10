//! `GET /metrics`: what the steps report, what the loop makes of them and
//! what each tree weighs, in Prometheus's text exposition format, so any
//! tool that scrapes Prometheus — Prometheus itself, Grafana's agent, an
//! OpenTelemetry collector — can chart and alert on a data root. Behind
//! the API token like every route (`Authorization: Bearer`).
//!
//! A series' type is read off its name (`datalib_metrics`): one ending
//! in `_total` is a counter, anything else a gauge. A step's counters
//! start again each run, which a scraper reads as a reset.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use datalib_dag::supervisor::record::StepRecord;
use datalib_dag::supervisor::tick::StateKind;
use datalib_runs::MetricRow;
use strum::VariantArray;

use crate::usage::OutputStorage;
use crate::AppState;

const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Counter,
    Gauge,
}

impl Kind {
    fn of(name: &str) -> Self {
        if datalib_metrics::is_counter(name) {
            Kind::Counter
        } else {
            Kind::Gauge
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Family {
    pub name: String,
    pub help: String,
    pub kind: Kind,
    pub samples: Vec<Sample>,
}

/// A step the config declares, with the group it is filed under.
pub struct StepEntry {
    pub id: String,
    pub group: Option<String>,
}

/// Everything one scrape reads, gathered by the handler.
pub struct Inputs<'a> {
    pub steps: &'a [StepEntry],
    /// The newest value of every series, across runs.
    pub metrics: &'a [MetricRow],
    pub records: &'a BTreeMap<String, StepRecord>,
    pub trees: &'a [OutputStorage],
    pub root_bytes: u64,
    pub disk: &'a crate::disk_free::DiskFree,
}

/// A name as Prometheus allows one: `[a-zA-Z_:][a-zA-Z0-9_:]*`, anything
/// else turned into `_`.
fn metric_name(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == ':' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.chars().next().is_none_or(|c| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// A label name: as a metric name, without `:`.
fn label_name(raw: &str) -> String {
    metric_name(raw).replace(':', "_")
}

fn escape_value(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn escape_help(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\n', "\\n")
}

/// The run store's canonical labels, `k=v,k=v`, as pairs.
fn parse_labels(canonical: &str) -> Vec<(String, String)> {
    canonical
        .split(',')
        .filter(|kv| !kv.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (kv.to_string(), String::new()),
        })
        .collect()
}

/// A step's own labels, then the series' — one that would shadow `step`
/// or `group` renamed `exported_<name>`, as Prometheus renames a clash.
fn step_labels(step: &StepEntry, series: &[(String, String)]) -> Vec<(String, String)> {
    let mut out = vec![("step".to_string(), step.id.clone())];
    if let Some(g) = &step.group {
        out.push(("group".to_string(), g.clone()));
    }
    for (k, v) in series {
        let k = label_name(k);
        let k = if k == "step" || k == "group" {
            format!("exported_{k}")
        } else {
            k
        };
        out.push((k, v.clone()));
    }
    out
}

fn stamp_secs(iso: &str) -> Option<f64> {
    let t = datalib_time::parse_strict(iso).ok()?;
    Some(t.to_unix_millis() as f64 / 1000.0)
}

/// Every family one scrape serves, sorted by name.
pub fn families(inputs: &Inputs<'_>) -> Vec<Family> {
    let by_id: BTreeMap<&str, &StepEntry> =
        inputs.steps.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut out: BTreeMap<String, Family> = BTreeMap::new();
    let mut add = |name: String, help: String, kind: Kind, sample: Sample| {
        out.entry(name.clone())
            .or_insert_with(|| Family {
                name,
                help,
                kind,
                samples: Vec::new(),
            })
            .samples
            .push(sample);
    };

    for m in inputs.metrics {
        let Some(step) = by_id.get(m.step.as_str()) else {
            continue;
        };
        add(
            format!("datalib_step_{}", metric_name(&m.name)),
            format!(
                "`{}` as a datalib step reported it: its newest value, from the last run it \
                 reported in.",
                m.name
            ),
            Kind::of(&m.name),
            Sample {
                labels: step_labels(step, &parse_labels(&m.labels)),
                value: m.value as f64,
            },
        );
    }

    for step in inputs.steps {
        let record = inputs.records.get(&step.id);
        let state = record.and_then(|r| r.state);
        for kind in StateKind::VARIANTS {
            let mut labels = step_labels(step, &[]);
            labels.push(("state".to_string(), kind.as_str().to_string()));
            add(
                "datalib_step_state".into(),
                "1 for the state the sync loop last put the step in, 0 for every other.".into(),
                Kind::Gauge,
                Sample {
                    labels,
                    value: if state == Some(*kind) { 1.0 } else { 0.0 },
                },
            );
        }
        if let Some(at) = record
            .and_then(|r| r.last_success_at.as_deref())
            .and_then(stamp_secs)
        {
            add(
                "datalib_step_last_success_timestamp_seconds".into(),
                "When the step last succeeded or was found up to date, as a Unix time.".into(),
                Kind::Gauge,
                Sample {
                    labels: step_labels(step, &[]),
                    value: at,
                },
            );
        }
    }

    for t in inputs.trees.iter().filter(|t| t.present) {
        add(
            "datalib_tree_bytes".into(),
            "Bytes on disk under a tree of the data root, as the server last measured it.".into(),
            Kind::Gauge,
            Sample {
                labels: vec![("tree".to_string(), t.path.clone())],
                value: t.bytes as f64,
            },
        );
    }
    add(
        "datalib_root_bytes".into(),
        "Bytes on disk under the whole data root.".into(),
        Kind::Gauge,
        Sample {
            labels: Vec::new(),
            value: inputs.root_bytes as f64,
        },
    );
    let disk = inputs.disk;
    if let (Some(free), Some(total)) = (disk.available_bytes, disk.total_bytes) {
        for (name, help, value) in [
            (
                "datalib_disk_free_bytes",
                "Bytes still writable on the data root's disk, as last looked at.",
                free,
            ),
            (
                "datalib_disk_size_bytes",
                "The size of the data root's disk.",
                total,
            ),
        ] {
            add(name.into(), help.into(), Kind::Gauge, gauge(value as f64));
        }
    }
    for (name, help, value) in [
        (
            "datalib_disk_pause_below_bytes",
            "The config's [disk_space] pause line: under it every step is held.",
            disk.pause_below_bytes as f64,
        ),
        (
            "datalib_disk_resume_at_bytes",
            "The config's [disk_space] resume line: held steps run again from it.",
            disk.resume_at_bytes as f64,
        ),
        (
            "datalib_disk_low",
            "1 while the steps are held for want of disk space, else 0.",
            if disk.low { 1.0 } else { 0.0 },
        ),
    ] {
        add(name.into(), help.into(), Kind::Gauge, gauge(value));
    }
    out.into_values().collect()
}

fn gauge(value: f64) -> Sample {
    Sample {
        labels: Vec::new(),
        value,
    }
}

/// The text exposition format: per family, `# HELP`, `# TYPE`, then its
/// samples.
pub fn render(families: &[Family]) -> String {
    let mut out = String::new();
    for f in families {
        let _ = writeln!(out, "# HELP {} {}", f.name, escape_help(&f.help));
        let _ = writeln!(out, "# TYPE {} {}", f.name, f.kind.as_str());
        for s in &f.samples {
            out.push_str(&f.name);
            if !s.labels.is_empty() {
                let labels = s
                    .labels
                    .iter()
                    .map(|(k, v)| format!("{k}=\"{}\"", escape_value(v)))
                    .collect::<Vec<_>>()
                    .join(",");
                let _ = write!(out, "{{{labels}}}");
            }
            let _ = writeln!(out, " {}", s.value);
        }
    }
    out
}

pub async fn get_metrics(State(s): State<AppState>) -> impl IntoResponse {
    let config_path = s.config_path();
    let text = std::fs::read_to_string(&config_path).unwrap_or_default();
    let steps: Vec<StepEntry> = datalib_dag::written::entries_as_written(&text)
        .map(|w| {
            w.steps
                .into_iter()
                .map(|st| StepEntry {
                    id: st.id,
                    group: st.group,
                })
                .collect()
        })
        .unwrap_or_default();
    let metrics = datalib_runs::latest_metrics(&s.root).await;
    let records = match s.sync.mailbox().await {
        Ok(store) => store
            .load_record()
            .await
            .map(|r| r.steps)
            .unwrap_or_default(),
        Err(_) => Default::default(),
    };
    let storage = s
        .usage
        .snapshot(
            s.root.as_path(),
            &crate::usage::measured_trees(&config_path),
        )
        .await;
    let disk = s
        .usage
        .free
        .snapshot(crate::disk_free::floor_of(&config_path))
        .await;
    let body = render(&families(&Inputs {
        steps: &steps,
        metrics: &metrics,
        records: &records,
        trees: &storage.outputs,
        root_bytes: storage.root.bytes,
        disk: &disk,
    }));
    ([(header::CONTENT_TYPE, CONTENT_TYPE)], body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric(step: &str, name: &str, labels: &str, value: i64) -> MetricRow {
        MetricRow {
            step: step.into(),
            name: name.into(),
            labels: labels.into(),
            value,
            ..Default::default()
        }
    }

    fn scrape(metrics: &[MetricRow]) -> String {
        let steps = [
            StepEntry {
                id: "slack/ingest".into(),
                group: Some("slack".into()),
            },
            StepEntry {
                id: "custom".into(),
                group: None,
            },
        ];
        render(&families(&Inputs {
            steps: &steps,
            metrics,
            records: &BTreeMap::new(),
            trees: &[],
            root_bytes: 1000,
            disk: &disk(),
        }))
    }

    fn disk() -> crate::disk_free::DiskFree {
        crate::disk_free::DiskFree {
            available_bytes: Some(9_000_000_000),
            total_bytes: Some(500_000_000_000),
            pause_below_bytes: 10_000_000_000,
            resume_at_bytes: 15_000_000_000,
            low: true,
            history: Vec::new(),
            window_secs: 300,
        }
    }

    #[test]
    fn a_total_is_a_counter_and_anything_else_a_gauge() {
        let text = scrape(&[
            metric("slack/ingest", "rows_upserted_total", "table=messages", 12),
            metric("slack/ingest", "queued", "from=a/b", 3),
        ]);
        assert!(
            text.contains("# TYPE datalib_step_rows_upserted_total counter\n"),
            "{text}"
        );
        assert!(text.contains(
            "datalib_step_rows_upserted_total{step=\"slack/ingest\",group=\"slack\",table=\"messages\"} 12\n"
        ), "{text}");
        assert!(
            text.contains("# TYPE datalib_step_queued gauge\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "datalib_step_queued{step=\"slack/ingest\",group=\"slack\",from=\"a/b\"} 3\n"
            ),
            "{text}"
        );
        assert!(text.contains("datalib_root_bytes 1000\n"), "{text}");
    }

    /// The disk the root lives on, and the floor the loop holds steps
    /// under, so an alert can fire before syncs stop rather than after.
    #[test]
    fn the_disk_and_its_floor_are_gauges() {
        let text = scrape(&[]);
        for line in [
            "# TYPE datalib_disk_free_bytes gauge\n",
            "datalib_disk_free_bytes 9000000000\n",
            "datalib_disk_size_bytes 500000000000\n",
            "datalib_disk_pause_below_bytes 10000000000\n",
            "datalib_disk_resume_at_bytes 15000000000\n",
            "datalib_disk_low 1\n",
        ] {
            assert!(text.contains(line), "{line:?} missing from:\n{text}");
        }
    }

    /// A family's samples sit together under one `# TYPE`, which a
    /// scraper requires, however the rows arrived.
    #[test]
    fn each_family_is_declared_once_with_its_samples_together() {
        let text = scrape(&[
            metric("slack/ingest", "api_requests_total", "", 1),
            metric("custom", "queued", "", 2),
            metric("custom", "api_requests_total", "", 4),
        ]);
        assert_eq!(
            text.matches("# TYPE datalib_step_api_requests_total")
                .count(),
            1
        );
        let lines: Vec<&str> = text.lines().collect();
        let at = lines
            .iter()
            .position(|l| l.starts_with("# TYPE datalib_step_api_requests_total"))
            .unwrap();
        assert!(lines[at + 1].starts_with("datalib_step_api_requests_total{step=\"slack/ingest\""));
        assert!(lines[at + 2].starts_with("datalib_step_api_requests_total{step=\"custom\"}"));
    }

    #[test]
    fn names_labels_and_values_are_made_safe() {
        let text = scrape(&[
            metric(
                "slack/ingest",
                "bytes-fetched.total",
                "step=x,table=a\"b",
                1,
            ),
            metric("gone/step", "queued", "", 9),
        ]);
        assert!(text.contains(
            "datalib_step_bytes_fetched_total{step=\"slack/ingest\",group=\"slack\",exported_step=\"x\",table=\"a\\\"b\"} 1\n"
        ), "{text}");
        assert!(
            !text.contains("gone/step"),
            "a step no longer in the config is not served"
        );
    }

    #[test]
    fn every_state_is_a_series_and_the_steps_is_one() {
        let steps = [StepEntry {
            id: "s".into(),
            group: None,
        }];
        let records = BTreeMap::from([(
            "s".to_string(),
            StepRecord {
                state: Some(StateKind::Running),
                last_success_at: Some("2026-09-30T10:00:00.000000+00:00".into()),
                ..Default::default()
            },
        )]);
        let text = render(&families(&Inputs {
            steps: &steps,
            metrics: &[],
            records: &records,
            trees: &[],
            root_bytes: 0,
            disk: &disk(),
        }));
        assert!(
            text.contains("datalib_step_state{step=\"s\",state=\"running\"} 1\n"),
            "{text}"
        );
        assert!(
            text.contains("datalib_step_state{step=\"s\",state=\"idle\"} 0\n"),
            "{text}"
        );
        assert!(
            text.contains("datalib_step_last_success_timestamp_seconds{step=\"s\"} 1790762400\n"),
            "{text}"
        );
    }
}
