//! A JMAP account as upstream holds it at one moment, and the playback
//! answers a download of it can ask for. Each request is keyed on its
//! exact bytes, built by `api::method_request` as the provider builds it,
//! and a later write to the same request replaces the answer.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use datalib_etl::http::{HttpRequest, HttpResponse, HttpService};
use datalib_etl::synthesize::{json_response, write_fixture};
use datalib_etl_email::ingest::api;
use datalib_etl_email::ingest::session::Session;
use serde_json::{json, Value};

pub const HOST: &str = "jmap.example.test";
pub const ACCOUNT: &str = "A1";

#[derive(Clone)]
pub struct Email {
    pub mailboxes: Vec<String>,
    pub thread: String,
    pub flagged: bool,
}

#[derive(Clone)]
pub struct Account {
    pub mailbox_state: String,
    pub email_state: String,
    pub mailboxes: BTreeMap<String, String>,
    pub emails: BTreeMap<String, Email>,
}

impl Account {
    /// Mailboxes as `(id, name)`, emails as `(id, the mailboxes it is
    /// filed under)`, each email its own thread `T<id>`.
    pub fn new(mailboxes: &[(&str, &str)], emails: &[(&str, &[&str])]) -> Self {
        Self {
            mailbox_state: "mbox-1".into(),
            email_state: "email-1".into(),
            mailboxes: mailboxes
                .iter()
                .map(|(id, name)| (id.to_string(), name.to_string()))
                .collect(),
            emails: emails
                .iter()
                .map(|(id, filed)| {
                    let email = Email {
                        mailboxes: filed.iter().map(|m| m.to_string()).collect(),
                        thread: format!("T{id}"),
                        flagged: false,
                    };
                    (id.to_string(), email)
                })
                .collect(),
        }
    }

    pub fn ids(&self) -> Vec<String> {
        self.emails.keys().cloned().collect()
    }

    fn email(&self, id: &str) -> Option<Value> {
        let e = self.emails.get(id)?;
        let filed: serde_json::Map<String, Value> = e
            .mailboxes
            .iter()
            .map(|m| (m.clone(), Value::Bool(true)))
            .collect();
        let mut keywords = json!({ "$seen": true });
        if e.flagged {
            keywords["$flagged"] = json!(true);
        }
        Some(json!({
            "id": id,
            "blobId": format!("B{id}"),
            "threadId": e.thread,
            "mailboxIds": filed,
            "keywords": keywords,
            "subject": format!("Stardate log {id}"),
            "receivedAt": "2026-09-01T10:00:00Z",
            "size": 256,
            "hasAttachment": false,
        }))
    }
}

pub fn status(code: u16) -> HttpResponse {
    HttpResponse {
        status: code,
        headers: BTreeMap::new(),
        body: b"{}".to_vec(),
        duration_ms: 0,
    }
}

fn eml(id: &str) -> HttpResponse {
    let body = format!(
        "Message-ID: <{id}@enterprise.starfleet>\r\n\
         Date: Tue, 1 Sep 2026 10:00:00 +0000\r\n\
         From: data@enterprise.starfleet\r\n\
         Subject: Stardate log {id}\r\n\
         \r\n\
         body of {id}\r\n",
    );
    HttpResponse {
        status: 200,
        headers: [("content-type".to_string(), "message/rfc822".to_string())].into(),
        body: body.into_bytes(),
        duration_ms: 0,
    }
}

pub struct Tape {
    out: PathBuf,
    session: Session,
    /// Ids per `Email/query` answer, whatever limit the request names: a
    /// server may return fewer than it was asked for.
    pub query_page: usize,
}

impl Tape {
    pub fn new(out: &Path) -> Self {
        let session_json = json!({
            "apiUrl": "https://jmap.example.test/jmap/api/",
            "downloadUrl": "https://jmap.example.test/jmap/download/{accountId}/{blobId}/{name}?type={type}",
            "uploadUrl": "https://jmap.example.test/jmap/upload/{accountId}/",
            "primaryAccounts": { "urn:ietf:params:jmap:mail": ACCOUNT },
            "accounts": { ACCOUNT: { "name": "t@example.test", "isPersonal": true } },
        });
        write_fixture(
            out,
            &HttpRequest::get(
                HttpService::Jmap,
                format!("https://{HOST}/.well-known/jmap"),
            ),
            &json_response(&session_json),
        )
        .expect("write the session fixture");
        Self {
            out: out.to_path_buf(),
            session: Session::from_value(session_json).expect("parse the fixture session"),
            query_page: 500,
        }
    }

    fn answer(&self, method: &str, args: Value, response: &HttpResponse) {
        let req = api::method_request(&self.session, method, args).expect("build the request");
        write_fixture(&self.out, &req, response).expect("write fixture");
    }

    pub fn call(&self, method: &str, args: Value, result: Value) {
        let body = json!({ "methodResponses": [[method, result, "a"]] });
        self.answer(method, args, &json_response(&body));
    }

    pub fn refuse(&self, method: &str, args: Value, code: u16) {
        self.answer(method, args, &status(code));
    }

    /// A JMAP method-level error, as `cannotCalculateChanges` arrives.
    pub fn method_error(&self, method: &str, args: Value, kind: &str) {
        let body = json!({ "methodResponses": [["error", { "type": kind }, "a"]] });
        self.answer(method, args, &json_response(&body));
    }

    /// Everything a run finds when upstream is `a` and has not moved
    /// since the states `a` names: the full mailbox list, the unfiltered
    /// `Email/query` pages, the state, both `/changes` saying nothing
    /// changed, one `Email/get` of every email, and every `.eml`.
    pub fn serve(&self, a: &Account) {
        self.call(
            "Mailbox/get",
            mailbox_get_args(None),
            json!({ "state": a.mailbox_state, "list": mailbox_list(a, None) }),
        );
        self.mailbox_changes(a, &a.mailbox_state, &[], &[]);
        self.email_changes(&a.email_state, &[], &[], &[], &a.email_state, false);
        self.gets(a, &[]);
        self.query(a, &[]);
        let ids = a.ids();
        self.gets(a, &ids.iter().map(String::as_str).collect::<Vec<_>>());
        for id in &ids {
            self.blob(id, eml(id));
        }
    }

    /// The `Email/query` pages for `a`'s emails under `in_mailboxes`, or
    /// for all of them.
    pub fn query(&self, a: &Account, in_mailboxes: &[&str]) {
        let ids: Vec<&String> = a
            .emails
            .iter()
            .filter(|(_, e)| {
                in_mailboxes.is_empty()
                    || e.mailboxes
                        .iter()
                        .any(|m| in_mailboxes.contains(&m.as_str()))
            })
            .map(|(id, _)| id)
            .collect();
        let pages: Vec<&[&String]> = if ids.is_empty() {
            vec![&[]]
        } else {
            ids.chunks(self.query_page).collect()
        };
        let mut position = 0;
        for page in pages {
            self.call(
                "Email/query",
                query_args(position, in_mailboxes),
                json!({ "ids": page, "queryState": "q-1", "total": ids.len() }),
            );
            position += page.len();
        }
    }

    /// One `Email/get` of exactly `ids`: the ones `a` has in `list`, the
    /// rest in `notFound`.
    pub fn gets(&self, a: &Account, ids: &[&str]) {
        let list: Vec<Value> = ids.iter().filter_map(|id| a.email(id)).collect();
        let not_found: Vec<&&str> = ids
            .iter()
            .filter(|id| !a.emails.contains_key(**id))
            .collect();
        self.call(
            "Email/get",
            email_get_args(ids),
            json!({ "state": a.email_state, "list": list, "notFound": not_found }),
        );
    }

    pub fn email_changes(
        &self,
        since: &str,
        created: &[&str],
        updated: &[&str],
        destroyed: &[&str],
        new_state: &str,
        has_more: bool,
    ) {
        self.call(
            "Email/changes",
            changes_args(since),
            json!({
                "created": created, "updated": updated, "destroyed": destroyed,
                "newState": new_state, "hasMoreChanges": has_more,
            }),
        );
    }

    /// `Mailbox/changes` from `since` to `a`'s state, and the
    /// `Mailbox/get` of what it names as changed.
    pub fn mailbox_changes(&self, a: &Account, since: &str, changed: &[&str], destroyed: &[&str]) {
        self.call(
            "Mailbox/changes",
            changes_args(since),
            json!({
                "created": [], "updated": changed, "destroyed": destroyed,
                "newState": a.mailbox_state, "hasMoreChanges": false,
            }),
        );
        if !changed.is_empty() {
            self.call(
                "Mailbox/get",
                mailbox_get_args(Some(changed)),
                json!({ "state": a.mailbox_state, "list": mailbox_list(a, Some(changed)) }),
            );
        }
    }

    pub fn blob(&self, email_id: &str, response: HttpResponse) {
        let url = self.session.download_url_for(
            ACCOUNT,
            &format!("B{email_id}"),
            "message.eml",
            "message/rfc822",
        );
        write_fixture(
            &self.out,
            &HttpRequest::get(HttpService::Jmap, url),
            &response,
        )
        .expect("write a blob fixture");
    }
}

fn mailbox_list(a: &Account, only: Option<&[&str]>) -> Vec<Value> {
    a.mailboxes
        .iter()
        .filter(|(id, _)| only.is_none_or(|only| only.contains(&id.as_str())))
        .map(|(id, name)| json!({ "id": id, "name": name, "parentId": null }))
        .collect()
}

pub fn mailbox_get_args(ids: Option<&[&str]>) -> Value {
    json!({ "accountId": ACCOUNT, "ids": ids })
}

pub fn changes_args(since: &str) -> Value {
    json!({ "accountId": ACCOUNT, "sinceState": since, "maxChanges": 5000 })
}

pub fn query_args(position: usize, in_mailboxes: &[&str]) -> Value {
    let mut args = json!({
        "accountId": ACCOUNT,
        "sort": [{ "property": "receivedAt", "isAscending": false }],
        "limit": 500,
        "position": position,
        "calculateTotal": true,
    });
    match in_mailboxes {
        [] => {}
        [one] => args["filter"] = json!({ "inMailbox": one }),
        many => {
            let conditions: Vec<Value> = many.iter().map(|m| json!({ "inMailbox": m })).collect();
            args["filter"] = json!({ "operator": "OR", "conditions": conditions });
        }
    }
    args
}

pub fn email_get_args(ids: &[&str]) -> Value {
    json!({
        "accountId": ACCOUNT,
        "ids": ids,
        "properties": [
            "id", "blobId", "threadId", "mailboxIds", "keywords", "from",
            "subject", "sentAt", "receivedAt", "size", "messageId",
            "hasAttachment", "attachments",
        ],
    })
}
