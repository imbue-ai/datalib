//! The walk down from configured roots: how a roots source's first sync
//! learns what sits under them, in requests that scale with what it
//! finds rather than with the workspace. A page or a block lists its
//! children (`GET /v1/blocks/{id}/children`); a database lists the rows
//! of each of its data sources (`POST /v1/data_sources/{id}/query`).
//! Everything it finds goes into `tree`, each page listed at its stamp
//! in `listed_pages`, as the search would have placed it.

use std::collections::{HashSet, VecDeque};

use anyhow::Result;
use serde_json::Value;

use super::db::{PageUpsert, Placed, TreeUpsert};
use super::official::NotionOfficialError;
use super::{run_over, Ctx, FetchSummary};

/// Rows past which one data source's listing may have been cut at
/// Notion's cap on a query.
const QUERY_CAP: usize = 10_000;

/// How a walk down ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Walked {
    /// Everything under the roots was listed.
    Whole,
    /// Everything was listed, but a database's rows reached Notion's cap
    /// on a query, so rows past it may be missing: only the search can
    /// list them.
    Capped,
    /// A listing would not come, or the run was stopped.
    Short,
}

/// Something under a root whose own children are listed next.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Node {
    /// A page, or a block that holds other blocks: its children.
    Blocks(String),
    /// A database: its data sources' rows.
    Database(String),
}

impl Node {
    fn of(placed: &Placed) -> Self {
        match placed {
            Placed::Other(t) if t.kind == super::schema_raw::TreeKind::Database => {
                Node::Database(t.id.clone())
            }
            p => Node::Blocks(p.id().to_string()),
        }
    }
}

/// Of one page of a children listing, what goes into `tree`: every child
/// page and child database, and every other block with children, which
/// a page can sit in (a toggle, a column). A table's children are its
/// rows of cells, which hold no page.
pub fn read_children(results: &[Value]) -> Vec<Placed> {
    results
        .iter()
        .filter(|b| {
            let kind = b.get("type").and_then(Value::as_str);
            let nests = b.get("has_children").and_then(Value::as_bool) == Some(true);
            matches!(kind, Some("child_page" | "child_database"))
                || (nests && kind != Some("table"))
        })
        .filter_map(|b| Some(Placed::from_block(b.get("id")?.as_str()?, b)))
        .collect()
}

/// How a listing that did not answer bears on the walk.
enum Unlisted {
    /// Notion has not got it, or will not show it: there is nothing
    /// under it to find.
    Nothing,
    /// The walk has a hole, so it is not complete.
    Hole(String),
}

fn unlisted(e: NotionOfficialError) -> Result<Unlisted> {
    match e {
        e if e.ends_the_run() => Err(run_over(e)),
        NotionOfficialError::NotFound(_) | NotionOfficialError::Forbidden(_) => {
            Ok(Unlisted::Nothing)
        }
        e => Ok(Unlisted::Hole(e.to_string())),
    }
}

impl Ctx<'_> {
    /// Walks down from `roots`, placing what it finds as it goes. A
    /// listing that would not come leaves a `listing:roots` row. A root
    /// Notion does not show is left unplaced, for the roots walk to ask
    /// for as a page.
    pub async fn walk_down(&self, roots: &[String], s: &mut FetchSummary) -> Result<Walked> {
        let mut queue: VecDeque<Node> = VecDeque::new();
        let mut seen: HashSet<Node> = HashSet::new();
        let mut holes: Vec<String> = Vec::new();
        let mut capped: Vec<String> = Vec::new();
        for root in roots {
            match self.client.get_block(root).await {
                Ok(block) => {
                    let placed = Placed::from_block(root, &block);
                    queue.push_back(Node::of(&placed));
                    self.place(&[placed], s).await?;
                }
                Err(_) if self.stop().requested() => return Ok(Walked::Short),
                Err(e) => match unlisted(e)? {
                    Unlisted::Nothing => {}
                    Unlisted::Hole(e) => holes.push(format!("{root}: {e}")),
                },
            }
        }
        while let Some(node) = queue.pop_front() {
            if self.stop().requested() {
                return Ok(Walked::Short);
            }
            if !seen.insert(node.clone()) {
                continue;
            }
            let found = match &node {
                Node::Blocks(id) => self.children(id, s).await?,
                Node::Database(id) => self.rows(id, &mut capped, s).await?,
            };
            match found {
                Ok(next) => queue.extend(next),
                Err(Unlisted::Nothing) => {}
                Err(Unlisted::Hole(e)) => holes.push(e),
            }
        }
        if self.stop().requested() {
            return Ok(Walked::Short);
        }
        if let Some(first) = holes.first() {
            self.found.listing(
                "roots",
                format!(
                    "{} listings under the roots would not come, so the next run walks \
                     down from the roots again; the first: {first}",
                    holes.len()
                ),
            );
            return Ok(Walked::Short);
        }
        if let Some(first) = capped.first() {
            self.found.listing(
                "roots",
                format!(
                    "{} databases under the roots listed {QUERY_CAP} rows or more, Notion's cap \
                     on a query, so this run listed the whole workspace through the search to \
                     find the rest; the first: {first}",
                    capped.len()
                ),
            );
            return Ok(Walked::Capped);
        }
        Ok(Walked::Whole)
    }

    async fn place(&self, placed: &[Placed], s: &mut FetchSummary) -> Result<()> {
        let mut tx = self.db.pool().begin().await?;
        self.db.store_placed(&mut tx, placed).await?;
        tx.commit().await?;
        self.wrote(placed.len() as u64).await;
        s.placed += placed
            .iter()
            .filter(|p| matches!(p, Placed::Page(_)))
            .count();
        self.bar.doing(&format!("walk: {} pages placed", s.placed));
        Ok(())
    }

    /// Every child of the page or block `id`, page by page of the
    /// listing; what holds more is the walk's next.
    async fn children(
        &self,
        id: &str,
        s: &mut FetchSummary,
    ) -> Result<Result<Vec<Node>, Unlisted>> {
        let mut next: Vec<Node> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let resp = match self.client.get_block_children(id, cursor.as_deref()).await {
                Ok(resp) => resp,
                Err(_) if self.stop().requested() => return Ok(Ok(next)),
                Err(e) => {
                    return Ok(Err(match unlisted(e)? {
                        Unlisted::Hole(e) => Unlisted::Hole(format!("{id}: {e}")),
                        nothing => nothing,
                    }))
                }
            };
            let placed = read_children(results(&resp));
            next.extend(placed.iter().map(Node::of));
            self.place(&placed, s).await?;
            match next_cursor(&resp) {
                Some(c) => cursor = Some(c),
                None => return Ok(Ok(next)),
            }
        }
    }

    /// The database `id` and the rows of each of its data sources.
    async fn rows(
        &self,
        id: &str,
        capped: &mut Vec<String>,
        s: &mut FetchSummary,
    ) -> Result<Result<Vec<Node>, Unlisted>> {
        let db = match self.client.get_database(id).await {
            Ok(db) => db,
            Err(_) if self.stop().requested() => return Ok(Ok(Vec::new())),
            Err(e) => {
                return Ok(Err(match unlisted(e)? {
                    Unlisted::Hole(e) => Unlisted::Hole(format!("{id}: {e}")),
                    nothing => nothing,
                }))
            }
        };
        self.place(&[Placed::Other(TreeUpsert::from_database(id, &db))], s)
            .await?;
        let sources: Vec<String> = db
            .get("data_sources")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|d| d.get("id").and_then(Value::as_str).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let mut next: Vec<Node> = Vec::new();
        for source in sources {
            let mut cursor: Option<String> = None;
            let mut listed = 0usize;
            loop {
                let resp = match self
                    .client
                    .query_data_source(&source, cursor.as_deref())
                    .await
                {
                    Ok(resp) => resp,
                    Err(_) if self.stop().requested() => return Ok(Ok(next)),
                    Err(e) => match unlisted(e)? {
                        Unlisted::Nothing => break,
                        Unlisted::Hole(e) => {
                            return Ok(Err(Unlisted::Hole(format!("{id} ({source}): {e}"))))
                        }
                    },
                };
                let rows: Vec<PageUpsert> = results(&resp)
                    .iter()
                    .filter(|r| r.get("object").and_then(Value::as_str) == Some("page"))
                    .filter_map(PageUpsert::from_object)
                    .collect();
                listed += rows.len();
                next.extend(rows.iter().map(|r| Node::Blocks(r.id.clone())));
                let mut tx = self.db.pool().begin().await?;
                self.db.store_listed_pages(&mut tx, &rows).await?;
                tx.commit().await?;
                self.wrote(rows.len() as u64).await;
                s.placed += rows.len();
                self.bar.doing(&format!("walk: {} pages placed", s.placed));
                match next_cursor(&resp) {
                    Some(c) => cursor = Some(c),
                    None => break,
                }
            }
            if listed >= QUERY_CAP {
                capped.push(format!("{id} ({listed} rows)"));
            }
        }
        Ok(Ok(next))
    }
}

fn results(resp: &Value) -> &[Value] {
    resp.get("results")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn next_cursor(resp: &Value) -> Option<String> {
    let more = resp.get("has_more").and_then(Value::as_bool) == Some(true);
    more.then(|| resp.get("next_cursor").and_then(Value::as_str))
        .flatten()
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::db::PlacedPage;
    use crate::ingest::schema_raw::TreeKind;
    use serde_json::json;

    fn block(id: &str, kind: &str, has_children: bool, parent: &str) -> Value {
        json!({
            "object": "block", "id": id, "type": kind, "has_children": has_children,
            "last_edited_time": "2026-09-01T00:00:00.000Z",
            "parent": {"type": "page_id", "page_id": parent},
            kind: {"title": format!("{id} title")},
        })
    }

    /// A child page is listed at its block's stamp; a database and a
    /// block that nests are placed and walked next; a paragraph, and a
    /// table whose children are cells, are neither.
    #[test]
    fn children_place_pages_databases_and_what_nests() {
        let results = [
            block("ops", "child_page", false, "bridge"),
            block("crew", "child_database", false, "bridge"),
            block("toggle", "toggle", true, "bridge"),
            block("para", "paragraph", false, "bridge"),
            block("grid", "table", true, "bridge"),
        ];
        let placed = read_children(&results);
        let ids: Vec<&str> = placed.iter().map(Placed::id).collect();
        assert_eq!(ids, ["ops", "crew", "toggle"]);
        assert_eq!(
            placed[0],
            Placed::Page(PlacedPage {
                id: "ops".into(),
                title: Some("ops title".into()),
                parent_type: Some("page_id".into()),
                parent_id: Some("bridge".into()),
                created_time: None,
                last_edited_time: Some("2026-09-01T00:00:00.000Z".into()),
            })
        );
        assert!(matches!(&placed[1], Placed::Other(t) if t.kind == TreeKind::Database));
        assert!(matches!(&placed[2], Placed::Other(t) if t.kind == TreeKind::Block));
        let next: Vec<Node> = placed.iter().map(Node::of).collect();
        assert_eq!(
            next,
            [
                Node::Blocks("ops".into()),
                Node::Database("crew".into()),
                Node::Blocks("toggle".into()),
            ]
        );
    }

    #[test]
    fn a_listing_pages_on_only_while_it_says_there_is_more() {
        assert_eq!(
            next_cursor(&json!({"has_more": true, "next_cursor": "c2"})).as_deref(),
            Some("c2")
        );
        assert_eq!(
            next_cursor(&json!({"has_more": false, "next_cursor": "c2"})),
            None
        );
        assert_eq!(
            next_cursor(&json!({"has_more": true, "next_cursor": null})),
            None
        );
    }
}
