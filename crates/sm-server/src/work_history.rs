//! The history page's data (sm#1452, ticket #1488): one row per ticket sm
//! tracks (or per PR that links to no ticket), and one ticket's timeline.
//!
//! Everything is read from stored rows: `work_items`, `work_claims`,
//! `work_links` and `events` (ticket #1485), Codex review registrations, and
//! owner docs with their publishes and posted reviews. Review requests and
//! doc publishes are not copied into `events`; they are merged in here at
//! read time (one store per fact). Nothing here calls GitHub. See
//! `docs/working/1452_sm_primitives.html`, appendices H and I.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::Result;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::owner_docs::{
    doc_name, doc_readable_path, repo_name, OwnerDocStore, OwnerDocSummary, OwnerDocVerdict,
};
use crate::work_claims::{
    claim_from_row, history_path, item_from_row, HolderState, SessionDirectory, StoredEvent,
    WorkClaim, WorkItem, CLAIM_COLUMNS, ITEM_COLUMNS,
};

pub const HISTORY_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 200;
/// An open item sm is still tracking whose last fetch is older than this
/// is shown `stale`.
const STALE_AFTER_SECONDS: i64 = 60 * 60;

/// A row flag (appendix I). `id` is the JSON form, `label` the chip text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Flag {
    TwoAgents,
    OpenAfterMerge,
    NoLiveHolder,
    WorktreeLeft,
    Stale,
}

impl Flag {
    pub fn id(self) -> &'static str {
        match self {
            Self::TwoAgents => "two_agents",
            Self::OpenAfterMerge => "open_after_merge",
            Self::NoLiveHolder => "no_live_holder",
            Self::WorktreeLeft => "worktree_left",
            Self::Stale => "stale",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::TwoAgents => "2 agents",
            Self::OpenAfterMerge => "Open after merge",
            Self::NoLiveHolder => "No live holder",
            Self::WorktreeLeft => "Worktree left",
            Self::Stale => "stale",
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        [
            Self::TwoAgents,
            Self::OpenAfterMerge,
            Self::NoLiveHolder,
            Self::WorktreeLeft,
            Self::Stale,
        ]
        .into_iter()
        .find(|flag| flag.id() == id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexReview {
    /// Lowercase `owner/name`, as claims key repos.
    pub repo: String,
    pub pr: i64,
    pub requester_session_id: Option<String>,
    pub requested_at: String,
    pub landed_at: Option<String>,
    pub review_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocPublish {
    pub commit_sha: String,
    pub session_id: String,
    pub review_requested: bool,
    pub published_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedOwnerReview {
    pub verdict: String,
    pub submitted_at: String,
    pub url: Option<String>,
    pub delivered_to_session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HistoryDoc {
    pub summary: OwnerDocSummary,
    /// Lowercase `owner/name`.
    pub repo: String,
    pub publishes: Vec<DocPublish>,
    pub reviews: Vec<PostedOwnerReview>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RowAgent {
    pub session_id: String,
    pub name: String,
    /// `working | idle | stopped | retired`, derived from the session record.
    pub state: String,
    /// The session's earliest claim on the thread.
    pub claimed_at: String,
    /// `None` while any of its claims on the thread is active; otherwise its
    /// latest-ending claim's end.
    pub ended_at: Option<String>,
    pub end_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RowPr {
    pub number: i64,
    pub title: String,
    pub state: String,
    pub url: String,
    pub codex_requested: usize,
    pub codex_landed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RowDoc {
    pub id: String,
    pub name: String,
    pub title: String,
    pub state: String,
    pub reader_path: String,
    pub owner_reviews: usize,
    pub author_session_id: String,
    pub published_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryRow {
    pub repo: String,
    pub number: i64,
    pub kind: String,
    pub title: String,
    pub state: String,
    pub url: String,
    pub flags: Vec<&'static str>,
    pub agents: Vec<RowAgent>,
    pub prs: Vec<RowPr>,
    pub docs: Vec<RowDoc>,
    pub last_activity: Option<String>,
    pub history_path: String,
    /// PR items: the tickets it links to (empty on the list, where a linked
    /// PR appears under its tickets instead).
    pub linked_tickets: Vec<i64>,
    pub synced_at: Option<String>,
    pub sync_error: Option<String>,
    #[serde(skip)]
    pub sort_nanos: i128,
}

impl HistoryRow {
    pub fn has_flag(&self, flag: Flag) -> bool {
        self.flags.contains(&flag.id())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TimelineEntry {
    pub at: String,
    pub session_id: Option<String>,
    pub name: Option<String>,
    /// The event kind (`claim.taken`, …), or `codex_review.requested`,
    /// `codex_review.landed`, `doc.published`, `doc.reviewed` for the facts
    /// merged in from their own stores.
    pub kind: String,
    pub text: String,
    /// A GitHub URL or an sm path.
    pub link: Option<String>,
    #[serde(skip)]
    pub sort_nanos: i128,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Timeline {
    pub item: HistoryRow,
    pub events: Vec<TimelineEntry>,
}

/// `(last_activity, repo, number)`: rows sort descending on the tuple and
/// `before` pages to the tuples below it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cursor {
    pub nanos: i128,
    pub repo: String,
    pub number: i64,
}

impl Cursor {
    fn of(row: &HistoryRow) -> Self {
        Self {
            nanos: row.sort_nanos,
            repo: row.repo.clone(),
            number: row.number,
        }
    }

    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(format!("{}|{}|{}", self.nanos, self.repo, self.number))
    }

    pub fn decode(value: &str) -> Option<Self> {
        let bytes = URL_SAFE_NO_PAD.decode(value.trim()).ok()?;
        let text = String::from_utf8(bytes).ok()?;
        let mut parts = text.splitn(3, '|');
        let nanos = parts.next()?.parse().ok()?;
        let repo = parts.next()?.to_owned();
        let number = parts.next()?.parse().ok()?;
        Some(Self {
            nanos,
            repo,
            number,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct HistoryQuery {
    /// Sessions the `agent` filter resolved to; `Some(empty)` matches nothing.
    pub agent_ids: Option<BTreeSet<String>>,
    /// A repo name, or `owner/name`; case-insensitive.
    pub repo: Option<String>,
    pub open_only: bool,
    pub before: Option<Cursor>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPage {
    pub rows: Vec<HistoryRow>,
    pub next_before: Option<String>,
    /// More than one repo among the matching rows: the page names them.
    pub multi_repo: bool,
}

/// Every stored row the page reads, loaded once per request.
#[derive(Debug, Clone, Default)]
pub struct HistoryData {
    items: BTreeMap<(String, i64), WorkItem>,
    /// Reserved spawn claims are left out: pages ignore them.
    claims: Vec<WorkClaim>,
    /// `(repo, pr)` → linked tickets, either source.
    pr_tickets: BTreeMap<(String, i64), BTreeSet<i64>>,
    /// `(repo, ticket)` → linked PRs, either source.
    ticket_prs: BTreeMap<(String, i64), BTreeSet<i64>>,
    events: Vec<StoredEvent>,
    reviews: Vec<CodexReview>,
    docs: Vec<HistoryDoc>,
    index: Index,
}

/// Positions into `HistoryData`'s vectors, keyed the way pages look them
/// up, so a row costs its own facts rather than a scan of every table.
#[derive(Debug, Clone, Default)]
struct Index {
    claims_by_item: BTreeMap<(String, i64), Vec<usize>>,
    reviews_by_pr: BTreeMap<(String, i64), Vec<usize>>,
    docs_by_pr: BTreeMap<(String, i64), Vec<usize>>,
    commit_docs_by_repo: BTreeMap<String, Vec<usize>>,
    events_by_ticket: BTreeMap<(String, i64), Vec<usize>>,
    events_by_pr: BTreeMap<(String, i64), Vec<usize>>,
    /// The latest `worktree.removed` / `worktree.left` event per path.
    worktree_latest: BTreeMap<String, usize>,
    /// Session id → the latest name snapshot (claims, then doc authorship).
    names: BTreeMap<String, String>,
}

/// One row's subject and the items whose facts it gathers.
struct Thread<'a> {
    item: &'a WorkItem,
    /// Ticket threads: the linked PRs. PR threads: the PR itself.
    prs: Vec<i64>,
    /// PR threads: the tickets the PR links to (timeline context only).
    tickets: Vec<i64>,
}

impl Thread<'_> {
    fn is_ticket(&self) -> bool {
        self.item.kind == "ticket"
    }
}

impl HistoryData {
    /// Reads the retained queue DB at `db_path`. A missing DB reads empty.
    pub fn load(db_path: &Path) -> Result<Self> {
        if !db_path.exists() {
            return Ok(Self::default());
        }
        let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let mut data = Self::default();
        if table_exists(&conn, "work_items")? {
            let mut statement = conn.prepare(&format!("SELECT {ITEM_COLUMNS} FROM work_items"))?;
            for item in statement.query_map([], item_from_row)? {
                let item = item?;
                data.items.insert((item.repo.clone(), item.number), item);
            }
        }
        if table_exists(&conn, "work_claims")? {
            let mut statement = conn.prepare(&format!(
                "SELECT {CLAIM_COLUMNS} FROM work_claims
                  WHERE reserved_at IS NULL ORDER BY claimed_at, id"
            ))?;
            data.claims = statement
                .query_map([], claim_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        if table_exists(&conn, "work_links")? {
            let mut statement =
                conn.prepare("SELECT repo, pr_number, ticket_number FROM work_links")?;
            for row in statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })? {
                let (repo, pr, ticket) = row?;
                data.pr_tickets
                    .entry((repo.clone(), pr))
                    .or_default()
                    .insert(ticket);
                data.ticket_prs
                    .entry((repo, ticket))
                    .or_default()
                    .insert(pr);
            }
        }
        if table_exists(&conn, "events")? {
            let mut statement = conn.prepare(
                "SELECT id, ts, kind, session_id, repo, ticket, pr, payload FROM events ORDER BY id",
            )?;
            data.events = statement
                .query_map([], |row| {
                    Ok(StoredEvent {
                        id: row.get(0)?,
                        ts: row.get(1)?,
                        kind: row.get(2)?,
                        session_id: row.get(3)?,
                        repo: row.get(4)?,
                        ticket: row.get(5)?,
                        pr: row.get(6)?,
                        payload: row
                            .get::<_, Option<String>>(7)?
                            .and_then(|text| serde_json::from_str(&text).ok())
                            .unwrap_or(Value::Null),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        if table_exists(&conn, "codex_review_request_registrations")? {
            let mut statement = conn.prepare(
                "SELECT repo, pr_number, requester_session_id, requested_at, review_landed_at,
                        review_url
                   FROM codex_review_request_registrations ORDER BY requested_at, id",
            )?;
            data.reviews = statement
                .query_map([], |row| {
                    Ok(CodexReview {
                        repo: row.get::<_, String>(0)?.to_ascii_lowercase(),
                        pr: row.get(1)?,
                        requester_session_id: row.get(2)?,
                        requested_at: row.get(3)?,
                        landed_at: row.get(4)?,
                        review_url: row.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        if table_exists(&conn, "owner_docs")? {
            data.docs = load_docs(&conn, db_path)?;
        }
        data.build_index();
        Ok(data)
    }

    fn build_index(&mut self) {
        let mut index = Index::default();
        for (i, doc) in self.docs.iter().enumerate() {
            match doc.summary.doc.pr_number {
                Some(pr) => index
                    .docs_by_pr
                    .entry((doc.repo.clone(), pr))
                    .or_default()
                    .push(i),
                None => index
                    .commit_docs_by_repo
                    .entry(doc.repo.clone())
                    .or_default()
                    .push(i),
            }
            if let Some(name) = doc
                .summary
                .doc
                .author_session_name
                .clone()
                .filter(|n| !n.trim().is_empty())
            {
                index
                    .names
                    .insert(doc.summary.doc.author_session_id.clone(), name);
            }
        }
        // Claims come oldest first, so the last snapshot wins.
        for (i, claim) in self.claims.iter().enumerate() {
            index
                .claims_by_item
                .entry((claim.repo.clone(), claim.number))
                .or_default()
                .push(i);
            if let Some(name) = claim.session_name.clone().filter(|n| !n.trim().is_empty()) {
                index.names.insert(claim.session_id.clone(), name);
            }
        }
        for (i, review) in self.reviews.iter().enumerate() {
            index
                .reviews_by_pr
                .entry((review.repo.clone(), review.pr))
                .or_default()
                .push(i);
        }
        for (i, event) in self.events.iter().enumerate() {
            if matches!(event.kind.as_str(), "worktree.removed" | "worktree.left") {
                if let Some(path) = event.payload["path"].as_str() {
                    index.worktree_latest.insert(path.to_owned(), i);
                }
            }
            let Some(repo) = event.repo.clone() else {
                continue;
            };
            if let Some(ticket) = event.ticket {
                index
                    .events_by_ticket
                    .entry((repo.clone(), ticket))
                    .or_default()
                    .push(i);
            }
            if let Some(pr) = event.pr {
                index.events_by_pr.entry((repo, pr)).or_default().push(i);
            }
        }
        self.index = index;
    }

    fn indexed<'a, T>(
        items: &'a [T],
        map: &BTreeMap<(String, i64), Vec<usize>>,
        repo: &str,
        numbers: impl IntoIterator<Item = i64>,
    ) -> Vec<(usize, &'a T)> {
        let mut found: Vec<usize> = numbers
            .into_iter()
            .filter_map(|n| map.get(&(repo.to_owned(), n)))
            .flatten()
            .copied()
            .collect();
        found.sort_unstable();
        found.dedup();
        found.into_iter().map(|i| (i, &items[i])).collect()
    }

    /// Sessions whose claim snapshots or doc authorship carry `name`
    /// (case-insensitive): how a retired agent is found by name.
    pub fn sessions_named(&self, name: &str) -> BTreeSet<String> {
        let name = name.trim();
        let mut ids = BTreeSet::new();
        for claim in &self.claims {
            if claim
                .session_name
                .as_deref()
                .is_some_and(|snapshot| snapshot.eq_ignore_ascii_case(name))
            {
                ids.insert(claim.session_id.clone());
            }
        }
        for doc in &self.docs {
            if doc
                .summary
                .doc
                .author_session_name
                .as_deref()
                .is_some_and(|snapshot| snapshot.eq_ignore_ascii_case(name))
            {
                ids.insert(doc.summary.doc.author_session_id.clone());
            }
        }
        ids
    }

    /// Whether any stored row names `session_id` (a purged session's id).
    pub fn knows_session(&self, session_id: &str) -> bool {
        self.claims.iter().any(|c| c.session_id == session_id)
            || self
                .reviews
                .iter()
                .any(|r| r.requester_session_id.as_deref() == Some(session_id))
            || self.docs.iter().any(|d| {
                d.summary.doc.author_session_id == session_id
                    || d.publishes.iter().any(|p| p.session_id == session_id)
            })
    }

    /// The list: one row per thread, newest activity first.
    pub fn list(
        &self,
        sessions: &SessionDirectory,
        query: &HistoryQuery,
        now: OffsetDateTime,
    ) -> HistoryPage {
        let repo_filter = query
            .repo
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty());
        let mut rows: Vec<HistoryRow> = self
            .threads()
            .into_iter()
            .filter(|thread| repo_filter.is_none_or(|repo| repo_matches(&thread.item.repo, repo)))
            .filter(|thread| !query.open_only || thread.item.state == "open")
            .filter(|thread| {
                query
                    .agent_ids
                    .as_ref()
                    .is_none_or(|ids| self.thread_involves(thread, ids))
            })
            .map(|thread| self.row(&thread, sessions, now))
            .collect();
        let multi_repo = rows
            .iter()
            .map(|row| row.repo.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            > 1;
        rows.sort_by_key(|row| std::cmp::Reverse(Cursor::of(row)));
        if let Some(before) = &query.before {
            rows.retain(|row| Cursor::of(row) < *before);
        }
        let limit = query.limit.clamp(1, MAX_LIMIT);
        let next_before = (rows.len() > limit).then(|| Cursor::of(&rows[limit - 1]).encode());
        rows.truncate(limit);
        HistoryPage {
            rows,
            next_before,
            multi_repo,
        }
    }

    /// Repos that have an item with this number, for `/t/<repo-name>/<n>`.
    pub fn repos_for(&self, name: &str, number: i64) -> Vec<String> {
        self.items
            .keys()
            .filter(|(repo, n)| *n == number && repo_name(repo).eq_ignore_ascii_case(name))
            .map(|(repo, _)| repo.clone())
            .collect()
    }

    /// One item's page; `None` when sm has no row for it.
    pub fn timeline(
        &self,
        sessions: &SessionDirectory,
        repo: &str,
        number: i64,
        now: OffsetDateTime,
    ) -> Option<Timeline> {
        let item = self.items.get(&(repo.to_owned(), number))?;
        let thread = if item.kind == "ticket" {
            self.ticket_thread(item)
        } else {
            Thread {
                item,
                prs: vec![item.number],
                tickets: self.linked(&self.pr_tickets, &item.repo, item.number),
            }
        };
        Some(Timeline {
            item: self.row(&thread, sessions, now),
            events: self.entries(&thread, sessions),
        })
    }

    fn linked(
        &self,
        map: &BTreeMap<(String, i64), BTreeSet<i64>>,
        repo: &str,
        number: i64,
    ) -> Vec<i64> {
        map.get(&(repo.to_owned(), number))
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    fn ticket_thread<'a>(&'a self, item: &'a WorkItem) -> Thread<'a> {
        Thread {
            item,
            prs: self.linked(&self.ticket_prs, &item.repo, item.number),
            tickets: vec![item.number],
        }
    }

    /// Every tracked ticket, and every tracked PR that links to no tracked
    /// ticket.
    fn threads(&self) -> Vec<Thread<'_>> {
        self.items
            .values()
            .filter_map(|item| {
                if item.kind == "ticket" {
                    return Some(self.ticket_thread(item));
                }
                let tickets = self.linked(&self.pr_tickets, &item.repo, item.number);
                let linked_to_tracked = tickets
                    .iter()
                    .any(|ticket| self.items.contains_key(&(item.repo.clone(), *ticket)));
                (!linked_to_tracked).then(|| Thread {
                    item,
                    prs: vec![item.number],
                    tickets: Vec::new(),
                })
            })
            .collect()
    }

    /// Claims on the thread's item and its PRs, oldest first.
    fn thread_claims<'a>(&'a self, thread: &Thread<'_>) -> Vec<&'a WorkClaim> {
        let numbers = std::iter::once(thread.item.number).chain(thread.prs.iter().copied());
        Self::indexed(
            &self.claims,
            &self.index.claims_by_item,
            &thread.item.repo,
            numbers,
        )
        .into_iter()
        .map(|(_, claim)| claim)
        .collect()
    }

    fn thread_reviews<'a>(&'a self, thread: &Thread<'_>) -> Vec<&'a CodexReview> {
        Self::indexed(
            &self.reviews,
            &self.index.reviews_by_pr,
            &thread.item.repo,
            thread.prs.iter().copied(),
        )
        .into_iter()
        .map(|(_, review)| review)
        .collect()
    }

    /// Docs published on a thread PR, plus (ticket threads) commit-only docs
    /// published by a session that held the ticket at that moment.
    fn thread_docs<'a>(&'a self, thread: &Thread<'_>) -> Vec<&'a HistoryDoc> {
        let repo = &thread.item.repo;
        let mut found = Self::indexed(
            &self.docs,
            &self.index.docs_by_pr,
            repo,
            thread.prs.iter().copied(),
        );
        if thread.is_ticket() {
            let ticket_claims: Vec<&WorkClaim> = self
                .index
                .claims_by_item
                .get(&(repo.clone(), thread.item.number))
                .into_iter()
                .flatten()
                .map(|&i| &self.claims[i])
                .collect();
            if !ticket_claims.is_empty() {
                for &i in self
                    .index
                    .commit_docs_by_repo
                    .get(repo)
                    .into_iter()
                    .flatten()
                {
                    let doc = &self.docs[i];
                    if doc.publishes.iter().any(|publish| {
                        ticket_claims
                            .iter()
                            .any(|claim| held_at(claim, &publish.session_id, &publish.published_at))
                    }) {
                        found.push((i, doc));
                    }
                }
                found.sort_by_key(|(i, _)| *i);
            }
        }
        found.into_iter().map(|(_, doc)| doc).collect()
    }

    /// A ticket's events and its PRs' events; a PR's own events and the
    /// ticket-only events of the tickets it links to. Oldest first.
    fn thread_events<'a>(&'a self, thread: &Thread<'_>) -> Vec<&'a StoredEvent> {
        let repo = &thread.item.repo;
        let mut found = if thread.is_ticket() {
            let mut found = Self::indexed(
                &self.events,
                &self.index.events_by_ticket,
                repo,
                [thread.item.number],
            );
            found.extend(Self::indexed(
                &self.events,
                &self.index.events_by_pr,
                repo,
                thread.prs.iter().copied(),
            ));
            found
        } else {
            let mut found = Self::indexed(
                &self.events,
                &self.index.events_by_pr,
                repo,
                [thread.item.number],
            );
            found.extend(
                Self::indexed(
                    &self.events,
                    &self.index.events_by_ticket,
                    repo,
                    thread.tickets.iter().copied(),
                )
                .into_iter()
                .filter(|(_, event)| event.pr.is_none()),
            );
            found
        };
        found.sort_by_key(|(i, _)| *i);
        found.dedup_by_key(|(i, _)| *i);
        found.into_iter().map(|(_, event)| event).collect()
    }

    fn thread_involves(&self, thread: &Thread<'_>, ids: &BTreeSet<String>) -> bool {
        self.thread_claims(thread)
            .iter()
            .any(|claim| ids.contains(&claim.session_id))
            || self.thread_docs(thread).iter().any(|doc| {
                ids.contains(&doc.summary.doc.author_session_id)
                    || doc.publishes.iter().any(|p| ids.contains(&p.session_id))
            })
            || self.thread_reviews(thread).iter().any(|review| {
                review
                    .requester_session_id
                    .as_ref()
                    .is_some_and(|id| ids.contains(id))
            })
    }

    /// The live name when the session exists, else the latest snapshot.
    fn session_name(&self, sessions: &SessionDirectory, session_id: &str) -> String {
        if let Some(session) = sessions.get(session_id) {
            return session.name.clone();
        }
        self.index
            .names
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| session_id.to_owned())
    }

    fn row(
        &self,
        thread: &Thread<'_>,
        sessions: &SessionDirectory,
        now: OffsetDateTime,
    ) -> HistoryRow {
        let item = thread.item;
        let claims = self.thread_claims(thread);
        let reviews = self.thread_reviews(thread);
        let docs = self.thread_docs(thread);

        let mut agents: Vec<RowAgent> = Vec::new();
        for claim in &claims {
            let active = claim.ended_at.is_none();
            match agents.iter_mut().find(|a| a.session_id == claim.session_id) {
                Some(agent) => {
                    if nanos(&claim.claimed_at) < nanos(&agent.claimed_at) {
                        agent.claimed_at = claim.claimed_at.clone();
                    }
                    if active {
                        agent.ended_at = None;
                        agent.end_reason = None;
                    } else if agent.ended_at.as_deref().is_some_and(|at| {
                        nanos(claim.ended_at.as_deref().unwrap_or_default()) > nanos(at)
                    }) {
                        agent.ended_at = claim.ended_at.clone();
                        agent.end_reason = claim.end_reason.clone();
                    }
                }
                None => agents.push(RowAgent {
                    session_id: claim.session_id.clone(),
                    name: self.session_name(sessions, &claim.session_id),
                    state: holder_state(sessions, &claim.session_id)
                        .as_str()
                        .to_owned(),
                    claimed_at: claim.claimed_at.clone(),
                    ended_at: claim.ended_at.clone(),
                    end_reason: claim.end_reason.clone(),
                }),
            }
        }
        agents.sort_by_key(|agent| (nanos(&agent.claimed_at), agent.session_id.clone()));

        let prs = thread
            .prs
            .iter()
            .map(|&number| {
                let pr_item = self.items.get(&(item.repo.clone(), number));
                let requests: Vec<_> = reviews.iter().filter(|r| r.pr == number).collect();
                RowPr {
                    number,
                    title: pr_item.map(|i| i.title.clone()).unwrap_or_default(),
                    state: pr_item.map_or_else(|| "open".to_owned(), |i| i.state.clone()),
                    url: pr_item
                        .map(|i| i.url.clone())
                        .filter(|url| !url.is_empty())
                        .unwrap_or_else(|| {
                            format!("https://github.com/{}/pull/{number}", item.repo)
                        }),
                    codex_requested: requests.len(),
                    codex_landed: requests.iter().filter(|r| r.landed_at.is_some()).count(),
                }
            })
            .collect::<Vec<_>>();

        let row_docs = docs
            .iter()
            .map(|doc| RowDoc {
                id: doc.summary.doc.id.clone(),
                name: doc_name(&doc.summary.doc.repo, &doc.summary.doc.path),
                title: doc.summary.doc.title.clone(),
                state: doc.summary.state.as_str().to_owned(),
                reader_path: doc_readable_path(
                    &doc.summary.doc.repo,
                    &doc.summary.doc.path,
                    &doc.summary.latest_commit_sha,
                ),
                owner_reviews: doc.reviews.len(),
                author_session_id: doc.summary.doc.author_session_id.clone(),
                published_at: doc.summary.published_at.clone(),
            })
            .collect::<Vec<_>>();

        let flags = self.flags(thread, &claims, &prs, sessions, now);
        let mut latest: Option<(i128, String)> = None;
        let mut bump = |at: &str| {
            let n = nanos(at);
            if at.is_empty() || n == i128::MIN {
                return;
            }
            if latest.as_ref().is_none_or(|(best, _)| n > *best) {
                latest = Some((n, at.to_owned()));
            }
        };
        // Every fact the timeline shows, without rendering it.
        for claim in &claims {
            bump(&claim.claimed_at);
            bump(claim.ended_at.as_deref().unwrap_or_default());
        }
        for event in self.thread_events(thread) {
            bump(&event.ts);
        }
        for review in &reviews {
            bump(&review.requested_at);
            bump(review.landed_at.as_deref().unwrap_or_default());
        }
        for doc in &docs {
            for publish in &doc.publishes {
                bump(&publish.published_at);
            }
            for review in &doc.reviews {
                bump(&review.submitted_at);
            }
        }
        let (sort_nanos, last_activity) = match latest {
            Some((n, at)) => (n, Some(at)),
            None => (i128::MIN, None),
        };

        HistoryRow {
            repo: item.repo.clone(),
            number: item.number,
            kind: item.kind.clone(),
            title: item.title.clone(),
            state: item.state.clone(),
            url: item.url.clone(),
            flags: flags.into_iter().map(Flag::id).collect(),
            agents,
            prs,
            docs: row_docs,
            last_activity,
            history_path: history_path(&item.repo, item.number),
            linked_tickets: if thread.is_ticket() {
                Vec::new()
            } else {
                thread.tickets.clone()
            },
            synced_at: item.synced_at.clone(),
            sync_error: item.sync_error.clone(),
            sort_nanos,
        }
    }

    fn flags(
        &self,
        thread: &Thread<'_>,
        claims: &[&WorkClaim],
        prs: &[RowPr],
        sessions: &SessionDirectory,
        now: OffsetDateTime,
    ) -> Vec<Flag> {
        let item = thread.item;
        let mut flags = Vec::new();

        // Two or more live holders of one item who are not all in one line.
        // Stopped holders are dormant and retired ones cannot hold.
        let mut groups: Vec<i64> = vec![item.number];
        groups.extend(thread.prs.iter().copied().filter(|&n| n != item.number));
        let collision = groups.iter().any(|&number| {
            let holders: BTreeSet<&str> = claims
                .iter()
                .filter(|c| c.number == number && c.ended_at.is_none())
                .filter(|c| {
                    matches!(
                        holder_state(sessions, &c.session_id),
                        HolderState::Working | HolderState::Idle
                    )
                })
                .map(|c| c.session_id.as_str())
                .collect();
            let holders: Vec<&str> = holders.into_iter().collect();
            holders.len() >= 2
                && holders.iter().enumerate().any(|(i, a)| {
                    holders[i + 1..]
                        .iter()
                        .any(|b| sessions.relation(a, b).is_none())
                })
        });
        if collision {
            flags.push(Flag::TwoAgents);
        }

        if thread.is_ticket() && item.state == "open" {
            if prs.iter().any(|pr| pr.state == "merged") {
                flags.push(Flag::OpenAfterMerge);
            }
            // Claimed once, held by nobody now. `taken` and `superseded`
            // always hand the ticket to another claim, so they never leave
            // it unheld by themselves; `closed` is not "no holder" but a
            // reopened ticket.
            let ticket_claims: Vec<_> = claims.iter().filter(|c| c.number == item.number).collect();
            if !ticket_claims.is_empty()
                && ticket_claims.iter().all(|c| {
                    c.ended_at.is_some()
                        && matches!(
                            c.end_reason.as_deref(),
                            Some("retired" | "released" | "taken" | "superseded")
                        )
                })
            {
                flags.push(Flag::NoLiveHolder);
            }
        }

        let paths: BTreeSet<&str> = claims
            .iter()
            .filter_map(|c| c.worktree_path.as_deref())
            .collect();
        // Retire cleanup records the path with symlinks resolved.
        let left = paths.iter().any(|path| {
            let latest = self.index.worktree_latest.get(*path).or_else(|| {
                self.index
                    .worktree_latest
                    .get(&crate::work_claims::worktrees::path_key(path))
            });
            latest.map(|&i| &self.events[i]).is_some_and(|event| {
                event.kind == "worktree.left" && event.payload["reason"].as_str() != Some("absent")
            })
        });
        if left {
            flags.push(Flag::WorktreeLeft);
        }

        // Stale: the last fetch failed, never happened, or (for an open item
        // sm still tracks because someone holds it) is over an hour old.
        // Closed and merged items are never re-polled, so their age means
        // nothing.
        let held = claims.iter().any(|c| c.ended_at.is_none());
        let old = item
            .synced_at
            .as_deref()
            .and_then(parse_time)
            .is_some_and(|at| (now - at).whole_seconds() > STALE_AFTER_SECONDS);
        if item.sync_error.is_some()
            || item.synced_at.is_none()
            || (item.state == "open" && held && old)
        {
            flags.push(Flag::Stale);
        }
        flags
    }

    /// The timeline, oldest first.
    fn entries(&self, thread: &Thread<'_>, sessions: &SessionDirectory) -> Vec<TimelineEntry> {
        let mut entries = Vec::new();
        let name_of = |id: &str| self.session_name(sessions, id);
        let events = self.thread_events(thread);
        // Claims recorded without their event (backfill, or taken before
        // the log existed) still show, from the claim row itself.
        let logged = |kind: &str, claim_id: &str| {
            events
                .iter()
                .any(|e| e.kind == kind && e.payload["claim_id"].as_str() == Some(claim_id))
        };
        let mut derived = Vec::new();
        for claim in self.thread_claims(thread) {
            let (ticket, pr) = match claim.kind.as_str() {
                "pr" => (None, Some(claim.number)),
                _ => (Some(claim.number), None),
            };
            let at_claim = |kind: &str, ts: &str, payload: Value| StoredEvent {
                id: 0,
                ts: ts.to_owned(),
                kind: kind.to_owned(),
                session_id: Some(claim.session_id.clone()),
                repo: Some(claim.repo.clone()),
                ticket,
                pr,
                payload,
            };
            if !logged("claim.taken", &claim.id) {
                derived.push(at_claim(
                    "claim.taken",
                    &claim.claimed_at,
                    serde_json::json!({"claim_id": claim.id, "source": claim.source}),
                ));
            }
            if let (Some(ended_at), false) = (&claim.ended_at, logged("claim.released", &claim.id))
            {
                derived.push(at_claim(
                    "claim.released",
                    ended_at,
                    serde_json::json!({
                        "claim_id": claim.id,
                        "end_reason": claim.end_reason,
                        "ended_by_session_id": claim.ended_by_session_id,
                    }),
                ));
            }
        }
        for event in events.into_iter().chain(derived.iter()) {
            let (text, link) = self.describe_event(event, thread, &name_of);
            entries.push(TimelineEntry {
                at: event.ts.clone(),
                session_id: event.session_id.clone(),
                name: event.session_id.as_deref().map(name_of),
                kind: event.kind.clone(),
                text,
                link,
                sort_nanos: nanos(&event.ts),
            });
        }
        for review in self.thread_reviews(thread) {
            let session_id = review.requester_session_id.clone();
            let name = session_id.as_deref().map(name_of);
            entries.push(TimelineEntry {
                at: review.requested_at.clone(),
                session_id: session_id.clone(),
                name: name.clone(),
                kind: "codex_review.requested".to_owned(),
                text: format!("requested Codex review on #{}", review.pr),
                link: None,
                sort_nanos: nanos(&review.requested_at),
            });
            if let Some(landed) = &review.landed_at {
                entries.push(TimelineEntry {
                    at: landed.clone(),
                    session_id,
                    name,
                    kind: "codex_review.landed".to_owned(),
                    text: format!("Codex review landed on #{}", review.pr),
                    link: review
                        .review_url
                        .clone()
                        .filter(|url| url.starts_with("https://")),
                    sort_nanos: nanos(landed),
                });
            }
        }
        for doc in self.thread_docs(thread) {
            let summary = &doc.summary;
            for (index, publish) in doc.publishes.iter().enumerate() {
                let verb = if index == 0 {
                    "published"
                } else {
                    "republished"
                };
                let requested = if publish.review_requested {
                    " (review requested)"
                } else {
                    ""
                };
                entries.push(TimelineEntry {
                    at: publish.published_at.clone(),
                    session_id: Some(publish.session_id.clone()),
                    name: Some(name_of(&publish.session_id)),
                    kind: "doc.published".to_owned(),
                    text: format!("{verb} {}{requested}", summary.doc.title),
                    link: Some(doc_readable_path(
                        &summary.doc.repo,
                        &summary.doc.path,
                        &publish.commit_sha,
                    )),
                    sort_nanos: nanos(&publish.published_at),
                });
            }
            for review in &doc.reviews {
                let verdict = OwnerDocVerdict::parse(&review.verdict)
                    .map_or_else(|| review.verdict.clone(), |v| v.wake_label().to_owned());
                entries.push(TimelineEntry {
                    at: review.submitted_at.clone(),
                    session_id: None,
                    name: None,
                    kind: "doc.reviewed".to_owned(),
                    text: format!("owner reviewed {}: {verdict}", summary.doc.title),
                    link: review.url.clone().filter(|url| url.starts_with("https://")),
                    sort_nanos: nanos(&review.submitted_at),
                });
            }
        }
        // Stable: same-instant entries keep their source order.
        entries.sort_by_key(|entry| entry.sort_nanos);
        entries
    }

    /// An event in words, relative to the page's item.
    fn describe_event(
        &self,
        event: &StoredEvent,
        thread: &Thread<'_>,
        name_of: &dyn Fn(&str) -> String,
    ) -> (String, Option<String>) {
        let payload = &event.payload;
        let subject = match event.pr {
            Some(pr) if thread.is_ticket() || pr != thread.item.number => format!("PR #{pr}"),
            Some(_) => "the PR".to_owned(),
            None if thread.is_ticket() && event.ticket == Some(thread.item.number) => {
                "the ticket".to_owned()
            }
            None => format!("ticket #{}", event.ticket.unwrap_or_default()),
        };
        let names = |key: &str| -> String {
            let list: Vec<String> = payload[key]
                .as_array()
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .map(|id| format!("{} ({id})", name_of(id)))
                        .collect()
                })
                .unwrap_or_default();
            list.join(", ")
        };
        let text = match event.kind.as_str() {
            "claim.taken" => {
                let how = match payload["source"].as_str() {
                    Some("spawn") => " at spawn",
                    Some("codex_review") => " (Codex review request)",
                    Some("doc_publish") => " (doc publish)",
                    Some("backfill") => " (from history)",
                    _ => "",
                };
                format!("claimed {subject}{how}")
            }
            "claim.released" => {
                let by = payload["ended_by_session_id"]
                    .as_str()
                    .map(|id| format!("{} ({id})", name_of(id)))
                    .unwrap_or_default();
                match payload["end_reason"].as_str().unwrap_or_default() {
                    "retired" => format!("retired; claim on {subject} ended"),
                    "released" => format!("released {subject}"),
                    "taken" => format!("claim on {subject} taken by {by}"),
                    "superseded" => {
                        format!("claim on {subject} ended while stopped; {by} claimed it")
                    }
                    "merged" => format!("claim on {subject} ended: merged"),
                    "closed" => format!("claim on {subject} ended: closed"),
                    other => format!("claim on {subject} ended: {other}"),
                }
            }
            "claim.refused" => format!(
                "was refused {subject}: held by {}",
                names("holder_session_ids")
            ),
            "claim.collision" => format!(
                "claimed {subject} while {} held it",
                names("holder_session_ids")
            ),
            "github.state_changed" => {
                let to = payload["to"].as_str().unwrap_or_default();
                let from = payload["from"].as_str().unwrap_or_default();
                let reason = payload["state_reason"]
                    .as_str()
                    .map(|r| format!(" ({})", r.replace('_', " ")))
                    .unwrap_or_default();
                let subject = capitalize(&subject);
                match (from, to) {
                    (_, "merged") => format!("{subject} merged"),
                    ("closed" | "merged", "open") => format!("{subject} reopened"),
                    (_, "closed") => format!("{subject} closed{reason}"),
                    _ => format!("{subject} {from} → {to}"),
                }
            }
            "link.added" | "link.removed" => {
                let pr = payload["pr_number"]
                    .as_i64()
                    .or(event.pr)
                    .unwrap_or_default();
                let ticket = payload["ticket_number"]
                    .as_i64()
                    .or(event.ticket)
                    .unwrap_or_default();
                let how = match payload["source"].as_str() {
                    Some("closing_ref") => " (the PR says it closes the ticket)",
                    Some("claim") => " (by the PR's claim)",
                    _ => "",
                };
                let pair = if thread.is_ticket() {
                    format!("PR #{pr}")
                } else {
                    format!("ticket #{ticket}")
                };
                if event.kind == "link.added" {
                    format!("{pair} linked{how}")
                } else {
                    format!("{pair} unlinked")
                }
            }
            "claim.nudge" => {
                let text = payload["text"].as_str().unwrap_or_default();
                if payload["message_id"].is_null() && event.session_id.is_none() {
                    format!("no live holder to tell: {text}")
                } else {
                    format!("was told: {text}")
                }
            }
            "worktree.removed" => format!(
                "worktree {} removed",
                payload["path"].as_str().unwrap_or_default()
            ),
            "worktree.left" => {
                let path = payload["path"].as_str().unwrap_or_default();
                match payload["reason"].as_str().unwrap_or_default() {
                    "absent" => format!("worktree {path} was already gone"),
                    reason => format!("worktree {path} kept: {reason}"),
                }
            }
            other => other.to_owned(),
        };
        let link = match event.pr {
            Some(pr) if thread.is_ticket() || pr != thread.item.number => self
                .items
                .get(&(thread.item.repo.clone(), pr))
                .map(|item| item.url.clone())
                .filter(|url| url.starts_with("https://")),
            _ => None,
        };
        (text, link)
    }
}

fn load_docs(conn: &Connection, db_path: &Path) -> Result<Vec<HistoryDoc>> {
    let summaries = OwnerDocStore::new(db_path.to_path_buf()).summaries(None, false)?;
    let mut publishes = BTreeMap::<String, Vec<DocPublish>>::new();
    if table_exists(conn, "owner_doc_publishes")? {
        let mut statement = conn.prepare(
            "SELECT doc_id, commit_sha, session_id, review_requested, published_at
               FROM owner_doc_publishes ORDER BY id",
        )?;
        for row in statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                DocPublish {
                    commit_sha: row.get(1)?,
                    session_id: row.get(2)?,
                    review_requested: row.get::<_, i64>(3)? != 0,
                    published_at: row.get(4)?,
                },
            ))
        })? {
            let (doc_id, publish) = row?;
            publishes.entry(doc_id).or_default().push(publish);
        }
    }
    let mut reviews = BTreeMap::<String, Vec<PostedOwnerReview>>::new();
    if table_exists(conn, "owner_doc_reviews")? {
        let mut statement = conn.prepare(
            "SELECT doc_id, verdict, submitted_at, github_review_url, delivered_to_session_id
               FROM owner_doc_reviews WHERE status = 'posted' ORDER BY submitted_at, rowid",
        )?;
        for row in statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                PostedOwnerReview {
                    verdict: row.get(1)?,
                    submitted_at: row.get(2)?,
                    url: row.get(3)?,
                    delivered_to_session_id: row.get(4)?,
                },
            ))
        })? {
            let (doc_id, review) = row?;
            reviews.entry(doc_id).or_default().push(review);
        }
    }
    Ok(summaries
        .into_iter()
        .map(|summary| HistoryDoc {
            repo: summary.doc.repo.to_ascii_lowercase(),
            publishes: publishes.remove(&summary.doc.id).unwrap_or_default(),
            reviews: reviews.remove(&summary.doc.id).unwrap_or_default(),
            summary,
        })
        .collect())
}

/// Whether `claim` belonged to `session_id` and was active at `at`.
fn held_at(claim: &WorkClaim, session_id: &str, at: &str) -> bool {
    let at = nanos(at);
    claim.session_id == session_id
        && nanos(&claim.claimed_at) <= at
        && claim
            .ended_at
            .as_deref()
            .is_none_or(|ended| nanos(ended) >= at)
}

fn holder_state(sessions: &SessionDirectory, session_id: &str) -> HolderState {
    // A session missing from the store was retired and purged.
    sessions
        .get(session_id)
        .map_or(HolderState::Retired, |session| session.state)
}

fn repo_matches(repo: &str, filter: &str) -> bool {
    if filter.contains('/') {
        repo.eq_ignore_ascii_case(filter)
    } else {
        repo_name(repo).eq_ignore_ascii_case(filter)
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

pub fn parse_time(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value.trim(), &Rfc3339).ok()
}

/// Sort key for an RFC 3339 time; unparseable sorts first.
fn nanos(value: &str) -> i128 {
    parse_time(value).map_or(i128::MIN, OffsetDateTime::unix_timestamp_nanos)
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

#[cfg(test)]
mod tests;
