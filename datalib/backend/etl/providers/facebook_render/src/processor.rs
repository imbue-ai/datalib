//! The render wave for the `facebook` source: one read of the raw store,
//! then six feeds — posts, albums, comments, reactions, Messenger,
//! friends — each rendered through the shared chat or contact renderer.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::processor::PlanContext;
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::render::render_all as chat_render_all;
use datalib_etl_chat_common::types::NormalizedChat;
use datalib_etl_contact_common::{render_all as contact_render_all, ContactDoc};
use datalib_etl_facebook::ingest::schema_raw::{
    ALBUMS_TABLE, COMMENTS_TABLE, COMMENT_EDITS_TABLE, FRIENDS_TABLE, GROUPS_JOINED_TABLE,
    GROUP_COMMENTS_TABLE, GROUP_POSTS_TABLE, MESSENGER_MESSAGES_TABLE, MESSENGER_THREADS_TABLE,
    OTHER_POSTS_TABLE, POSTS_TABLE, POST_EDITS_TABLE, PROFILE_TABLE, REACTIONS_TABLE,
};
use datalib_etl_facebook::ingest::{db_path_for, RawDb};
use datalib_etl_facebook_config::FacebookRenderConfig;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{changed_rows, Bucket, Buckets, Input, RawRange};
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use serde_json::Value;

use crate::activity::{build_comments, build_reactions, comments_profile, reactions_profile};
use crate::albums::{albums_profile, build_albums};
use crate::common::{str_field, RENDER_VERSION};
use crate::friends::{build_friends, friends_profile};
use crate::messenger::{build_conversations, messenger_profile};
use crate::posts::{build_posts, posts_profile, PostRows};

pub fn plan_render(
    ctx: PlanContext,
    config: FacebookRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        FacebookRender,
    ))
}

/// Whose export this is: the configured source it renders under, the
/// name every item is written under, the `account` label every row
/// carries, and the profile rows both came from, which every document
/// therefore declares.
#[derive(Debug, Clone, Default)]
pub struct Owner {
    /// The source's group id — a component of every id minted here.
    pub source_id: String,
    pub name: String,
    pub account: Option<String>,
    pub inputs: Vec<Input>,
}

impl Owner {
    /// From the `profile_v2` record: the first listed email is the
    /// account, the full name is the author. An export always has one;
    /// a store without it falls back to "Me".
    pub fn from_profile(source_id: &str, rows: &[(String, Value)]) -> Owner {
        let inputs = rows
            .iter()
            .map(|(id, _)| Input::new(PROFILE_TABLE, id))
            .collect();
        let profile = rows.first().and_then(|(_, v)| v.get("profile_v2"));
        let name = profile
            .and_then(|p| p.get("name"))
            .and_then(|n| str_field(n, "full_name"))
            .map(str::to_string);
        let email = profile
            .and_then(|p| p.pointer("/emails/emails/0"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        Owner {
            source_id: source_id.to_string(),
            name: name.clone().unwrap_or_else(|| "Me".to_string()),
            account: datalib_etl_chat_common::account_label("", email, name.as_deref()),
            inputs,
        }
    }
}

/// What every feed's render needs to know about the source it is
/// rendering.
pub struct Source<'a> {
    pub raw_dir: &'a Path,
    pub out_dir: &'a Path,
    pub name: &'a str,
    pub range: RawRange<'a>,
}

/// What one render pass did: every bucket it looked at, and the commit
/// it read.
#[derive(Debug, Default)]
pub struct Outcome {
    pub buckets: Buckets,
    pub new_head: Option<String>,
}

/// The bundle key `render_all` looks a chat's blobs up under.
const MEDIA_PROJECTION: &str = "SELECT DISTINCT uri AS ref_id, blake3, \
            NULL AS content_type, uri AS upstream_name \
     FROM media_blobs \
     WHERE uri IN ({placeholders}) AND blake3 IS NOT NULL";

const ALL_TABLES: &[&str] = &[
    POSTS_TABLE,
    OTHER_POSTS_TABLE,
    ALBUMS_TABLE,
    COMMENTS_TABLE,
    REACTIONS_TABLE,
    FRIENDS_TABLE,
    PROFILE_TABLE,
    MESSENGER_THREADS_TABLE,
    MESSENGER_MESSAGES_TABLE,
    POST_EDITS_TABLE,
    COMMENT_EDITS_TABLE,
    GROUP_POSTS_TABLE,
    GROUP_COMMENTS_TABLE,
    GROUPS_JOINED_TABLE,
];

/// Everything one pass reads off the store, built while it is open.
struct Loaded {
    owner: Owner,
    posts: Vec<NormalizedChat>,
    albums: Vec<NormalizedChat>,
    comments: Vec<NormalizedChat>,
    reactions: Vec<NormalizedChat>,
    conversations: Vec<NormalizedChat>,
    friends: Vec<ContactDoc>,
    /// Per chat id, the media bytes its attachments reference.
    blobs: HashMap<String, BlobBundle>,
    buckets: Buckets,
    head: String,
}

pub fn render_source(
    source: &Source<'_>,
    progress: &Progress,
    on_doc: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<Outcome> {
    let db_path = db_path_for(source.raw_dir);
    if !db_path.exists() {
        return Ok(Outcome::default());
    }
    let range = source.range;
    // One open for every table and every blob: reopening a doltlite store
    // while the last connection is still closing is what makes a later
    // `dolt_commit` fail.
    let loaded = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            // Read at a commit: this store belongs to the download step, and
            // nothing committed means nothing to render from.
            let Some(db) = RawDb::open_reader(&db_path, range.pin).await? else {
                return Ok(None);
            };
            let pin = db.pin().expect("a reader is pinned at open").clone();
            let mut tables: HashMap<&str, Vec<(String, Value)>> = HashMap::new();
            for table in ALL_TABLES {
                let rows =
                    datalib_etl::doltlite_raw::load_payloads_with_id_if_present(db.pool(), table)
                        .await
                        .with_context(|| format!("load {table}"))?;
                tables.insert(table, rows);
            }
            let changed = changed_rows(db.pool(), range, &pin, ALL_TABLES).await?;
            let rows = |t: &str| tables.get(t).map(Vec::as_slice).unwrap_or(&[]);

            let owner = Owner::from_profile(source.name, rows(PROFILE_TABLE));
            let mut posts = build_posts(
                &PostRows {
                    posts: rows(POSTS_TABLE),
                    other_posts: rows(OTHER_POSTS_TABLE),
                    group_posts: rows(GROUP_POSTS_TABLE),
                    edits: rows(POST_EDITS_TABLE),
                    groups_joined: rows(GROUPS_JOINED_TABLE),
                },
                &owner,
            );
            let mut albums = build_albums(rows(ALBUMS_TABLE), &owner);
            let mut comments = build_comments(
                rows(COMMENTS_TABLE),
                rows(GROUP_COMMENTS_TABLE),
                rows(COMMENT_EDITS_TABLE),
                &owner,
            );
            let mut reactions = build_reactions(rows(REACTIONS_TABLE), &owner);
            let mut conversations = build_conversations(
                rows(MESSENGER_THREADS_TABLE),
                rows(MESSENGER_MESSAGES_TABLE),
                &owner,
            );
            let mut friends = build_friends(rows(FRIENDS_TABLE), &owner);

            let mut buckets = Buckets::new();
            // Which post an edit is of is decided over every post and every
            // edit, so where there are edits, a change to any of them can
            // move a version — to another post, or to a document of its
            // own no row of which changed — and every post renders again.
            let edited = !rows(POST_EDITS_TABLE).is_empty()
                || changed
                    .as_ref()
                    .is_some_and(|c| c.contains_key(POST_EDITS_TABLE));
            let as_one: &[&str] = if edited {
                &[
                    POSTS_TABLE,
                    OTHER_POSTS_TABLE,
                    GROUP_POSTS_TABLE,
                    POST_EDITS_TABLE,
                ]
            } else {
                &[]
            };
            buckets.extend(narrow_chats(&mut posts, changed.as_ref(), range, as_one));
            for chats in [
                &mut albums,
                &mut comments,
                &mut reactions,
                &mut conversations,
            ] {
                buckets.extend(narrow_chats(chats, changed.as_ref(), range, &[]));
            }
            buckets.extend(narrow_contacts(&mut friends, changed.as_ref(), range));

            let mut blobs = HashMap::new();
            if let Some(cas) = db.cas() {
                let refs = posts
                    .iter()
                    .chain(&albums)
                    .chain(&comments)
                    .chain(&conversations)
                    .map(|chat| (chat.id.clone(), attachment_refs(chat)));
                blobs = BlobBundle::load_many(db.pool(), Some(cas.pool()), MEDIA_PROJECTION, refs)
                    .await
                    .context("load media")?;
            }
            let head = pin.commit().to_string();
            db.close().await;
            Ok::<_, anyhow::Error>(Some(Loaded {
                owner,
                posts,
                albums,
                comments,
                reactions,
                conversations,
                friends,
                blobs,
                buckets,
                head,
            }))
        })
    })?;
    let Some(loaded) = loaded else {
        return Ok(Outcome::default());
    };

    let mut outcome = Outcome {
        buckets: loaded.buckets,
        new_head: Some(loaded.head),
    };
    let no_blobs = HashMap::new();
    for (profile, chats, blobs) in [
        (posts_profile(), &loaded.posts, &loaded.blobs),
        (albums_profile(), &loaded.albums, &loaded.blobs),
        (comments_profile(), &loaded.comments, &loaded.blobs),
        (reactions_profile(), &loaded.reactions, &no_blobs),
        (messenger_profile(), &loaded.conversations, &loaded.blobs),
    ] {
        let s = chat_render_all(
            &profile,
            chats,
            source.out_dir,
            source.name,
            blobs,
            progress,
            on_doc,
        )
        .with_context(|| format!("render {}", profile.chat_kind))?;
        outcome.buckets.extend(s.buckets);
    }
    let s = contact_render_all(
        &friends_profile(&loaded.owner),
        &loaded.friends,
        source.out_dir,
        source.name,
        progress,
        on_doc,
    )
    .context("render friends")?;
    outcome.buckets.extend(s.buckets);
    Ok(outcome)
}

/// Keep the chats this run has to render — the driver's stale set plus
/// every chat a changed row maps to — and name each of those keys with
/// nothing first, so a stale chat whose rows are gone loses its
/// documents. `None` from either side renders everything.
/// A change to a row of any `as_one` table renders every chat again:
/// those tables decide something across all of them.
fn narrow_chats(
    chats: &mut Vec<NormalizedChat>,
    changed: Option<&HashMap<String, HashSet<String>>>,
    range: RawRange<'_>,
    as_one: &[&str],
) -> Buckets {
    let forward = changed.map(|changed| {
        let all = as_one
            .iter()
            .any(|t| changed.get(*t).is_some_and(|ids| !ids.is_empty()));
        chats
            .iter()
            .filter(|c| all || touched(&c.inputs, changed))
            .map(|c| c.chat_uuid.clone())
            .collect::<HashSet<String>>()
    });
    let render = range.narrow(forward.as_ref());
    if let Some(render) = &render {
        chats.retain(|c| render.contains(&c.chat_uuid));
    }
    empty_buckets(render)
}

fn narrow_contacts(
    contacts: &mut Vec<ContactDoc>,
    changed: Option<&HashMap<String, HashSet<String>>>,
    range: RawRange<'_>,
) -> Buckets {
    let forward = changed.map(|changed| {
        contacts
            .iter()
            .filter(|c| touched(&c.inputs, changed))
            .map(|c| c.doc_uuid.clone())
            .collect::<HashSet<String>>()
    });
    let render = range.narrow(forward.as_ref());
    if let Some(render) = &render {
        contacts.retain(|c| render.contains(&c.doc_uuid));
    }
    empty_buckets(render)
}

fn touched(inputs: &[Input], changed: &HashMap<String, HashSet<String>>) -> bool {
    inputs
        .iter()
        .any(|i| changed.get(&i.table).is_some_and(|ids| ids.contains(&i.id)))
}

fn empty_buckets(render: Option<HashSet<String>>) -> Buckets {
    let mut keys: Vec<String> = render.into_iter().flatten().collect();
    keys.sort();
    keys.into_iter()
        .map(|key| Bucket {
            key,
            inputs: Vec::new(),
        })
        .collect()
}

fn attachment_refs(chat: &NormalizedChat) -> Vec<String> {
    let mut refs: Vec<String> = chat
        .buckets
        .iter()
        .flat_map(|b| &b.items)
        .flat_map(|i| &i.attachments)
        .filter_map(|a| a.ref_id.clone())
        .collect();
    refs.sort();
    refs.dedup();
    refs
}

struct FacebookRender;

#[async_trait]
impl SourceRender for FacebookRender {
    const PROVIDER: &'static str = "facebook";

    fn render_version(&self) -> u32 {
        RENDER_VERSION
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        let mut on_doc = |md| ctx.emit_doc(md);
        let source = Source {
            raw_dir: raw_path,
            out_dir: ctx.root,
            name: ctx.name,
            range: ctx.raw_range(),
        };
        let outcome =
            render_source(&source, ctx.progress, &mut on_doc).context("facebook render")?;
        // Every bucket looked at, named first with nothing and then, for
        // the rendered ones, with what they read — in that order, so a
        // bucket no feed rendered ends declared with nothing and its
        // documents go.
        ctx.finish(&outcome.buckets, outcome.new_head.as_deref())?;
        Ok("rendered".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render(raw: &Path) -> Result<Outcome> {
        let source = Source {
            raw_dir: raw,
            out_dir: raw,
            name: "facebook",
            range: RawRange {
                cursor: None,
                pin: None,
                stale: None,
            },
        };
        render_source(&source, &Progress::noop(), &mut |_| Ok(()))
    }

    /// A table that failed to load read as one the export did not carry,
    /// so its documents were rendered from nothing and the render's sweep
    /// deleted them, with the step reporting success. Only a table that
    /// does not exist is "no rows".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_table_that_will_not_load_fails_the_render() {
        let d = tempfile::tempdir().unwrap();
        let raw = d.path();
        let db = RawDb::open(&db_path_for(raw)).await.unwrap();
        datalib_etl::doltlite_raw::commit_run(db.pool(), "an export with no posts")
            .await
            .unwrap();
        db.close().await;
        assert!(
            render(raw).is_ok(),
            "a table the export did not carry is no rows"
        );

        let db = RawDb::open(&db_path_for(raw)).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TABLE {POSTS_TABLE} (id TEXT PRIMARY KEY)"
        )))
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {POSTS_TABLE} (id) VALUES ('p1')"
        )))
        .execute(db.pool())
        .await
        .unwrap();
        datalib_etl::doltlite_raw::commit_run(db.pool(), "a posts table it cannot read")
            .await
            .unwrap();
        db.close().await;
        assert!(render(raw).is_err());
    }

    #[test]
    fn owner_is_the_email_then_the_name() {
        let rows = vec![(
            "p1".to_string(),
            json!({"profile_v2": {
                "name": {"full_name": "Jean-Luc Picard"},
                "emails": {"emails": ["picard@enterprise.starfleet"]},
            }}),
        )];
        let owner = Owner::from_profile("fb", &rows);
        assert_eq!(owner.name, "Jean-Luc Picard");
        assert_eq!(
            owner.account.as_deref(),
            Some("picard@enterprise.starfleet")
        );
        assert_eq!(owner.inputs.len(), 1);
        assert_eq!(owner.inputs[0].table, PROFILE_TABLE);

        let no_email = vec![(
            "p1".to_string(),
            json!({"profile_v2": {"name": {"full_name": "Data"}, "emails": {"emails": []}}}),
        )];
        assert_eq!(
            Owner::from_profile("fb", &no_email).account.as_deref(),
            Some("Data")
        );
        let none = Owner::from_profile("fb", &[]);
        assert_eq!(none.name, "Me");
        assert_eq!(none.account, None);
    }
}
