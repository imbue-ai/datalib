//! The render wave for the `facebook` source: one read of the raw store,
//! then five feeds — posts, albums, comments, reactions, friends — each
//! rendered through the shared chat or contact renderer.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::processor::PlanContext;
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::render::render_all as chat_render_all;
use datalib_etl_chat_common::types::NormalizedChat;
use datalib_etl_contact_common::{render_all as contact_render_all, NormalizedContact};
use datalib_etl_facebook::ingest::schema_raw::{
    ALBUMS_TABLE, COMMENTS_TABLE, FRIENDS_TABLE, OTHER_POSTS_TABLE, POSTS_TABLE, PROFILE_TABLE,
    REACTIONS_TABLE,
};
use datalib_etl_facebook::ingest::{db_path_for, RawDb};
use datalib_etl_facebook_config::FacebookRenderConfig;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{changed_rows, Bucket, Buckets, Input, RawRange};
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
use serde_json::Value;

use crate::activity::{build_comments, build_reactions, comments_profile, reactions_profile};
use crate::albums::{albums_profile, build_albums};
use crate::common::{str_field, RENDER_VERSION};
use crate::friends::{build_friends, friends_profile};
use crate::posts::{build_posts, posts_profile};

pub fn plan_render(
    ctx: PlanContext,
    config: FacebookRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(FacebookRender {
        id: format!("facebook/{name}/render"),
        raw_path,
        name,
    })])
}

/// Whose export this is: the name every item is written under, the
/// `account` label every row carries, and the profile rows both came
/// from, which every document therefore declares.
#[derive(Debug, Clone, Default)]
pub struct Owner {
    pub name: String,
    pub account: Option<String>,
    pub inputs: Vec<Input>,
}

impl Owner {
    /// From the `profile_v2` record: the first listed email is the
    /// account, the full name is the author. An export always has one;
    /// a store without it falls back to "Me".
    pub fn from_profile(rows: &[(String, Value)]) -> Owner {
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
     FROM pinned_media_blobs \
     WHERE uri IN ({placeholders}) AND blake3 IS NOT NULL";

const ALL_TABLES: &[&str] = &[
    POSTS_TABLE,
    OTHER_POSTS_TABLE,
    ALBUMS_TABLE,
    COMMENTS_TABLE,
    REACTIONS_TABLE,
    FRIENDS_TABLE,
    PROFILE_TABLE,
];

/// Everything one pass reads off the store, built while it is open.
struct Loaded {
    owner: Owner,
    posts: Vec<NormalizedChat>,
    albums: Vec<NormalizedChat>,
    comments: Vec<NormalizedChat>,
    reactions: Vec<NormalizedChat>,
    friends: Vec<NormalizedContact>,
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
            // A table the export did not carry does not exist; that is
            // "no rows", not a failed render.
            let mut tables: HashMap<&str, Vec<(String, Value)>> = HashMap::new();
            for table in ALL_TABLES {
                let rows = datalib_etl::doltlite_raw::load_payloads_with_id(
                    db.pool(),
                    datalib_etl::pin::Reads::At(&pin),
                    table,
                )
                .await
                .unwrap_or_default();
                tables.insert(table, rows);
            }
            let changed = changed_rows(db.pool(), range, &pin, ALL_TABLES).await?;
            let rows = |t: &str| tables.get(t).map(Vec::as_slice).unwrap_or(&[]);

            let owner = Owner::from_profile(rows(PROFILE_TABLE));
            let mut posts = build_posts(rows(POSTS_TABLE), rows(OTHER_POSTS_TABLE), &owner);
            let mut albums = build_albums(rows(ALBUMS_TABLE), &owner);
            let mut comments = build_comments(rows(COMMENTS_TABLE), &owner);
            let mut reactions = build_reactions(rows(REACTIONS_TABLE), &owner);
            let mut friends = build_friends(rows(FRIENDS_TABLE), &owner);

            let mut buckets = Buckets::new();
            for chats in [&mut posts, &mut albums, &mut comments, &mut reactions] {
                buckets.extend(narrow_chats(chats, changed.as_ref(), range));
            }
            buckets.extend(narrow_contacts(&mut friends, changed.as_ref(), range));

            let mut blobs = HashMap::new();
            if let Some(cas) = db.cas() {
                for chat in posts.iter().chain(&albums).chain(&comments) {
                    let refs = attachment_refs(chat);
                    if refs.is_empty() {
                        continue;
                    }
                    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
                    let bundle = BlobBundle::load(db.pool(), cas.pool(), MEDIA_PROJECTION, &refs)
                        .await
                        .with_context(|| format!("load media for {}", chat.id))?;
                    blobs.insert(chat.id.clone(), bundle);
                }
            }
            let head = pin.commit().to_string();
            db.close().await;
            Ok::<_, anyhow::Error>(Some(Loaded {
                owner,
                posts,
                albums,
                comments,
                reactions,
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
fn narrow_chats(
    chats: &mut Vec<NormalizedChat>,
    changed: Option<&HashMap<String, HashSet<String>>>,
    range: RawRange<'_>,
) -> Buckets {
    let forward = changed.map(|changed| {
        chats
            .iter()
            .filter(|c| touched(&c.inputs, changed))
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
    contacts: &mut Vec<NormalizedContact>,
    changed: Option<&HashMap<String, HashSet<String>>>,
    range: RawRange<'_>,
) -> Buckets {
    let forward = changed.map(|changed| {
        contacts
            .iter()
            .filter(|c| touched(&c.inputs, changed))
            .map(|c| c.contact_uuid.clone())
            .collect::<HashSet<String>>()
    });
    let render = range.narrow(forward.as_ref());
    if let Some(render) = &render {
        contacts.retain(|c| render.contains(&c.contact_uuid));
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

struct FacebookRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl RenderProcessor for FacebookRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(RENDER_VERSION)
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        let mut on_doc = |md| ctx.emit_doc(md);
        let source = Source {
            raw_dir: &self.raw_path,
            out_dir: ctx.root,
            name: &self.name,
            range: ctx.raw_range(),
        };
        let outcome =
            render_source(&source, ctx.progress, &mut on_doc).context("facebook render")?;
        // Every bucket looked at, named first with nothing and then, for
        // the rendered ones, with what they read — in that order, so a
        // bucket no feed rendered ends declared with nothing and its
        // documents go.
        for bucket in &outcome.buckets {
            ctx.declare_bucket(&bucket.key, &bucket.inputs)?;
        }
        if let Some(head) = outcome.new_head.as_deref() {
            ctx.consumed(head);
        }
        Ok("rendered".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn owner_is_the_email_then_the_name() {
        let rows = vec![(
            "p1".to_string(),
            json!({"profile_v2": {
                "name": {"full_name": "Jean-Luc Picard"},
                "emails": {"emails": ["picard@enterprise.starfleet"]},
            }}),
        )];
        let owner = Owner::from_profile(&rows);
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
            Owner::from_profile(&no_email).account.as_deref(),
            Some("Data")
        );
        let none = Owner::from_profile(&[]);
        assert_eq!(none.name, "Me");
        assert_eq!(none.account, None);
    }
}
