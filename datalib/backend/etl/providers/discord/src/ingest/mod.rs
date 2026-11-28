//! Discord downloader: mirror the conversations the signed-in account can
//! read (the channels, threads and forum posts of the servers the config
//! names, and the account's direct messages) into raw JSON tables.
//!
//! A run lists first: the account, its servers, each server's channels,
//! their threads, the direct messages. Every listing names each
//! conversation's newest message, so one with nothing new costs no
//! request. Then it walks what is owed newest first across every
//! conversation, a page at a time, each page stored with the stretch of
//! ids it shows read whole, and fetches each page's files. INGEST.md has
//! the full description.

pub mod api;
pub mod db;
pub mod files;
pub mod schema_raw;
pub mod select;
pub mod snowflake;
pub mod visible;
pub mod walk;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::Result;
use datalib_etl::download_run::DownloadRun;
use datalib_etl::raw_store::{Batch, Sealer};
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl_discord_config::{
    DiscordSync, DEFAULT_LIST_EVERY_HOURS, DEFAULT_REFRESH_EVERY_MINUTES,
    DEFAULT_REFRESH_WINDOW_DAYS,
};
use datalib_etl_web::coverage::{self, Span};
use datalib_etl_web::http::{HttpRequest, LatchkeySettings};
use datalib_etl_web::owed;
use datalib_etl_web::owed_files::HeldFiles;
use datalib_etl_web::run_ended::{self, RunEnded};
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::Value;

pub use api::DiscordError;
pub use db::{db_path_for, RawDb};

use db::{id_of, Stamp};
use schema_raw::{
    history_scope, is_text, is_thread, is_thread_parent, is_webhook, message_key, threads_scope,
    ATTACHMENTS, MEMBERS,
};
use walk::Walk;

/// Files asked for at once. They come from Discord's file host, not its
/// API, whose requests go one at a time.
const FILES_AT_ONCE: usize = 3;
/// Bytes of files held in memory before they are written.
const FILE_FLUSH_BYTES: usize = 32 << 20;

#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub latchkey: LatchkeySettings,
    /// The store this run writes into, opened and closed by the caller
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    pub sync: DiscordSync,
    pub blob_size_limit_bytes: Option<u64>,
    /// The run's clock: what an offset in `since`, a refresh window and a
    /// sweep's age are counted back from.
    pub now: IsoOffsetTimestamp,
    pub progress: datalib_etl::progress::Progress,
    pub control: datalib_etl::control::DownloadControl,
    pub sealer: Option<Sealer>,
}

impl FetchOptions {
    pub fn new(db: RawDb, now: IsoOffsetTimestamp) -> Self {
        FetchOptions {
            db,
            now,
            latchkey: LatchkeySettings::default(),
            sync: DiscordSync::default(),
            blob_size_limit_bytes: None,
            progress: datalib_etl::progress::Progress::noop(),
            control: datalib_etl::control::DownloadControl::default(),
            sealer: None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct FetchSummary {
    /// Conversations listed and wanted.
    pub conversations: usize,
    /// Of those, the ones that owed anything.
    pub walked: usize,
    pub messages: usize,
    /// Messages deleted upstream, and so here.
    pub pruned: usize,
    pub files: usize,
    pub requests: u64,
}

impl FetchSummary {
    pub fn line(&self) -> String {
        format!(
            "conversations={} walked={} messages={} pruned={} files={} requests={}",
            self.conversations, self.walked, self.messages, self.pruned, self.files, self.requests
        )
    }
}

/// A conversation to walk: its id, and its newest message as listed.
struct Conversation {
    id: String,
    tip: u64,
}

fn tip_of(channel: &Value) -> Option<u64> {
    snowflake::parse(channel.get("last_message_id")?.as_str()?)
}

/// The records of a listing; an answer of any other shape says nothing
/// about what upstream holds, so it is never read as an empty list.
fn list_of(answer: Value) -> Result<Vec<Value>, String> {
    match answer {
        Value::Array(records) => Ok(records),
        _ => Err("the answer is not a list".to_string()),
    }
}

struct Ctx<'a> {
    opts: &'a FetchOptions,
    found: &'a RunProblems,
    /// No message older than this is asked for.
    floor: u64,
    requests: AtomicU64,
    downloaded: AtomicUsize,
    held_files: HeldFiles,
    /// Every read and write of the store goes through this until the run
    /// ends: it holds the store's one connection between seals.
    writes: Batch<'a>,
}

impl Ctx<'_> {
    fn stopped(&self) -> bool {
        self.opts.control.stop.requested()
    }

    /// The answer, or `None` where the run should do without it: it was
    /// told to stop, or this one thing failed and is in `problems` under
    /// `name`.
    async fn ask(&mut self, name: &str, req: HttpRequest) -> Result<Option<Value>> {
        if self.stopped() {
            return Ok(None);
        }
        self.requests.fetch_add(1, Ordering::Relaxed);
        match api::send(req, &self.opts.latchkey).await {
            Ok(answer) => Ok(Some(answer)),
            Err(_) if self.stopped() => Ok(None),
            Err(e) if e.ends_the_run() => Err(RunEnded::new(&e, e.upstream()).into()),
            Err(e) if e.retrying_changes_nothing() => {
                self.found.listing_cut_off(name, e.to_string());
                Ok(None)
            }
            Err(e) => {
                self.found.listing(name, e.to_string());
                Ok(None)
            }
        }
    }

    /// [`Self::ask`] for a listing: an answer that is not a list is a
    /// failed listing too.
    async fn ask_list(&mut self, name: &str, req: HttpRequest) -> Result<Option<Vec<Value>>> {
        let Some(answer) = self.ask(name, req).await? else {
            return Ok(None);
        };
        Ok(list_of(answer)
            .inspect_err(|e| self.found.listing(name, e.clone()))
            .ok())
    }

    /// Whether the listing `key` is due again.
    async fn due(&mut self, key: &str, every: chrono::Duration) -> Result<bool> {
        let age = db::SWEEPS
            .age(&mut **self.writes.tx().await?, key, &self.opts.now)
            .await?;
        Ok(age.is_none_or(|age| age >= every))
    }

    fn list_every(&self) -> chrono::Duration {
        chrono::Duration::hours(
            self.opts
                .sync
                .list_every_hours
                .unwrap_or(DEFAULT_LIST_EVERY_HOURS),
        )
    }

    /// Who the account is. Nothing else can be asked without it.
    async fn account(&mut self) -> Result<Option<String>> {
        let Some(me) = self.ask("account", api::me_request()).await? else {
            return Ok(None);
        };
        let Some(id) = id_of(&me).map(String::from) else {
            anyhow::bail!("Discord's /users/@me names no account");
        };
        let now = IsoOffsetTimestamp::now_local();
        let tx = self.writes.tx().await?;
        db::write_account(tx, &[(&id, &me)], &now).await?;
        db::delete_other_accounts(tx, &id).await?;
        db::write_users(tx, &[(&id, &me)], &now).await?;
        self.writes.wrote(2).await?;
        Ok(Some(id))
    }

    async fn dms(&mut self) -> Result<Vec<Conversation>> {
        if !self.opts.sync.dms {
            return Ok(Vec::new());
        }
        let Some(listed) = self.ask_list("direct messages", api::dms_request()).await? else {
            return Ok(Vec::new());
        };
        let (chosen, problems) = select::dms(&listed, self.opts.sync.dm_conversations.as_deref());
        self.found.config(problems);
        let now = IsoOffsetTimestamp::now_local();
        let channels: Vec<&Value> = chosen.iter().collect();
        let mut people: BTreeMap<&str, &Value> = BTreeMap::new();
        for person in channels
            .iter()
            .filter_map(|c| c.get("recipients")?.as_array())
            .flatten()
        {
            people.extend(id_of(person).map(|id| (id, person)));
        }
        let people: Vec<(&str, &Value)> = people.into_iter().collect();
        let tx = self.writes.tx().await?;
        db::write_channels(tx, &channels, &now).await?;
        db::write_users(tx, &people, &now).await?;
        self.writes
            .wrote((channels.len() + people.len()) as u64)
            .await?;
        Ok(conversations(channels))
    }

    /// The channels of the servers the config names that hold messages or
    /// threads and that the account can read.
    async fn server_channels(&mut self, me: &str) -> Result<Vec<Value>> {
        if !self.opts.sync.wants_servers() {
            return Ok(Vec::new());
        }
        let Some(listed) = self.ask_list("servers", api::guilds_request()).await? else {
            return Ok(Vec::new());
        };
        let (servers, problems) = select::servers(
            &listed,
            self.opts.sync.servers.as_deref(),
            self.opts.sync.all_servers,
        );
        self.found.config(problems);
        let now = IsoOffsetTimestamp::now_local();
        let rows: Vec<(&str, &Value)> = servers
            .iter()
            .filter_map(|g| Some((id_of(g)?, g)))
            .collect();
        db::write_guilds(self.writes.tx().await?, &rows, &now).await?;
        self.writes.wrote(rows.len() as u64).await?;

        let mut every_listed: Vec<Value> = Vec::new();
        let mut readable: Vec<Value> = Vec::new();
        let mut whole = true;
        for (guild_id, guild) in rows {
            let name = format!("channels of server {guild_id}");
            let Some(listed) = self
                .ask_list(&name, api::channels_request(guild_id))
                .await?
            else {
                whole = false;
                continue;
            };
            let Some(member) = self.member(me, guild_id).await? else {
                whole = false;
                continue;
            };
            let refs: Vec<&Value> = listed.iter().collect();
            db::write_channels(self.writes.tx().await?, &refs, &now).await?;
            self.writes.wrote(refs.len() as u64).await?;
            let owner = guild.get("owner").and_then(Value::as_bool) == Some(true);
            readable.extend(
                select::channels(&listed, self.opts.sync.channels.as_deref())
                    .into_iter()
                    .filter(|c| member.can_read(c, owner))
                    .cloned(),
            );
            every_listed.extend(listed);
        }
        if whole {
            self.found.config(select::unknown_channels(
                &every_listed,
                self.opts.sync.channels.as_deref(),
            ));
        }
        Ok(readable)
    }

    /// The account in one server, its roles listed again when that is due.
    async fn member(&mut self, me: &str, guild_id: &str) -> Result<Option<visible::Member>> {
        let key = format!("roles:{guild_id}");
        if self.due(&key, self.list_every()).await? {
            let name = format!("roles of server {guild_id}");
            let roles = self.ask_list(&name, api::roles_request(guild_id)).await?;
            let member = self.ask(&name, api::member_request(guild_id)).await?;
            let (Some(roles), Some(member)) = (roles, member) else {
                return Ok(None);
            };
            let now = IsoOffsetTimestamp::now_local();
            let run_now = self.opts.now.clone();
            let tx = self.writes.tx().await?;
            db::write_roles(tx, guild_id, &roles, &now).await?;
            db::write_member(tx, &[(guild_id, &member)], &now).await?;
            db::SWEEPS.record(&mut **tx, &key, &run_now).await?;
            self.writes.wrote(roles.len() as u64 + 1).await?;
        }
        let tx = self.writes.tx().await?;
        let roles = db::stored_roles(tx, guild_id).await?;
        let member = db::stored_payload(tx, MEMBERS, guild_id).await?;
        Ok(member.map(|m| visible::Member::new(me, guild_id, &m, &roles)))
    }

    /// Lists the threads of each of `parents` whose listing is due, newest
    /// message first, down to where an earlier listing reached.
    async fn list_threads(&mut self, parents: &[&Value]) -> Result<()> {
        for parent in parents {
            let Some(parent) = id_of(parent) else {
                continue;
            };
            let key = format!("threads:{parent}");
            if !self.due(&key, self.list_every()).await? {
                continue;
            }
            if self.threads_of(parent).await? {
                let run_now = self.opts.now.clone();
                db::SWEEPS
                    .record(&mut **self.writes.tx().await?, &key, &run_now)
                    .await?;
            }
        }
        Ok(())
    }

    /// Whether the listing got to its end.
    async fn threads_of(&mut self, parent: &str) -> Result<bool> {
        let scope = threads_scope(parent);
        let held = coverage::held(&mut **self.writes.tx().await?, &scope).await?;
        let name = format!("threads of {parent}");
        let mut offset = 0;
        let mut top: Option<u64> = None;
        loop {
            let Some(page) = self
                .ask(&name, api::threads_request(parent, offset))
                .await?
            else {
                return Ok(false);
            };
            let Some(threads) = page.get("threads").and_then(Value::as_array).cloned() else {
                self.found
                    .listing(&name, "the answer holds no list of threads");
                return Ok(false);
            };
            let more = page.get("has_more").and_then(Value::as_bool) == Some(true);
            let tips: Vec<u64> = threads.iter().filter_map(tip_of).collect();
            let top_here = top.or(tips.iter().copied().max());
            let oldest = tips.iter().copied().min();
            // The listing's last page, or one past `since`, covers it down
            // to the floor.
            let ends = !more || threads.is_empty() || oldest.is_some_and(|o| o < self.floor);
            let bottom = match oldest {
                Some(oldest) if !ends => oldest,
                _ => self.floor,
            };
            let now = IsoOffsetTimestamp::now_local();
            let refs: Vec<&Value> = threads.iter().collect();
            let tx = self.writes.tx().await?;
            db::write_channels(tx, &refs, &now).await?;
            if let Some(top) = top_here.filter(|t| *t >= bottom) {
                let span = Span::new(snowflake::key(bottom), snowflake::key(top));
                coverage::cover(tx, &scope, span).await?;
            }
            self.writes.wrote(refs.len() as u64).await?;
            let below = Span::new(snowflake::key(self.floor), snowflake::key(bottom));
            if ends || coverage::gaps(&below, &held).is_empty() {
                return Ok(true);
            }
            top = Some(bottom);
            offset += threads.len();
        }
    }

    /// The walk `conversation` owes, if any.
    async fn walk_of(&mut self, conversation: &Conversation) -> Result<Option<Walk>> {
        let id = &conversation.id;
        let held = coverage::held(&mut **self.writes.tx().await?, &history_scope(id)).await?;
        let window = self.window_start();
        let unread =
            window.and_then(|from| walk::unread_window(from, self.floor, conversation.tip, &held));
        let reread_from = match window {
            Some(from) if unread.is_none() && !held.is_empty() => {
                self.reread_from(id, from).await?
            }
            _ => None,
        };
        let stretches = walk::owed(self.floor, conversation.tip, &held, reread_from);
        Ok((!stretches.is_empty()).then(|| Walk::new(id, stretches, unread)))
    }

    /// Where the refresh window starts; `None` with the window off.
    fn window_start(&self) -> Option<u64> {
        let days = self.opts.sync.refresh_window_days;
        let days = days.unwrap_or(DEFAULT_REFRESH_WINDOW_DAYS);
        let window = chrono::Duration::days(days).num_milliseconds();
        (days > 0).then(|| snowflake::floor_at(self.opts.now.to_unix_millis() - window))
    }

    /// `from`, the window's start, when the conversation's re-read is due
    /// and there is anything in it to find changed.
    async fn reread_from(&mut self, id: &str, from: u64) -> Result<Option<u64>> {
        let every = self.opts.sync.refresh_every_minutes;
        let every = chrono::Duration::minutes(every.unwrap_or(DEFAULT_REFRESH_EVERY_MINUTES));
        if !db::holds_messages_since(self.writes.tx().await?, id, from).await? {
            return Ok(None);
        }
        let due = self.due(&refresh_key(id), every).await?;
        Ok(due.then_some(from))
    }

    async fn walk_all(
        &mut self,
        conversations: Vec<Conversation>,
        threads: bool,
        s: &mut FetchSummary,
    ) -> Result<()> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut walks: Vec<Walk> = Vec::new();
        for conversation in conversations {
            if !seen.insert(conversation.id.clone()) {
                continue;
            }
            s.conversations += 1;
            walks.extend(self.walk_of(&conversation).await?);
        }
        s.walked = walks.len();
        self.opts.progress.set_message("messages");
        // Newest first across every conversation, so a run cut off part
        // way has the newest of the account rather than the whole of
        // whichever conversations came first.
        while let Some(i) = (0..walks.len())
            .filter(|i| walks[*i].next_key().is_some())
            .max_by_key(|i| (walks[*i].next_key(), std::cmp::Reverse(*i)))
        {
            if self.stopped() {
                return Ok(());
            }
            let id = walks[i].channel_id.clone();
            let req = api::messages_request(&id, walks[i].next_before());
            let Some(page) = self.ask_list(&format!("messages of {id}"), req).await? else {
                // The next run owes what this one did not read.
                walks.swap_remove(i);
                continue;
            };
            let started = self.store_page(&mut walks[i], page, s).await?;
            if threads {
                for thread in started {
                    if seen.insert(thread.id.clone()) {
                        s.conversations += 1;
                        let walk = self.walk_of(&thread).await?;
                        s.walked += usize::from(walk.is_some());
                        walks.extend(walk);
                    }
                }
            }
        }
        Ok(())
    }

    /// One page of one conversation, with the stretch of ids it shows read
    /// whole; then its files. Returns the threads its messages started.
    async fn store_page(
        &mut self,
        walk: &mut Walk,
        page: Vec<Value>,
        s: &mut FetchSummary,
    ) -> Result<Vec<Conversation>> {
        let channel = walk.channel_id.clone();
        let ids: Vec<u64> = page
            .iter()
            .filter_map(|m| snowflake::parse(id_of(m)?))
            .collect();
        let Some(page_read) = walk.page(&ids) else {
            return Ok(Vec::new());
        };
        let kept: Vec<&Value> = page
            .iter()
            .filter(|m| {
                id_of(m)
                    .and_then(snowflake::parse)
                    .is_some_and(|id| id >= page_read.floor)
            })
            .collect();
        let people = people_in(&kept);
        let people: Vec<(&str, &Value)> = people.into_iter().collect();
        let threads: Vec<&Value> = kept
            .iter()
            .filter_map(|m| m.get("thread"))
            .filter(|t| is_thread(t))
            .collect();
        let now = IsoOffsetTimestamp::now_local();
        let run_now = self.opts.now.clone();
        let stamp = match page_read.reread {
            Some(_) => Stamp::FirstSeen,
            None => Stamp::Fetched,
        };
        let tx = self.writes.tx().await?;
        db::write_users(tx, &people, &now).await?;
        db::write_channels(tx, &threads, &now).await?;
        db::write_messages(tx, &channel, &kept, stamp, &now).await?;
        if let Some((lo, below)) = page_read.reread {
            s.pruned += db::prune_messages(tx, &channel, lo, below, &ids).await?;
        }
        coverage::cover(tx, &history_scope(&channel), page_read.span).await?;
        if page_read.refreshed {
            db::SWEEPS
                .record(&mut **tx, &refresh_key(&channel), &run_now)
                .await?;
        }
        s.messages += kept.len();
        self.opts.progress.inc(kept.len() as u64);
        self.writes.wrote(kept.len().max(1) as u64).await?;
        if self.opts.sync.media {
            let keys: Vec<String> = ids.iter().map(|id| message_key(&channel, *id)).collect();
            self.files(Some(&keys)).await?;
        }
        Ok(conversations(threads))
    }

    /// The owed files of `message_keys`, or of every stored message.
    async fn files(&mut self, message_keys: Option<&[String]>) -> Result<()> {
        if self.stopped() {
            return Ok(());
        }
        let owed_files = db::owed_files(self.writes.tx().await?, message_keys).await?;
        if owed_files.is_empty() {
            return Ok(());
        }
        // The newest message's first.
        let listed: Vec<owed::Listed> = owed_files
            .keys()
            .rev()
            .map(|k| owed::Listed::new(k.clone(), None::<String>))
            .collect();
        let fetcher = files::FileFetcher {
            db: &self.opts.db,
            latchkey: &self.opts.latchkey,
            size_limit: self.opts.blob_size_limit_bytes,
            now_secs: self.opts.now.to_unix_millis() / 1000,
            owed: owed_files,
            held: &self.held_files,
            fresh: Mutex::new(HashMap::new()),
            requests: &self.requests,
            downloaded: &self.downloaded,
        };
        let the_loop = owed::Loop {
            pool: self.opts.db.pool(),
            table: ATTACHMENTS,
            phase: "files",
            stop: &self.opts.control.stop,
            found: self.found,
            sealer: self.opts.sealer.as_ref(),
            batch: 1,
            concurrency: FILES_AT_ONCE,
            flush: 16,
            flush_bytes: FILE_FLUSH_BYTES,
            failures_in_a_row: 0,
        };
        owed::drain_into(&the_loop, &mut self.writes, listed, &fetcher).await?;
        Ok(())
    }
}

fn refresh_key(channel_id: &str) -> String {
    format!("refresh:{channel_id}")
}

fn conversations<'a>(channels: impl IntoIterator<Item = &'a Value>) -> Vec<Conversation> {
    channels
        .into_iter()
        .filter_map(|c| {
            Some(Conversation {
                id: id_of(c)?.to_string(),
                tip: tip_of(c)?,
            })
        })
        .collect()
}

/// Everyone `messages` name by their own profile.
fn people_in<'a>(messages: &[&'a Value]) -> BTreeMap<&'a str, &'a Value> {
    let mut out = BTreeMap::new();
    for m in messages {
        let author = m.get("author").filter(|_| !is_webhook(m));
        let asked = ["interaction", "interaction_metadata"]
            .into_iter()
            .filter_map(|k| m.get(k)?.get("user"));
        let mentioned = m.get("mentions").and_then(Value::as_array);
        for person in author
            .into_iter()
            .chain(asked)
            .chain(mentioned.into_iter().flatten())
        {
            out.extend(id_of(person).map(|id| (id, person)));
        }
    }
    out
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    let sealer = opts.sealer.clone();
    run_problems::collecting_sealed(&pool, &stop, sealer.as_ref(), |found| download(opts, found))
        .await
}

async fn download(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let _ = datalib_etl_web::latchkey::ensure_curl_router();
    let db = opts.db.clone();
    let run = DownloadRun::start(&found, db.pool(), &serde_json::to_value(&opts.sync)?).await?;
    let floor = match &opts.sync.since {
        Some(since) => {
            let at = datalib_time::TimeBound::absolute(since, &opts.now)?;
            snowflake::floor_at(datalib_time::parse_with_assumed_utc(&at)?.to_unix_millis())
        }
        None => 0,
    };
    let mut ctx = Ctx {
        opts: &opts,
        found: &found,
        floor,
        requests: AtomicU64::new(0),
        downloaded: AtomicUsize::new(0),
        held_files: Mutex::new(HashMap::new()),
        writes: Batch::new(db.pool(), opts.sealer.as_ref()),
    };
    let mut s = FetchSummary::default();
    let result = run_phases(&mut ctx, &mut s).await;
    let result = run_ended::record_run_ended(result, "Discord", s.conversations > 0, &found);
    s.requests = ctx.requests.load(Ordering::Relaxed);
    s.files = ctx.downloaded.load(Ordering::Relaxed);
    let result = ctx.writes.finish_or_roll_back(result).await;
    run.summarize(&s);
    result.map(|()| s)
}

async fn run_phases(ctx: &mut Ctx<'_>, s: &mut FetchSummary) -> Result<()> {
    ctx.opts.progress.set_message("listing");
    let Some(me) = ctx.account().await? else {
        if ctx.stopped() {
            return Ok(());
        }
        anyhow::bail!("Discord did not say who the account is, so nothing was fetched");
    };
    *ctx.held_files.lock().unwrap() = db::held_files(ctx.writes.tx().await?).await?;
    let mut wanted = ctx.dms().await?;
    let channels = ctx.server_channels(&me).await?;
    wanted.extend(conversations(channels.iter().filter(|c| is_text(c))));
    let threads = ctx.opts.sync.threads;
    if threads {
        let parents: Vec<&Value> = channels.iter().filter(|c| is_thread_parent(c)).collect();
        ctx.list_threads(&parents).await?;
        // Every stored thread, so one listed by a run cut off before it
        // was read is owed too.
        let parent_ids: Vec<&str> = parents.iter().filter_map(|p| id_of(p)).collect();
        let stored = db::stored_threads(ctx.writes.tx().await?, &parent_ids).await?;
        wanted.extend(
            stored
                .into_iter()
                .filter_map(|(id, tip)| Some(Conversation { id, tip: tip? })),
        );
    }
    ctx.walk_all(wanted, threads, s).await?;
    if ctx.opts.sync.media {
        // What an earlier run left: stopped before its files, failed, or
        // ran with `media` off.
        ctx.files(None).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_webhooks_author_is_nobody() {
        let m = json!({"author": {"id": "5", "username": "Ship's computer"}, "webhook_id": "5",
                       "mentions": [{"id": "1701"}]});
        let people = people_in(&[&m]);
        assert_eq!(people.keys().copied().collect::<Vec<_>>(), ["1701"]);
    }

    #[test]
    fn a_channel_nobody_wrote_in_is_no_conversation() {
        let empty = json!({"id": "1", "last_message_id": null});
        let live = json!({"id": "2", "last_message_id": "17"});
        let got = conversations([&empty, &live]);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].id.as_str(), got[0].tip), ("2", 17));
    }
}
