//! Linear's GraphQL API (`api.linear.app/graphql`) through `latchkey
//! curl`, which adds the credential. Every list the download reads is
//! one connection, asked for a page at a time in one shape
//! ([`PageQuery`]); [`Api`] is that one call, so a test can answer it
//! from memory.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use datalib_etl_web::http::{
    default_retryability, latchkey_curl_classified, HttpError, HttpRequest, HttpResponse,
    HttpService, LatchkeySettings, Retryability,
};

use super::fields;

pub const URL: &str = "https://api.linear.app/graphql";
const TIMEOUT: Duration = Duration::from_secs(120);

#[derive(thiserror::Error, Debug)]
pub enum LinearError {
    /// The credential was refused: nothing else this run asks will work.
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    /// Linear refused the request for its rate limit and named no wait,
    /// so nothing says when sending again would be admitted.
    #[error("rate limited: {0}")]
    RateLimited(String),
    /// The shared retry guard gave up on the service.
    #[error("{0}")]
    GaveUp(String),
    /// The request never got an answer to judge.
    #[error(transparent)]
    Transport(HttpError),
    #[error("{0}")]
    Permanent(String),
}

impl LinearError {
    /// Going on would only fail every request left.
    pub fn ends_the_run(&self) -> bool {
        matches!(
            self,
            Self::Unauthorized(_) | Self::RateLimited(_) | Self::GaveUp(_)
        )
    }
}

/// One page of one connection. `filter` is a value of the connection's
/// filter type (`filter_type`), or `None` for every record.
#[derive(Debug, Clone)]
pub struct PageQuery<'a> {
    pub root: &'a str,
    pub fields: &'a str,
    pub filter_type: &'a str,
    pub filter: Option<Value>,
    /// Arguments the connection takes beyond the common ones, written as
    /// GraphQL (`includeDisabled: true`).
    pub extra_args: &'a str,
    pub first: u32,
    pub after: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Page {
    pub nodes: Vec<Value>,
    pub has_next: bool,
    pub end_cursor: Option<String>,
}

#[async_trait]
pub trait Api: Send + Sync {
    /// One page of a connection, most recently updated first.
    async fn page(&self, q: &PageQuery<'_>) -> Result<Page, LinearError>;
    /// One page of an issue's history beyond what came with the issue.
    async fn issue_history(&self, issue_id: &str, after: Option<&str>)
        -> Result<Page, LinearError>;
    /// The account and its workspace: `{viewer, organization}`.
    async fn whoami(&self) -> Result<Value, LinearError>;
    fn requests(&self) -> u64;
}

/// The GraphQL document and variables for `q`. Archived records are
/// included: archiving is a change to a record, not its removal.
pub fn page_document(q: &PageQuery<'_>) -> (String, Value) {
    let (filter_var, filter_arg) = if q.filter_type.is_empty() {
        (String::new(), "")
    } else {
        (format!(", $filter: {}", q.filter_type), ", filter: $filter")
    };
    let extra = if q.extra_args.is_empty() {
        String::new()
    } else {
        format!(", {}", q.extra_args)
    };
    let doc = format!(
        "query($first: Int!, $after: String{filter_var}) {{ {root}(first: $first, after: $after, \
         includeArchived: true, orderBy: updatedAt{filter_arg}{extra}) {{ nodes {{ {fields} }} \
         pageInfo {{ hasNextPage endCursor }} }} }}",
        root = q.root,
        fields = q.fields,
    );
    let mut vars = json!({ "first": q.first, "after": q.after });
    if !q.filter_type.is_empty() {
        vars["filter"] = q.filter.clone().unwrap_or(Value::Null);
    }
    (doc, vars)
}

fn history_document() -> String {
    format!(
        "query($id: String!, $after: String) {{ issue(id: $id) {{ history(first: 100, after: \
         $after) {{ nodes {{ {} }} pageInfo {{ hasNextPage endCursor }} }} }} }}",
        fields::issue_history()
    )
}

/// The connection at `data.<path…>` as a [`Page`].
pub fn read_page(data: &Value, path: &[&str]) -> Result<Page, LinearError> {
    let mut at = data;
    for step in path {
        at = at.get(step).ok_or_else(|| {
            LinearError::Permanent(format!("the answer has no `{}`", path.join(".")))
        })?;
    }
    let nodes = at
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| LinearError::Permanent(format!("`{}` has no nodes", path.join("."))))?;
    let info = at.get("pageInfo");
    Ok(Page {
        nodes,
        has_next: info
            .and_then(|i| i.get("hasNextPage"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        end_cursor: info
            .and_then(|i| i.get("endCursor"))
            .and_then(Value::as_str)
            .map(String::from),
    })
}

/// Linear refusing a request for its rate limit: a 429, or a GraphQL
/// error coded `RATELIMITED` under any status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitRefusal {
    /// The `Retry-After` header as sent, when there was one.
    pub retry_after_header: Option<String>,
    /// That header in seconds, when it was a whole number.
    pub retry_after: Option<Duration>,
}

pub fn rate_limit_refusal(resp: &HttpResponse) -> Option<RateLimitRefusal> {
    let refused = resp.status == 429 || (resp.status != 200 && names_ratelimited(&resp.body));
    if !refused {
        return None;
    }
    let header = resp.header("retry-after").map(|h| h.trim().to_string());
    Some(RateLimitRefusal {
        retry_after: header
            .as_deref()
            .and_then(|h| h.parse::<u64>().ok())
            .map(Duration::from_secs),
        retry_after_header: header,
    })
}

fn names_ratelimited(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("errors").and_then(Value::as_array).cloned())
        .is_some_and(|errors| errors.iter().any(|e| error_code(e) == "RATELIMITED"))
}

/// A refusal is retried after exactly the wait its `Retry-After` names,
/// however long; the shared loop would otherwise cap it at ten minutes and
/// send again into another refusal. A refusal naming no wait in seconds is
/// handed back, and [`LinearClient::query`] ends the run with it.
pub fn linear_retryability(resp: &HttpResponse) -> Retryability {
    match rate_limit_refusal(resp) {
        Some(RateLimitRefusal {
            retry_after: Some(wait),
            ..
        }) => Retryability::RetryAfterUncapped(wait),
        Some(_) => Retryability::Accept,
        None => default_retryability(resp),
    }
}

fn refusal_message(refusal: &RateLimitRefusal, said: &str) -> String {
    let header = match &refusal.retry_after_header {
        Some(h) => format!("Retry-After: {h:?}"),
        None => "no Retry-After".to_string(),
    };
    format!(
        "Linear refused a request for its rate limit and named no wait in seconds ({header}): \
         {said}"
    )
}

/// What a GraphQL `errors` array means. Linear answers a spent budget
/// and a refused credential as errors with a code, under HTTP 400 as
/// often as under 429 or 401.
pub fn classify_errors(errors: &[Value]) -> LinearError {
    let said = describe_errors(errors);
    if errors.iter().any(|e| error_code(e) == "RATELIMITED") {
        LinearError::RateLimited(said)
    } else if errors
        .iter()
        .any(|e| matches!(error_code(e), "AUTHENTICATION_ERROR" | "FORBIDDEN"))
    {
        LinearError::Unauthorized(said)
    } else {
        LinearError::Permanent(said)
    }
}

fn error_code(e: &Value) -> &str {
    e.pointer("/extensions/code")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// Each error's message and code, as one line.
fn describe_errors(errors: &[Value]) -> String {
    let said: Vec<String> = errors
        .iter()
        .map(|e| {
            let msg = e
                .pointer("/extensions/userPresentableMessage")
                .or_else(|| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("(no message)");
            format!("{msg} [{}]", error_code(e))
        })
        .collect();
    said.join("; ")
}

pub struct LinearClient {
    latchkey: LatchkeySettings,
    requests: AtomicU64,
}

impl LinearClient {
    pub fn new(latchkey: LatchkeySettings) -> Self {
        Self {
            latchkey,
            requests: AtomicU64::new(0),
        }
    }

    pub async fn query(&self, document: &str, variables: Value) -> Result<Value, LinearError> {
        let body = json!({ "query": document, "variables": variables })
            .to_string()
            .into_bytes();
        let req = HttpRequest::post_json(HttpService::Linear, URL, body)
            .header("Accept", "application/json")
            .latchkey(self.latchkey.clone())
            .timeout(TIMEOUT);
        let resp = latchkey_curl_classified(&req, linear_retryability)
            .await
            .map_err(|e: HttpError| match e {
                HttpError::GaveUp { .. } => LinearError::GaveUp(e.to_string()),
                e => LinearError::Transport(e),
            })?;
        self.requests.fetch_add(1, Ordering::Relaxed);
        let text = resp.body_str();
        let parsed: Option<Value> = serde_json::from_str(&text).ok();
        let errors = parsed
            .as_ref()
            .and_then(|v| v.get("errors"))
            .and_then(Value::as_array);
        if let Some(refusal) = rate_limit_refusal(&resp) {
            let said = match errors.filter(|e| !e.is_empty()) {
                Some(errors) => describe_errors(errors),
                None => format!("HTTP {}", resp.status),
            };
            return Err(LinearError::RateLimited(refusal_message(&refusal, &said)));
        }
        if let Some(errors) = errors.filter(|e| !e.is_empty()) {
            return Err(classify_errors(errors));
        }
        match resp.status {
            200 => {}
            401 => return Err(LinearError::Unauthorized("HTTP 401".into())),
            status => {
                let preview: String = text.chars().take(300).collect();
                return Err(LinearError::Permanent(format!(
                    "HTTP {status}: {preview:?}"
                )));
            }
        }
        parsed
            .and_then(|mut v| v.get_mut("data").map(Value::take))
            .ok_or_else(|| LinearError::Permanent("HTTP 200 with no `data`".into()))
    }
}

#[async_trait]
impl Api for LinearClient {
    async fn page(&self, q: &PageQuery<'_>) -> Result<Page, LinearError> {
        let (doc, vars) = page_document(q);
        let data = self.query(&doc, vars).await?;
        read_page(&data, &[q.root])
    }

    async fn issue_history(
        &self,
        issue_id: &str,
        after: Option<&str>,
    ) -> Result<Page, LinearError> {
        let data = self
            .query(
                &history_document(),
                json!({ "id": issue_id, "after": after }),
            )
            .await?;
        read_page(&data, &["issue", "history"])
    }

    async fn whoami(&self) -> Result<Value, LinearError> {
        self.query(
            "{ viewer { id name displayName email } organization { id name urlKey } }",
            json!({}),
        )
        .await
    }

    fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_asks_newest_first_with_archived_records_and_the_filter_bound() {
        let q = PageQuery {
            root: "issues",
            fields: "id updatedAt",
            filter_type: "IssueFilter",
            filter: Some(json!({"team": {"id": {"in": ["t1"]}}})),
            extra_args: "",
            first: 50,
            after: Some("c1".into()),
        };
        let (doc, vars) = page_document(&q);
        assert_eq!(
            doc,
            "query($first: Int!, $after: String, $filter: IssueFilter) { issues(first: $first, \
             after: $after, includeArchived: true, orderBy: updatedAt, filter: $filter) { nodes \
             { id updatedAt } pageInfo { hasNextPage endCursor } } }"
        );
        assert_eq!(
            vars,
            json!({"first": 50, "after": "c1", "filter": {"team": {"id": {"in": ["t1"]}}}})
        );
    }

    #[test]
    fn a_connection_without_a_filter_takes_none_and_its_own_arguments() {
        let q = PageQuery {
            root: "users",
            fields: "id",
            filter_type: "",
            filter: None,
            extra_args: "includeDisabled: true",
            first: 100,
            after: None,
        };
        let (doc, vars) = page_document(&q);
        assert!(
            doc.contains("orderBy: updatedAt, includeDisabled: true)"),
            "{doc}"
        );
        assert!(!doc.contains("$filter"), "{doc}");
        assert_eq!(vars, json!({"first": 100, "after": null}));
    }

    #[test]
    fn a_page_is_read_with_its_cursor() {
        let data = json!({"issue": {"history": {"nodes": [{"id": "h1"}],
            "pageInfo": {"hasNextPage": true, "endCursor": "c9"}}}});
        let page = read_page(&data, &["issue", "history"]).unwrap();
        assert_eq!(page.nodes.len(), 1);
        assert!(page.has_next);
        assert_eq!(page.end_cursor.as_deref(), Some("c9"));
        assert!(read_page(&data, &["issues"]).is_err());
    }

    /// Linear answers a spent hourly budget as a GraphQL error under HTTP
    /// 400; read as an ordinary failure, every request left in the run
    /// would fail one at a time.
    #[test]
    fn a_spent_budget_and_a_refused_credential_end_the_run() {
        let limited = classify_errors(&[json!({"message": "Rate limit exceeded",
            "extensions": {"code": "RATELIMITED"}})]);
        assert!(matches!(limited, LinearError::RateLimited(_)));
        assert!(limited.ends_the_run());
        let refused = classify_errors(&[json!({"message": "no",
            "extensions": {"code": "AUTHENTICATION_ERROR"}})]);
        assert!(matches!(refused, LinearError::Unauthorized(_)));
        let bad = classify_errors(&[json!({"message": "Cannot query field",
            "extensions": {"code": "GRAPHQL_VALIDATION_FAILED"}})]);
        assert!(!bad.ends_the_run());
        assert!(bad.to_string().contains("Cannot query field"), "{bad}");
    }

    fn answer(status: u16, body: Value, retry_after: Option<&str>) -> HttpResponse {
        HttpResponse {
            status,
            headers: retry_after
                .map(|v| ("retry-after".to_string(), v.to_string()))
                .into_iter()
                .collect(),
            body: body.to_string().into_bytes(),
            duration_ms: 0,
        }
    }

    fn ratelimited() -> Value {
        json!({"errors": [{"message": "Rate limit exceeded: too many requests in the last hour.",
            "extensions": {"code": "RATELIMITED"}}]})
    }

    /// Every 429 was retried by the shared loop after at most ten minutes,
    /// so a refusal that asked for longer was sent again into another.
    #[test]
    fn a_rate_limit_refusal_is_retried_after_the_whole_wait_it_names() {
        for (status, wait) in [(429, "2"), (400, "60"), (429, "720"), (429, "3600")] {
            assert_eq!(
                linear_retryability(&answer(status, ratelimited(), Some(wait))),
                Retryability::RetryAfterUncapped(Duration::from_secs(wait.parse().unwrap())),
                "{status} {wait}"
            );
        }
        for no_wait in [
            answer(429, ratelimited(), None),
            answer(400, ratelimited(), None),
            answer(429, json!({}), Some("Wed, 21 Oct 2015 07:28:00 GMT")),
        ] {
            assert_eq!(
                linear_retryability(&no_wait),
                Retryability::Accept,
                "{no_wait:?}"
            );
            assert!(rate_limit_refusal(&no_wait).is_some(), "{no_wait:?}");
        }
        assert_eq!(
            linear_retryability(&answer(503, json!({}), None)),
            Retryability::Retry { retry_after: None }
        );
    }

    /// A query past Linear's per-query maximum never fits, so waiting
    /// cannot help: it fails that page and the run goes on.
    #[test]
    fn a_query_too_complex_is_neither_retried_nor_a_rate_limit() {
        let body = json!({"errors": [{"message": "Query too complex",
            "extensions": {"code": "INPUT_ERROR", "userPresentableMessage":
            "The query is too complex. Complexity: 12000. Maximum allowed complexity: 10000."}}]});
        let resp = answer(400, body.clone(), None);
        assert_eq!(linear_retryability(&resp), Retryability::Accept);
        assert_eq!(rate_limit_refusal(&resp), None);
        let e = classify_errors(body["errors"].as_array().unwrap());
        assert!(matches!(e, LinearError::Permanent(_)), "{e}");
        assert!(!e.ends_the_run());
    }

    #[test]
    fn a_refusal_naming_no_wait_says_what_it_sent() {
        let refusal = rate_limit_refusal(&answer(429, ratelimited(), Some("soon"))).unwrap();
        let said = refusal_message(&refusal, "Rate limit exceeded [RATELIMITED]");
        assert!(said.contains(r#"Retry-After: "soon""#), "{said}");
        assert!(said.contains("[RATELIMITED]"), "{said}");
    }
}
