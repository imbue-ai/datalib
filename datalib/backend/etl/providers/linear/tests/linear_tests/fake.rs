//! A Linear that answers from memory: each connection's records, newest
//! `updatedAt` first, filtered the way Linear filters them, a page at a
//! time. Every request is counted per connection, which is what the
//! incremental tests measure.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use datalib_etl_linear::ingest::{Api, LinearError, Page, PageQuery};
use serde_json::{json, Value};

pub const HISTORY_PER_ISSUE: usize = 50;

#[derive(Default)]
pub struct FakeLinear {
    pub records: Mutex<BTreeMap<String, Vec<Value>>>,
    /// Each issue's whole history, newest first, served 50 at a time.
    pub history: Mutex<HashMap<String, Vec<Value>>>,
    pub requests: Mutex<BTreeMap<String, u64>>,
    /// Records served per connection, a bound record read twice counted
    /// twice.
    pub served: Mutex<BTreeMap<String, u64>>,
    /// `(connection, n)`: the n-th request for it fails, once.
    pub fail: Mutex<Option<(String, u64)>>,
    total: AtomicU64,
}

fn cmp_stamp(record: &Value, op: &str, bound: &Value) -> bool {
    let at = record
        .get("updatedAt")
        .and_then(Value::as_str)
        .unwrap_or("");
    let bound = bound.as_str().unwrap_or("");
    match op {
        "gte" => at >= bound,
        "lte" => at <= bound,
        other => panic!("the fake has no `{other}`"),
    }
}

/// Linear's filter language, as much of it as the download speaks:
/// `and`, `or`, `{field: {id: {in: [...]}}}`, a nested object filter,
/// and `updatedAt` bounds.
pub fn matches(filter: &Value, record: &Value) -> bool {
    let Some(f) = filter.as_object() else {
        return true;
    };
    f.iter().all(|(key, cond)| match key.as_str() {
        "and" => cond.as_array().unwrap().iter().all(|c| matches(c, record)),
        "or" => cond.as_array().unwrap().iter().any(|c| matches(c, record)),
        "updatedAt" => cond
            .as_object()
            .unwrap()
            .iter()
            .all(|(op, bound)| cmp_stamp(record, op, bound)),
        field => {
            let target = record.get(field).unwrap_or(&Value::Null);
            if let Some(list) = cond.pointer("/id/in").and_then(Value::as_array) {
                let id = target.get("id").and_then(Value::as_str);
                id.is_some_and(|id| list.iter().any(|v| v.as_str() == Some(id)))
            } else if target.is_object() {
                matches(cond, target)
            } else {
                false
            }
        }
    })
}

fn page_of(all: Vec<Value>, first: u32, after: Option<&str>) -> Page {
    let start: usize = after.map(|c| c.parse().unwrap()).unwrap_or(0);
    let end = (start + first as usize).min(all.len());
    Page {
        nodes: all[start.min(end)..end].to_vec(),
        has_next: end < all.len(),
        end_cursor: Some(end.to_string()),
    }
}

impl FakeLinear {
    pub fn with(records: Vec<(&str, Vec<Value>)>) -> Self {
        let fake = FakeLinear::default();
        for (root, list) in records {
            fake.set(root, list);
        }
        fake
    }

    pub fn set(&self, root: &str, list: Vec<Value>) {
        self.records.lock().unwrap().insert(root.to_string(), list);
    }

    /// Replaces the record with `id` in `root`, or adds it.
    pub fn put(&self, root: &str, record: Value) {
        let mut records = self.records.lock().unwrap();
        let list = records.entry(root.to_string()).or_default();
        list.retain(|r| r["id"] != record["id"]);
        list.push(record);
    }

    pub fn requests_for(&self, root: &str) -> u64 {
        self.requests
            .lock()
            .unwrap()
            .get(root)
            .copied()
            .unwrap_or(0)
    }

    pub fn served_for(&self, root: &str) -> u64 {
        self.served.lock().unwrap().get(root).copied().unwrap_or(0)
    }

    pub fn reset_counts(&self) {
        self.requests.lock().unwrap().clear();
        self.served.lock().unwrap().clear();
    }

    fn count(&self, root: &str) -> Result<(), LinearError> {
        self.total.fetch_add(1, Ordering::Relaxed);
        let n = {
            let mut r = self.requests.lock().unwrap();
            let n = r.entry(root.to_string()).or_default();
            *n += 1;
            *n
        };
        let mut fail = self.fail.lock().unwrap();
        if fail.as_ref().is_some_and(|(r, at)| r == root && *at == n) {
            *fail = None;
            return Err(LinearError::Permanent(format!(
                "{root} page {n} would not come"
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl Api for FakeLinear {
    async fn page(&self, q: &PageQuery<'_>) -> Result<Page, LinearError> {
        self.count(q.root)?;
        let mut all: Vec<Value> = self
            .records
            .lock()
            .unwrap()
            .get(q.root)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| q.filter.as_ref().is_none_or(|f| matches(f, r)))
            .collect();
        all.sort_by(|a, b| b["updatedAt"].as_str().cmp(&a["updatedAt"].as_str()));
        let mut page = page_of(all, q.first, q.after.as_deref());
        *self
            .served
            .lock()
            .unwrap()
            .entry(q.root.to_string())
            .or_default() += page.nodes.len() as u64;
        if q.root == "issues" {
            let history = self.history.lock().unwrap();
            for issue in &mut page.nodes {
                let id = issue["id"].as_str().unwrap().to_string();
                let h = history.get(&id).cloned().unwrap_or_default();
                let p = page_of(h, HISTORY_PER_ISSUE as u32, None);
                issue["history"] = json!({"nodes": p.nodes,
                    "pageInfo": {"hasNextPage": p.has_next, "endCursor": p.end_cursor}});
            }
        }
        Ok(page)
    }

    async fn issue_history(
        &self,
        issue_id: &str,
        after: Option<&str>,
    ) -> Result<Page, LinearError> {
        self.count("issue_history")?;
        let h = self
            .history
            .lock()
            .unwrap()
            .get(issue_id)
            .cloned()
            .unwrap_or_default();
        Ok(page_of(h, 100, after))
    }

    async fn whoami(&self) -> Result<Value, LinearError> {
        Ok(json!({"viewer": {"id": "u-picard"}, "organization": {"name": "Starfleet"}}))
    }

    fn requests(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
}

#[test]
fn the_fake_filters_like_linear() {
    let r = json!({"updatedAt": "2026-09-02", "team": {"id": "t1"}, "project": null,
                   "issue": {"team": {"id": "t2"}}});
    assert!(matches(&json!({"team": {"id": {"in": ["t1"]}}}), &r));
    assert!(!matches(&json!({"project": {"id": {"in": ["p1"]}}}), &r));
    assert!(matches(
        &json!({"issue": {"team": {"id": {"in": ["t2"]}}}}),
        &r
    ));
    assert!(matches(
        &json!({"and": [{"or": [{"project": {"id": {"in": []}}}, {"team": {"id": {"in": ["t1"]}}}]},
                         {"updatedAt": {"gte": "2026-09-01", "lte": "2026-09-02"}}]}),
        &r
    ));
    assert!(!matches(&json!({"updatedAt": {"lte": "2026-09-01"}}), &r));
}
