//! Owner docs (sm#1447): pointers to agent-written docs that live in git.
//!
//! sm stores `(repo, path, pr, commit)` pointers and derived read state; file
//! bytes always come from GitHub at a pinned commit and are cached on disk by
//! commit SHA, which never needs invalidating. See `specs/1447_owner_docs.md`.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{bail, Context, Result};
use rand_core::{OsRng, RngCore};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Row};
use serde::Serialize;
use sha1::{Digest, Sha1};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

/// Unused cache entries older than this are pruned.
pub const DOC_CACHE_MAX_IDLE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OwnerDoc {
    pub id: String,
    pub repo: String,
    pub path: String,
    pub pr_number: Option<i64>,
    pub author_session_id: String,
    pub author_session_name: Option<String>,
    pub title: String,
    pub note: Option<String>,
    pub retracted_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OwnerDocPublish {
    pub id: i64,
    pub doc_id: String,
    pub commit_sha: String,
    pub blob_sha: String,
    pub session_id: String,
    pub review_requested: bool,
    pub published_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerDocState {
    New,
    Updated,
    ReviewRequested,
    Reviewed,
    Read,
}

impl OwnerDocState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Updated => "updated",
            Self::ReviewRequested => "review_requested",
            Self::Reviewed => "reviewed",
            Self::Read => "read",
        }
    }

    /// Unread or awaiting the owner: what the `sm watch` marker counts.
    pub fn needs_owner(self) -> bool {
        matches!(self, Self::New | Self::Updated | Self::ReviewRequested)
    }
}

/// A doc with its latest publish and derived state, as lists show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OwnerDocSummary {
    #[serde(flatten)]
    pub doc: OwnerDoc,
    pub state: OwnerDocState,
    pub latest_commit_sha: String,
    pub latest_blob_sha: String,
    pub published_at: String,
    pub publish_count: usize,
    /// The latest posted review reached no session: its author was retired
    /// with no parent to take it.
    pub review_undelivered: bool,
}

/// A comment the owner is writing, anchored to a revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OwnerDocDraft {
    pub id: String,
    pub doc_id: String,
    pub commit_sha: String,
    /// Source line in the file at `commit_sha`; `None` is not placeable.
    pub line: Option<i64>,
    pub quote: String,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerDocVerdict {
    Approve,
    ChangesRequested,
    Comment,
}

impl OwnerDocVerdict {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "approve" => Some(Self::Approve),
            "changes_requested" => Some(Self::ChangesRequested),
            "comment" => Some(Self::Comment),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::ChangesRequested => "changes_requested",
            Self::Comment => "comment",
        }
    }

    /// The first line of the GitHub review body.
    pub fn body_header(self) -> &'static str {
        match self {
            Self::Approve => "**Verdict: Approved**",
            Self::ChangesRequested => "**Verdict: Changes requested**",
            Self::Comment => "**Verdict: Comments**",
        }
    }

    /// How the wake message names the verdict.
    pub fn wake_label(self) -> &'static str {
        match self {
            Self::Approve => "approved",
            Self::ChangesRequested => "changes requested",
            Self::Comment => "comments",
        }
    }
}

/// One `POST /docs/{id}/review` submission, keyed by the client's
/// `submission_id`. `status` is `submitting`, `posted` or `failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OwnerDocReview {
    pub id: String,
    pub status: String,
    pub pending_review_node_id: Option<String>,
    pub doc_id: String,
    pub commit_sha: String,
    pub blob_sha: String,
    pub verdict: String,
    pub body: Option<String>,
    pub line_comment_count: i64,
    pub file_comment_count: i64,
    pub github_review_id: Option<i64>,
    pub github_review_url: Option<String>,
    pub submitted_at: String,
    pub delivered_to_session_id: Option<String>,
}

/// What a review submission records once GitHub has the review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedOwnerDocReview {
    pub github_review_id: Option<i64>,
    pub github_review_url: String,
    pub line_comment_count: i64,
    pub file_comment_count: i64,
    /// Drafts the review carried; they are deleted with the transition.
    pub draft_ids: Vec<String>,
    /// `(session id, text)` of the `[sm review]` wake, or `None` when no
    /// session can take it (recorded as undelivered).
    pub wake: Option<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishOwnerDoc {
    pub repo: String,
    pub path: String,
    pub pr_number: Option<i64>,
    pub session_id: String,
    pub session_name: Option<String>,
    pub title: String,
    pub note: Option<String>,
    pub commit_sha: String,
    pub blob_sha: String,
    pub review_requested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedOwnerDoc {
    pub doc: OwnerDoc,
    pub publish: OwnerDocPublish,
    /// False when this publish added an event to an existing doc.
    pub created: bool,
}

/// Inputs to the state rules for one doc, all from stored data.
#[derive(Debug, Clone, Default)]
pub struct OwnerDocStateInputs<'a> {
    /// Publishes in publish order; the last one is the latest.
    pub publishes: Vec<&'a OwnerDocPublish>,
    pub viewed_blobs: BTreeSet<&'a str>,
    /// `(blob_sha, submitted_at)` of reviews with status `posted`.
    pub posted_reviews: Vec<(&'a str, &'a str)>,
}

/// Derived doc state; first match wins (spec "Storage").
pub fn derive_owner_doc_state(inputs: &OwnerDocStateInputs<'_>) -> OwnerDocState {
    let Some(latest) = inputs.publishes.last() else {
        return OwnerDocState::New;
    };
    if latest.review_requested
        && !inputs
            .posted_reviews
            .iter()
            .any(|(_, submitted_at)| timestamp_after(submitted_at, &latest.published_at))
    {
        return OwnerDocState::ReviewRequested;
    }
    if inputs
        .posted_reviews
        .iter()
        .any(|(blob, _)| *blob == latest.blob_sha)
    {
        return OwnerDocState::Reviewed;
    }
    if inputs.viewed_blobs.contains(latest.blob_sha.as_str()) {
        return OwnerDocState::Read;
    }
    if !inputs.viewed_blobs.is_empty() {
        return OwnerDocState::Updated;
    }
    OwnerDocState::New
}

fn timestamp_after(candidate: &str, reference: &str) -> bool {
    match (
        OffsetDateTime::parse(candidate, &Rfc3339),
        OffsetDateTime::parse(reference, &Rfc3339),
    ) {
        (Ok(candidate), Ok(reference)) => candidate > reference,
        _ => candidate > reference,
    }
}

/// Git's blob id: `sha1("blob <len>\0" + bytes)`.
pub fn git_blob_sha(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn is_full_commit_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn is_owner_doc_id(value: &str) -> bool {
    value.len() == 8 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn validate_repo_slug(repo: &str) -> Result<()> {
    let Some((owner, name)) = repo.split_once('/') else {
        bail!("repo must be owner/name, got {repo:?}");
    };
    let valid = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    };
    if !valid(owner) || !valid(name) {
        bail!("repo must be owner/name, got {repo:?}");
    }
    Ok(())
}

/// Repo-relative paths only: no absolute paths, `.`/`..` or empty segments.
pub fn validate_repo_path(path: &str) -> Result<()> {
    if path.is_empty() || path.contains('\\') || path.contains('\0') {
        bail!("invalid repo-relative path {path:?}");
    }
    let normal = Path::new(path)
        .components()
        .all(|component| matches!(component, Component::Normal(_)));
    if !normal
        || path
            .split('/')
            .any(|segment| matches!(segment, "" | "." | ".."))
    {
        bail!("invalid repo-relative path {path:?}");
    }
    Ok(())
}

/// Commit SHA characters in a printed `?version=`.
pub const DOC_VERSION_LEN: usize = 12;

/// `?version=` accepts a commit SHA prefix at least as long as git's short SHA.
pub fn is_doc_version(value: &str) -> bool {
    (7..=40).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The repo name without its owner: `acme/widgets` → `widgets`.
pub fn repo_name(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

/// How people and agents name a doc: `<repo-name>/<path in repo>`.
pub fn doc_name(repo: &str, path: &str) -> String {
    format!("{}/{path}", repo_name(repo))
}

/// The readable reader path, pinned to a publish:
/// `/docs/<repo-name>/<path in repo>?version=<12-char SHA prefix>`. Each
/// segment is percent-encoded, so the path survives spaces and `#`.
pub fn doc_readable_path(repo: &str, path: &str, commit_sha: &str) -> String {
    let encoded = std::iter::once(repo_name(repo))
        .chain(path.split('/'))
        .map(percent_encode_segment)
        .collect::<Vec<_>>()
        .join("/");
    let version = &commit_sha[..commit_sha.len().min(DOC_VERSION_LEN)];
    format!("/docs/{encoded}?version={version}")
}

fn percent_encode_segment(segment: &str) -> String {
    segment
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Why a readable name did not resolve. Every variant is a 404.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadableDocError {
    NotFound,
    VersionNotFound,
    AmbiguousVersion,
}

impl ReadableDocError {
    pub fn detail(self) -> &'static str {
        match self {
            Self::NotFound => "Doc not found",
            Self::VersionNotFound => "Version not found",
            Self::AmbiguousVersion => "Version matches more than one commit; use more characters",
        }
    }
}

pub fn init_owner_docs_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS owner_docs (
            id TEXT PRIMARY KEY,
            repo TEXT NOT NULL,
            path TEXT NOT NULL,
            pr_number INTEGER,
            author_session_id TEXT NOT NULL,
            author_session_name TEXT,
            title TEXT NOT NULL,
            note TEXT,
            retracted_at TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE UNIQUE INDEX IF NOT EXISTS owner_docs_key
            ON owner_docs(repo, path, IFNULL(pr_number, -1));
        CREATE INDEX IF NOT EXISTS owner_docs_author
            ON owner_docs(author_session_id);
        CREATE TABLE IF NOT EXISTS owner_doc_publishes (
            id INTEGER PRIMARY KEY,
            doc_id TEXT NOT NULL REFERENCES owner_docs(id),
            commit_sha TEXT NOT NULL,
            blob_sha TEXT NOT NULL,
            session_id TEXT NOT NULL,
            review_requested INTEGER NOT NULL DEFAULT 0,
            published_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS owner_doc_publishes_doc
            ON owner_doc_publishes(doc_id, id);
        CREATE TABLE IF NOT EXISTS owner_doc_views (
            doc_id TEXT NOT NULL,
            blob_sha TEXT NOT NULL,
            viewed_at TEXT NOT NULL,
            PRIMARY KEY (doc_id, blob_sha)
        );
        CREATE TABLE IF NOT EXISTS owner_doc_drafts (
            id TEXT PRIMARY KEY,
            doc_id TEXT NOT NULL,
            commit_sha TEXT NOT NULL,
            line INTEGER,
            quote TEXT NOT NULL,
            body TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS owner_doc_reviews (
            id TEXT PRIMARY KEY,
            status TEXT NOT NULL,
            pending_review_node_id TEXT,
            doc_id TEXT NOT NULL,
            commit_sha TEXT NOT NULL,
            blob_sha TEXT NOT NULL,
            verdict TEXT NOT NULL,
            body TEXT,
            line_comment_count INTEGER NOT NULL,
            file_comment_count INTEGER NOT NULL,
            github_review_id INTEGER,
            github_review_url TEXT,
            submitted_at TEXT NOT NULL,
            delivered_to_session_id TEXT
        );
        "#,
    )?;
    Ok(())
}

/// Owner-doc rows in the retained queue DB (`sm_send.db_path`).
#[derive(Debug, Clone)]
pub struct OwnerDocStore {
    db_path: PathBuf,
}

impl OwnerDocStore {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    fn open_write(&self) -> Result<Connection> {
        if let Some(parent) = self.db_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(&self.db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        init_owner_docs_schema(&conn)?;
        Ok(conn)
    }

    /// Read connections never create the DB; a missing one reads as empty.
    fn open_read(&self) -> Result<Option<Connection>> {
        if !self.db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let has_tables: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'owner_doc_reviews'",
                [],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        Ok(has_tables.then_some(conn))
    }

    pub fn ensure_schema(&self) -> Result<()> {
        self.open_write().map(|_| ())
    }

    /// Adds a publish event, creating the doc on first publish of its key.
    /// A republish takes over authorship when `author_is_live` says the
    /// recorded author is retired or gone, so owner reviews wake the agent
    /// still working the doc; a live author keeps it.
    pub fn publish(
        &self,
        request: PublishOwnerDoc,
        author_is_live: impl FnOnce(&str) -> bool,
    ) -> Result<PublishedOwnerDoc> {
        validate_repo_slug(&request.repo)?;
        validate_repo_path(&request.path)?;
        let mut conn = self.open_write()?;
        let tx = conn.transaction()?;
        let now = now_rfc3339();
        let existing = tx
            .query_row(
                &format!(
                    "SELECT {DOC_COLUMNS} FROM owner_docs
                     WHERE repo = ?1 AND path = ?2 AND IFNULL(pr_number, -1) = IFNULL(?3, -1)"
                ),
                params![request.repo, request.path, request.pr_number],
                doc_from_row,
            )
            .optional()?;
        let created = existing.is_none();
        let doc_id = match existing {
            Some(doc) => {
                // The latest publish carries the current title and note, and
                // republishing brings a retracted doc back.
                tx.execute(
                    "UPDATE owner_docs
                     SET title = ?2, note = ?3, retracted_at = NULL, updated_at = ?4
                     WHERE id = ?1",
                    params![doc.id, request.title, request.note, now],
                )?;
                if doc.author_session_id != request.session_id
                    && !author_is_live(&doc.author_session_id)
                {
                    tx.execute(
                        "UPDATE owner_docs
                         SET author_session_id = ?2, author_session_name = ?3
                         WHERE id = ?1",
                        params![doc.id, request.session_id, request.session_name],
                    )?;
                }
                doc.id
            }
            None => {
                let id = generate_owner_doc_id(&tx)?;
                tx.execute(
                    "INSERT INTO owner_docs
                     (id, repo, path, pr_number, author_session_id, author_session_name,
                      title, note, retracted_at, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?9)",
                    params![
                        id,
                        request.repo,
                        request.path,
                        request.pr_number,
                        request.session_id,
                        request.session_name,
                        request.title,
                        request.note,
                        now
                    ],
                )?;
                id
            }
        };
        tx.execute(
            "INSERT INTO owner_doc_publishes
             (doc_id, commit_sha, blob_sha, session_id, review_requested, published_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                doc_id,
                request.commit_sha,
                request.blob_sha,
                request.session_id,
                request.review_requested,
                now
            ],
        )?;
        let publish_id = tx.last_insert_rowid();
        let doc = get_doc_conn(&tx, &doc_id)?.context("published doc vanished")?;
        let publish = tx.query_row(
            &format!("SELECT {PUBLISH_COLUMNS} FROM owner_doc_publishes WHERE id = ?1"),
            params![publish_id],
            publish_from_row,
        )?;
        tx.commit()?;
        Ok(PublishedOwnerDoc {
            doc,
            publish,
            created,
        })
    }

    pub fn get(&self, doc_id: &str) -> Result<Option<OwnerDoc>> {
        let Some(conn) = self.open_read()? else {
            return Ok(None);
        };
        get_doc_conn(&conn, doc_id)
    }

    pub fn publishes(&self, doc_id: &str) -> Result<Vec<OwnerDocPublish>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(&format!(
            "SELECT {PUBLISH_COLUMNS} FROM owner_doc_publishes WHERE doc_id = ?1 ORDER BY id"
        ))?;
        let rows = statement
            .query_map(params![doc_id], publish_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn record_view(&self, doc_id: &str, blob_sha: &str) -> Result<()> {
        let conn = self.open_write()?;
        conn.execute(
            "INSERT OR IGNORE INTO owner_doc_views (doc_id, blob_sha, viewed_at)
             VALUES (?1, ?2, ?3)",
            params![doc_id, blob_sha, now_rfc3339()],
        )?;
        Ok(())
    }

    pub fn retract(&self, doc_id: &str) -> Result<Option<OwnerDoc>> {
        let conn = self.open_write()?;
        let now = now_rfc3339();
        conn.execute(
            "UPDATE owner_docs SET retracted_at = IFNULL(retracted_at, ?2), updated_at = ?2
             WHERE id = ?1",
            params![doc_id, now],
        )?;
        get_doc_conn(&conn, doc_id)
    }

    /// Summaries with derived state. `authors` limits the result to docs
    /// written by those sessions; `None` lists every doc.
    pub fn summaries(
        &self,
        authors: Option<&BTreeSet<String>>,
        include_retracted: bool,
    ) -> Result<Vec<OwnerDocSummary>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let docs = {
            let mut statement = conn.prepare(&format!(
                "SELECT {DOC_COLUMNS} FROM owner_docs ORDER BY created_at, id"
            ))?;
            let rows = statement
                .query_map([], doc_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let docs: Vec<_> = docs
            .into_iter()
            .filter(|doc| include_retracted || doc.retracted_at.is_none())
            .filter(|doc| authors.is_none_or(|authors| authors.contains(&doc.author_session_id)))
            .collect();
        if docs.is_empty() {
            return Ok(Vec::new());
        }
        let mut publishes = BTreeMap::<String, Vec<OwnerDocPublish>>::new();
        {
            let mut statement = conn.prepare(&format!(
                "SELECT {PUBLISH_COLUMNS} FROM owner_doc_publishes ORDER BY id"
            ))?;
            for publish in statement.query_map([], publish_from_row)? {
                let publish = publish?;
                publishes
                    .entry(publish.doc_id.clone())
                    .or_default()
                    .push(publish);
            }
        }
        let mut views = BTreeMap::<String, BTreeSet<String>>::new();
        {
            let mut statement = conn.prepare("SELECT doc_id, blob_sha FROM owner_doc_views")?;
            for row in statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })? {
                let (doc_id, blob) = row?;
                views.entry(doc_id).or_default().insert(blob);
            }
        }
        let mut reviews = BTreeMap::<String, Vec<(String, String)>>::new();
        // Whether each doc's latest posted review reached a session.
        let mut latest_delivered = BTreeMap::<String, bool>::new();
        {
            let mut statement = conn.prepare(
                "SELECT doc_id, blob_sha, submitted_at, delivered_to_session_id
                 FROM owner_doc_reviews WHERE status = 'posted'
                 ORDER BY submitted_at, rowid",
            )?;
            for row in statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })? {
                let (doc_id, blob, submitted_at, delivered_to) = row?;
                latest_delivered.insert(doc_id.clone(), delivered_to.is_some());
                reviews
                    .entry(doc_id)
                    .or_default()
                    .push((blob, submitted_at));
            }
        }
        let empty_views = BTreeSet::new();
        let mut summaries = Vec::new();
        for doc in docs {
            let Some(doc_publishes) = publishes.get(&doc.id).filter(|p| !p.is_empty()) else {
                continue;
            };
            let latest = doc_publishes.last().expect("non-empty publishes");
            let inputs = OwnerDocStateInputs {
                publishes: doc_publishes.iter().collect(),
                viewed_blobs: views
                    .get(&doc.id)
                    .unwrap_or(&empty_views)
                    .iter()
                    .map(String::as_str)
                    .collect(),
                posted_reviews: reviews
                    .get(&doc.id)
                    .map(|rows| {
                        rows.iter()
                            .map(|(blob, at)| (blob.as_str(), at.as_str()))
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            summaries.push(OwnerDocSummary {
                state: derive_owner_doc_state(&inputs),
                latest_commit_sha: latest.commit_sha.clone(),
                latest_blob_sha: latest.blob_sha.clone(),
                published_at: latest.published_at.clone(),
                publish_count: doc_publishes.len(),
                review_undelivered: latest_delivered.get(&doc.id) == Some(&false),
                doc,
            });
        }
        Ok(summaries)
    }

    /// Resolve `<repo-name>/<path>` plus an optional `?version=` to one doc
    /// and the commit to render. The repo name matches case-insensitively and
    /// the path exactly; retracted docs still resolve, as they do by id.
    ///
    /// One path can hold two docs (a PR doc and a commit-only doc). Without a
    /// version the newest publish across them wins; with one, the newest
    /// publish whose commit starts with the prefix wins, and a prefix that
    /// matches two different commits is refused.
    pub fn resolve_readable(
        &self,
        name: &str,
        path: &str,
        version: Option<&str>,
    ) -> Result<std::result::Result<(OwnerDoc, String), ReadableDocError>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Err(ReadableDocError::NotFound));
        };
        let docs: BTreeMap<String, OwnerDoc> = {
            let mut statement = conn.prepare(&format!(
                "SELECT {DOC_COLUMNS} FROM owner_docs WHERE path = ?1"
            ))?;
            let rows = statement
                .query_map(params![path], doc_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter()
                .filter(|doc| repo_name(&doc.repo).eq_ignore_ascii_case(name))
                .map(|doc| (doc.id.clone(), doc))
                .collect()
        };
        if docs.is_empty() {
            return Ok(Err(ReadableDocError::NotFound));
        }
        let mut publishes = Vec::new();
        for doc_id in docs.keys() {
            publishes.extend(self.publishes(doc_id)?);
        }
        if let Some(version) = version {
            let version = version.to_ascii_lowercase();
            if !is_doc_version(&version) {
                return Ok(Err(ReadableDocError::VersionNotFound));
            }
            publishes.retain(|publish| publish.commit_sha.starts_with(&version));
            let commits: BTreeSet<_> = publishes.iter().map(|p| &p.commit_sha).collect();
            if commits.len() > 1 {
                return Ok(Err(ReadableDocError::AmbiguousVersion));
            }
        }
        let Some(publish) = publishes.into_iter().max_by_key(|publish| publish.id) else {
            return Ok(Err(if version.is_some() {
                ReadableDocError::VersionNotFound
            } else {
                ReadableDocError::NotFound
            }));
        };
        Ok(Ok((docs[&publish.doc_id].clone(), publish.commit_sha)))
    }

    pub fn summary(&self, doc_id: &str) -> Result<Option<OwnerDocSummary>> {
        Ok(self
            .summaries(None, true)?
            .into_iter()
            .find(|summary| summary.doc.id == doc_id))
    }

    /// PR docs named `<repo-name>/<path>`: the candidates whose current PR
    /// head a readable `?version=` may name when it matches no publish.
    pub fn pr_docs_named(&self, name: &str, path: &str) -> Result<Vec<OwnerDoc>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(&format!(
            "SELECT {DOC_COLUMNS} FROM owner_docs WHERE path = ?1 AND pr_number IS NOT NULL
             ORDER BY created_at, id"
        ))?;
        let rows = statement
            .query_map(params![path], doc_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter(|doc| repo_name(&doc.repo).eq_ignore_ascii_case(name))
            .collect())
    }

    /// Every draft on the doc, across revisions, oldest first.
    pub fn drafts(&self, doc_id: &str) -> Result<Vec<OwnerDocDraft>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        drafts_conn(&conn, doc_id)
    }

    pub fn create_draft(
        &self,
        doc_id: &str,
        commit_sha: &str,
        line: Option<i64>,
        quote: &str,
        body: &str,
    ) -> Result<OwnerDocDraft> {
        let conn = self.open_write()?;
        let now = now_rfc3339();
        let id = random_hex(6);
        conn.execute(
            "INSERT INTO owner_doc_drafts
             (id, doc_id, commit_sha, line, quote, body, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![id, doc_id, commit_sha, line, quote, body, now],
        )?;
        get_draft_conn(&conn, doc_id, &id)?.context("created draft vanished")
    }

    pub fn update_draft(
        &self,
        doc_id: &str,
        draft_id: &str,
        body: &str,
    ) -> Result<Option<OwnerDocDraft>> {
        let conn = self.open_write()?;
        conn.execute(
            "UPDATE owner_doc_drafts SET body = ?3, updated_at = ?4
             WHERE doc_id = ?1 AND id = ?2",
            params![doc_id, draft_id, body, now_rfc3339()],
        )?;
        get_draft_conn(&conn, doc_id, draft_id)
    }

    pub fn delete_draft(&self, doc_id: &str, draft_id: &str) -> Result<bool> {
        let conn = self.open_write()?;
        Ok(conn.execute(
            "DELETE FROM owner_doc_drafts WHERE doc_id = ?1 AND id = ?2",
            params![doc_id, draft_id],
        )? > 0)
    }

    pub fn review(&self, submission_id: &str) -> Result<Option<OwnerDocReview>> {
        let Some(conn) = self.open_read()? else {
            return Ok(None);
        };
        get_review_conn(&conn, submission_id)
    }

    /// The doc's review submissions, oldest first.
    pub fn reviews(&self, doc_id: &str) -> Result<Vec<OwnerDocReview>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(&format!(
            "SELECT {REVIEW_COLUMNS} FROM owner_doc_reviews WHERE doc_id = ?1
             ORDER BY submitted_at, rowid"
        ))?;
        let rows = statement
            .query_map(params![doc_id], review_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Rows a crash or restart left mid-submit.
    pub fn submitting_reviews(&self) -> Result<Vec<OwnerDocReview>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(&format!(
            "SELECT {REVIEW_COLUMNS} FROM owner_doc_reviews WHERE status = 'submitting'
             ORDER BY submitted_at, rowid"
        ))?;
        let rows = statement
            .query_map([], review_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Records a submission as `submitting` before any GitHub call. Returns
    /// the stored row and whether this call inserted it; an existing row
    /// (a retry of the same `submission_id`) is returned unchanged.
    pub fn begin_review(
        &self,
        submission_id: &str,
        doc_id: &str,
        commit_sha: &str,
        blob_sha: &str,
        verdict: OwnerDocVerdict,
        body: Option<&str>,
    ) -> Result<(OwnerDocReview, bool)> {
        let conn = self.open_write()?;
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO owner_doc_reviews
             (id, status, pending_review_node_id, doc_id, commit_sha, blob_sha, verdict, body,
              line_comment_count, file_comment_count, github_review_id, github_review_url,
              submitted_at, delivered_to_session_id)
             VALUES (?1, 'submitting', NULL, ?2, ?3, ?4, ?5, ?6, 0, 0, NULL, NULL, ?7, NULL)",
            params![
                submission_id,
                doc_id,
                commit_sha,
                blob_sha,
                verdict.as_str(),
                body,
                now_rfc3339()
            ],
        )? > 0;
        let review = get_review_conn(&conn, submission_id)?.context("review row vanished")?;
        Ok((review, inserted))
    }

    pub fn set_pending_review_node_id(
        &self,
        submission_id: &str,
        node_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.open_write()?;
        conn.execute(
            "UPDATE owner_doc_reviews SET pending_review_node_id = ?2
             WHERE id = ?1 AND status = 'submitting'",
            params![submission_id, node_id],
        )?;
        Ok(())
    }

    /// The revision's latest unfinished (`submitting`) submission. While it
    /// exists its drafts are frozen and new submissions resume it.
    pub fn unfinished_review(
        &self,
        doc_id: &str,
        commit_sha: &str,
    ) -> Result<Option<OwnerDocReview>> {
        let Some(conn) = self.open_read()? else {
            return Ok(None);
        };
        Ok(conn
            .query_row(
                &format!(
                    "SELECT {REVIEW_COLUMNS} FROM owner_doc_reviews
                     WHERE doc_id = ?1 AND commit_sha = ?2 AND status = 'submitting'
                     ORDER BY submitted_at DESC, rowid DESC LIMIT 1"
                ),
                params![doc_id, commit_sha],
                review_from_row,
            )
            .optional()?)
    }

    /// A retry of a `failed` submission: back to `submitting`, to be
    /// reconciled against GitHub. Other states are returned unchanged, and
    /// so is a failed row once a newer submission of its doc and revision
    /// exists: that one (another device's) owns the drafts now, and reviving
    /// this row would post a second, empty review.
    pub fn reopen_review(&self, submission_id: &str) -> Result<OwnerDocReview> {
        let conn = self.open_write()?;
        conn.execute(
            "UPDATE owner_doc_reviews SET status = 'submitting'
             WHERE id = ?1 AND status = 'failed'
               AND NOT EXISTS (
                 SELECT 1 FROM owner_doc_reviews AS newer
                 WHERE newer.doc_id = owner_doc_reviews.doc_id
                   AND newer.commit_sha = owner_doc_reviews.commit_sha
                   AND newer.rowid > owner_doc_reviews.rowid)",
            params![submission_id],
        )?;
        get_review_conn(&conn, submission_id)?.context("review row vanished")
    }

    /// `submitting` → `failed`; the drafts stay for a retry.
    pub fn fail_review(&self, submission_id: &str) -> Result<()> {
        let conn = self.open_write()?;
        conn.execute(
            "UPDATE owner_doc_reviews SET status = 'failed', pending_review_node_id = NULL
             WHERE id = ?1 AND status = 'submitting'",
            params![submission_id],
        )?;
        Ok(())
    }

    /// `submitting` → `posted`, in one transaction with deleting the review's
    /// drafts and queueing its wake, so the wake is queued exactly once.
    /// Returns the row and whether this call made the transition.
    pub fn finish_review(
        &self,
        submission_id: &str,
        posted: &PostedOwnerDocReview,
    ) -> Result<(OwnerDocReview, bool)> {
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE owner_doc_reviews
             SET status = 'posted', pending_review_node_id = NULL,
                 line_comment_count = ?2, file_comment_count = ?3,
                 github_review_id = ?4, github_review_url = ?5,
                 delivered_to_session_id = ?6
             WHERE id = ?1 AND status = 'submitting'",
            params![
                submission_id,
                posted.line_comment_count,
                posted.file_comment_count,
                posted.github_review_id,
                posted.github_review_url,
                posted.wake.as_ref().map(|(session, _)| session)
            ],
        )? > 0;
        if changed {
            let doc_id: String = tx.query_row(
                "SELECT doc_id FROM owner_doc_reviews WHERE id = ?1",
                params![submission_id],
                |row| row.get(0),
            )?;
            for draft_id in &posted.draft_ids {
                tx.execute(
                    "DELETE FROM owner_doc_drafts WHERE doc_id = ?1 AND id = ?2",
                    params![doc_id, draft_id],
                )?;
            }
            if let Some((session_id, text)) = &posted.wake {
                crate::queue::enqueue_message_once_in_conn(
                    &tx,
                    &format!("owner-review-{submission_id}"),
                    session_id,
                    text,
                )?;
            }
        }
        let review = get_review_conn(&tx, submission_id)?.context("review row vanished")?;
        tx.commit()?;
        Ok((review, changed))
    }
}

const REVIEW_COLUMNS: &str = "id, status, pending_review_node_id, doc_id, commit_sha, blob_sha, \
     verdict, body, line_comment_count, file_comment_count, github_review_id, github_review_url, \
     submitted_at, delivered_to_session_id";
const DRAFT_COLUMNS: &str = "id, doc_id, commit_sha, line, quote, body, created_at, updated_at";

fn review_from_row(row: &Row<'_>) -> rusqlite::Result<OwnerDocReview> {
    Ok(OwnerDocReview {
        id: row.get(0)?,
        status: row.get(1)?,
        pending_review_node_id: row.get(2)?,
        doc_id: row.get(3)?,
        commit_sha: row.get(4)?,
        blob_sha: row.get(5)?,
        verdict: row.get(6)?,
        body: row.get(7)?,
        line_comment_count: row.get(8)?,
        file_comment_count: row.get(9)?,
        github_review_id: row.get(10)?,
        github_review_url: row.get(11)?,
        submitted_at: row.get(12)?,
        delivered_to_session_id: row.get(13)?,
    })
}

fn draft_from_row(row: &Row<'_>) -> rusqlite::Result<OwnerDocDraft> {
    Ok(OwnerDocDraft {
        id: row.get(0)?,
        doc_id: row.get(1)?,
        commit_sha: row.get(2)?,
        line: row.get(3)?,
        quote: row.get(4)?,
        body: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn drafts_conn(conn: &Connection, doc_id: &str) -> Result<Vec<OwnerDocDraft>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {DRAFT_COLUMNS} FROM owner_doc_drafts WHERE doc_id = ?1
         ORDER BY created_at, rowid"
    ))?;
    let rows = statement
        .query_map(params![doc_id], draft_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn get_draft_conn(
    conn: &Connection,
    doc_id: &str,
    draft_id: &str,
) -> Result<Option<OwnerDocDraft>> {
    Ok(conn
        .query_row(
            &format!("SELECT {DRAFT_COLUMNS} FROM owner_doc_drafts WHERE doc_id = ?1 AND id = ?2"),
            params![doc_id, draft_id],
            draft_from_row,
        )
        .optional()?)
}

fn get_review_conn(conn: &Connection, submission_id: &str) -> Result<Option<OwnerDocReview>> {
    Ok(conn
        .query_row(
            &format!("SELECT {REVIEW_COLUMNS} FROM owner_doc_reviews WHERE id = ?1"),
            params![submission_id],
            review_from_row,
        )
        .optional()?)
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buffer);
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

const DOC_COLUMNS: &str = "id, repo, path, pr_number, author_session_id, author_session_name, \
     title, note, retracted_at, created_at, updated_at";
const PUBLISH_COLUMNS: &str =
    "id, doc_id, commit_sha, blob_sha, session_id, review_requested, published_at";

fn doc_from_row(row: &Row<'_>) -> rusqlite::Result<OwnerDoc> {
    Ok(OwnerDoc {
        id: row.get(0)?,
        repo: row.get(1)?,
        path: row.get(2)?,
        pr_number: row.get(3)?,
        author_session_id: row.get(4)?,
        author_session_name: row.get(5)?,
        title: row.get(6)?,
        note: row.get(7)?,
        retracted_at: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn publish_from_row(row: &Row<'_>) -> rusqlite::Result<OwnerDocPublish> {
    Ok(OwnerDocPublish {
        id: row.get(0)?,
        doc_id: row.get(1)?,
        commit_sha: row.get(2)?,
        blob_sha: row.get(3)?,
        session_id: row.get(4)?,
        review_requested: row.get::<_, i64>(5)? != 0,
        published_at: row.get(6)?,
    })
}

fn get_doc_conn(conn: &Connection, doc_id: &str) -> Result<Option<OwnerDoc>> {
    Ok(conn
        .query_row(
            &format!("SELECT {DOC_COLUMNS} FROM owner_docs WHERE id = ?1"),
            params![doc_id],
            doc_from_row,
        )
        .optional()?)
}

fn generate_owner_doc_id(conn: &Connection) -> Result<String> {
    for _ in 0..16 {
        let mut bytes = [0u8; 4];
        OsRng.fill_bytes(&mut bytes);
        let id: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        if get_doc_conn(conn, &id)?.is_none() {
            return Ok(id);
        }
    }
    bail!("could not allocate a unique doc id")
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// Content-addressed file cache: `<root>/{repo}/{commit_sha}/{path}`.
#[derive(Debug, Clone)]
pub struct DocCache {
    root: PathBuf,
}

impl DocCache {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn entry_path(&self, repo: &str, commit_sha: &str, path: &str) -> Result<PathBuf> {
        validate_repo_slug(repo)?;
        validate_repo_path(path)?;
        if !is_full_commit_sha(commit_sha) {
            bail!("invalid commit sha {commit_sha:?}");
        }
        Ok(self.root.join(repo).join(commit_sha).join(path))
    }

    pub fn get(&self, repo: &str, commit_sha: &str, path: &str) -> Option<Vec<u8>> {
        let entry = self.entry_path(repo, commit_sha, path).ok()?;
        let bytes = fs::read(&entry).ok()?;
        // mtime records the last use, which drives pruning.
        if let Ok(file) = fs::File::options().write(true).open(&entry) {
            let _ = file.set_modified(SystemTime::now());
        }
        Some(bytes)
    }

    pub fn put(&self, repo: &str, commit_sha: &str, path: &str, bytes: &[u8]) -> Result<()> {
        let entry = self.entry_path(repo, commit_sha, path)?;
        let parent = entry.parent().context("cache entry has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let mut nonce = [0u8; 4];
        OsRng.fill_bytes(&mut nonce);
        let tmp = parent.join(format!(
            ".{}.{}.tmp",
            entry
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ));
        fs::write(&tmp, bytes).with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, &entry).with_context(|| format!("failed to move {}", entry.display()))?;
        Ok(())
    }

    /// Removes files unused for longer than `max_idle`, then empty directories.
    pub fn prune(&self, max_idle: Duration) -> Result<usize> {
        let cutoff = SystemTime::now()
            .checked_sub(max_idle)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if !self.root.exists() {
            return Ok(0);
        }
        prune_dir(&self.root, cutoff, true)
    }
}

fn prune_dir(dir: &Path, cutoff: SystemTime, is_root: bool) -> Result<usize> {
    let mut removed = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            removed += prune_dir(&path, cutoff, false)?;
        } else if entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| modified < cutoff)
        {
            fs::remove_file(&path)?;
            removed += 1;
        }
    }
    if !is_root && fs::read_dir(dir)?.next().is_none() {
        let _ = fs::remove_dir(dir);
    }
    Ok(removed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Html,
    Markdown,
    Text,
}

pub fn doc_kind(path: &str) -> DocKind {
    let extension = path
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, extension)| extension.to_ascii_lowercase());
    match extension.as_deref() {
        Some("html" | "htm") => DocKind::Html,
        Some("md" | "markdown") => DocKind::Markdown,
        _ => DocKind::Text,
    }
}

/// The page served by the doc reader, with `injection` (the review client)
/// just before `</body>`. HTML keeps every byte of the owner's document
/// apart from the inserted `data-sm-line` attributes; markdown carries the
/// same attributes from its source offsets.
pub fn render_doc_page(path: &str, title: &str, bytes: &[u8], injection: &str) -> Vec<u8> {
    use crate::owner_doc_render::{
        annotate_html_lines, find_body_end, inject_before_body_end, render_markdown_with_lines,
    };
    let shell = |body: String| {
        let page = doc_shell(title, &body).into_bytes();
        let body_end = find_body_end(&page);
        inject_before_body_end(page, body_end, injection)
    };
    match doc_kind(path) {
        DocKind::Html => {
            let annotated = annotate_html_lines(bytes);
            inject_before_body_end(annotated.bytes, annotated.body_end, injection)
        }
        DocKind::Markdown => shell(format!(
            "<article>\n{}</article>",
            render_markdown_with_lines(&String::from_utf8_lossy(bytes))
        )),
        DocKind::Text => shell(format!(
            "<pre>{}</pre>",
            escape_html(&String::from_utf8_lossy(bytes))
        )),
    }
}

fn doc_shell(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
:root {{ color-scheme: light dark; }}
body {{ margin: 0 auto; max-width: 46rem; padding: 1.5rem 1rem 4rem;
  font: 16px/1.6 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
pre, code {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.9em; }}
pre {{ overflow-x: auto; padding: 0.75rem; border-radius: 6px;
  background: color-mix(in srgb, currentColor 7%, transparent); white-space: pre; }}
body > pre {{ white-space: pre-wrap; word-break: break-word; }}
table {{ border-collapse: collapse; display: block; overflow-x: auto; }}
th, td {{ border: 1px solid color-mix(in srgb, currentColor 25%, transparent); padding: 0.3rem 0.6rem; }}
blockquote {{ margin-left: 0; padding-left: 1rem;
  border-left: 3px solid color-mix(in srgb, currentColor 25%, transparent); }}
img {{ max-width: 100%; }}
</style>
</head>
<body>
{body}
</body>
</html>
"#,
        title = escape_html(title)
    )
}

pub fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// Title when `--title` is not given: the HTML `<title>`, the first markdown
/// heading, or the file name.
pub fn default_doc_title(path: &str, bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let found = match doc_kind(path) {
        DocKind::Html => html_title(&text),
        DocKind::Markdown => text.lines().find_map(|line| {
            let heading = line.trim_start().strip_prefix('#')?;
            let heading = heading.trim_start_matches('#');
            heading
                .starts_with(' ')
                .then(|| heading.trim().trim_end_matches('#').trim().to_owned())
        }),
        DocKind::Text => None,
    };
    found
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path).to_owned())
}

fn html_title(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find("</title")?;
    let raw = text[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    Some(decode_basic_entities(&raw))
}

fn decode_basic_entities(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (OwnerDocStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "sm-owner-docs-{}-{}",
            std::process::id(),
            OsRng.next_u64()
        ));
        fs::create_dir_all(&dir).unwrap();
        (OwnerDocStore::new(dir.join("message_queue.db")), dir)
    }

    fn publish(pr: Option<i64>, commit: &str, blob: &str) -> PublishOwnerDoc {
        PublishOwnerDoc {
            repo: "acme/widgets".into(),
            path: "specs/memo.html".into(),
            pr_number: pr,
            session_id: "agent001".into(),
            session_name: Some("memo-writer".into()),
            title: "Memo".into(),
            note: None,
            commit_sha: commit.repeat(40),
            blob_sha: blob.repeat(40),
            review_requested: false,
        }
    }

    #[test]
    fn republishing_the_same_key_adds_a_publish_event_not_a_doc() {
        let (store, dir) = store();
        let first = store.publish(publish(Some(7), "a", "1"), |_| true).unwrap();
        assert!(first.created);
        let mut again = publish(Some(7), "b", "2");
        again.title = "Memo v2".into();
        again.note = Some("addressed comments".into());
        let second = store.publish(again, |_| true).unwrap();
        assert!(!second.created);
        assert_eq!(second.doc.id, first.doc.id);
        assert_eq!(second.doc.title, "Memo v2");
        assert_eq!(second.doc.note.as_deref(), Some("addressed comments"));
        assert_eq!(store.publishes(&first.doc.id).unwrap().len(), 2);

        // A different PR, or no PR, for the same path is a different doc.
        let other_pr = store.publish(publish(Some(8), "c", "3"), |_| true).unwrap();
        let no_pr = store.publish(publish(None, "d", "4"), |_| true).unwrap();
        let no_pr_again = store.publish(publish(None, "e", "4"), |_| true).unwrap();
        assert!(other_pr.created && no_pr.created && !no_pr_again.created);
        assert_ne!(other_pr.doc.id, first.doc.id);
        assert_ne!(no_pr.doc.id, other_pr.doc.id);
        assert_eq!(no_pr_again.doc.id, no_pr.doc.id);
        assert!(is_owner_doc_id(&first.doc.id));

        let summaries = store.summaries(None, false).unwrap();
        assert_eq!(summaries.len(), 3);
        let memo = summaries.iter().find(|s| s.doc.id == first.doc.id).unwrap();
        assert_eq!(memo.publish_count, 2);
        assert_eq!(memo.latest_commit_sha, "b".repeat(40));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn republish_takes_authorship_only_from_a_gone_author() {
        let (store, dir) = store();
        let first = store.publish(publish(Some(7), "a", "1"), |_| true).unwrap();
        let by = |session: &str, name: &str, commit: &str| {
            let mut request = publish(Some(7), commit, "1");
            request.session_id = session.into();
            request.session_name = Some(name.into());
            request
        };

        // A live author keeps the doc when another agent republishes it.
        let kept = store
            .publish(by("agent002", "helper", "b"), |author| {
                assert_eq!(author, "agent001");
                true
            })
            .unwrap();
        assert_eq!(kept.doc.author_session_id, "agent001");
        assert_eq!(kept.doc.author_session_name.as_deref(), Some("memo-writer"));

        // A retired or unknown author hands it to the republishing session.
        let moved = store
            .publish(by("agent003", "memo-writer-2", "c"), |_| false)
            .unwrap();
        assert_eq!(moved.doc.id, first.doc.id);
        assert_eq!(moved.doc.author_session_id, "agent003");
        assert_eq!(
            moved.doc.author_session_name.as_deref(),
            Some("memo-writer-2")
        );
        let summaries = store
            .summaries(Some(&["agent003".to_owned()].into()), false)
            .unwrap();
        assert_eq!(summaries.len(), 1);

        // The author republishing its own doc never asks about liveness.
        store
            .publish(by("agent003", "memo-writer-2", "d"), |_| {
                panic!("liveness checked for the author's own republish")
            })
            .unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn state_moves_new_read_updated_only_when_the_blob_changes() {
        let (store, dir) = store();
        let state = |id: &str| store.summary(id).unwrap().unwrap().state;
        let doc = store
            .publish(publish(Some(7), "a", "1"), |_| true)
            .unwrap()
            .doc;
        assert_eq!(state(&doc.id), OwnerDocState::New);
        store.record_view(&doc.id, &"1".repeat(40)).unwrap();
        assert_eq!(state(&doc.id), OwnerDocState::Read);
        // A push that doesn't touch the file: new commit, same blob.
        store.publish(publish(Some(7), "b", "1"), |_| true).unwrap();
        assert_eq!(state(&doc.id), OwnerDocState::Read);
        store.publish(publish(Some(7), "c", "2"), |_| true).unwrap();
        assert_eq!(state(&doc.id), OwnerDocState::Updated);
        store.record_view(&doc.id, &"2".repeat(40)).unwrap();
        assert_eq!(state(&doc.id), OwnerDocState::Read);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn state_rules_cover_review_requested_and_reviewed() {
        let publish = |blob: &str, at: &str, review_requested| OwnerDocPublish {
            id: 1,
            doc_id: "d".into(),
            commit_sha: "c".into(),
            blob_sha: blob.into(),
            session_id: "s".into(),
            review_requested,
            published_at: at.into(),
        };
        let first = publish("b1", "2026-09-24T10:00:00Z", true);
        let mut inputs = OwnerDocStateInputs {
            publishes: vec![&first],
            ..Default::default()
        };
        assert_eq!(
            derive_owner_doc_state(&inputs),
            OwnerDocState::ReviewRequested
        );
        inputs.posted_reviews = vec![("b1", "2026-09-24T11:00:00Z")];
        assert_eq!(derive_owner_doc_state(&inputs), OwnerDocState::Reviewed);
        // Asking again for the same blob is a new request.
        let again = publish("b1", "2026-09-24T12:00:00Z", true);
        inputs.publishes.push(&again);
        assert_eq!(
            derive_owner_doc_state(&inputs),
            OwnerDocState::ReviewRequested
        );
    }

    #[test]
    fn finishing_a_review_deletes_its_drafts_and_queues_one_wake() {
        let (store, dir) = store();
        let doc = store
            .publish(publish(Some(7), "a", "1"), |_| true)
            .unwrap()
            .doc;
        let (sha, other_sha) = ("a".repeat(40), "b".repeat(40));
        let draft = store
            .create_draft(&doc.id, &sha, Some(3), "the quote", "fix this")
            .unwrap();
        assert_eq!(draft.line, Some(3));
        let edited = store
            .update_draft(&doc.id, &draft.id, "fix this, please")
            .unwrap()
            .unwrap();
        assert_eq!(edited.body, "fix this, please");
        let later = store
            .create_draft(&doc.id, &other_sha, None, "", "later")
            .unwrap();
        assert!(store
            .update_draft("ffffffff", &draft.id, "x")
            .unwrap()
            .is_none());

        let (row, inserted) = store
            .begin_review(
                "sub-00000001",
                &doc.id,
                &sha,
                &"1".repeat(40),
                OwnerDocVerdict::Comment,
                None,
            )
            .unwrap();
        assert!(inserted);
        assert_eq!(row.status, "submitting");
        // A retry of the same submission gets the stored row back.
        let (_, inserted) = store
            .begin_review(
                "sub-00000001",
                &doc.id,
                &sha,
                &"1".repeat(40),
                OwnerDocVerdict::Approve,
                None,
            )
            .unwrap();
        assert!(!inserted);
        assert_eq!(store.submitting_reviews().unwrap().len(), 1);

        let posted = PostedOwnerDocReview {
            github_review_id: Some(99),
            github_review_url: "https://github.com/acme/widgets/pull/7#pullrequestreview-99".into(),
            line_comment_count: 1,
            file_comment_count: 0,
            draft_ids: vec![draft.id.clone()],
            wake: Some(("agent001".into(), "[sm review] ...".into())),
        };
        let (row, changed) = store.finish_review("sub-00000001", &posted).unwrap();
        assert!(changed);
        assert_eq!(row.status, "posted");
        assert_eq!(row.verdict, "comment");
        assert_eq!(row.delivered_to_session_id.as_deref(), Some("agent001"));
        let (_, changed) = store.finish_review("sub-00000001", &posted).unwrap();
        assert!(!changed, "a second finish is a no-op");
        assert_eq!(store.drafts(&doc.id).unwrap(), vec![later]);
        let conn = Connection::open(dir.join("message_queue.db")).unwrap();
        let wakes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_queue
                 WHERE id = 'owner-review-sub-00000001' AND target_session_id = 'agent001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(wakes, 1);
        let summary = store.summary(&doc.id).unwrap().unwrap();
        assert_eq!(summary.state, OwnerDocState::Reviewed);
        assert!(!summary.review_undelivered);

        // A review nobody could take is recorded as undelivered.
        store
            .begin_review(
                "sub-00000002",
                &doc.id,
                &sha,
                &"1".repeat(40),
                OwnerDocVerdict::Comment,
                None,
            )
            .unwrap();
        let undelivered = PostedOwnerDocReview {
            wake: None,
            draft_ids: Vec::new(),
            ..posted
        };
        store.finish_review("sub-00000002", &undelivered).unwrap();
        assert!(store.summary(&doc.id).unwrap().unwrap().review_undelivered);
        store
            .begin_review(
                "sub-00000003",
                &doc.id,
                &sha,
                "x",
                OwnerDocVerdict::Comment,
                None,
            )
            .unwrap();
        store.fail_review("sub-00000003").unwrap();
        assert_eq!(
            store.review("sub-00000003").unwrap().unwrap().status,
            "failed"
        );
        assert!(store
            .delete_draft(&doc.id, &store.drafts(&doc.id).unwrap()[0].id)
            .unwrap());
        assert!(store.drafts(&doc.id).unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retract_hides_until_republished() {
        let (store, dir) = store();
        let doc = store
            .publish(publish(None, "a", "1"), |_| true)
            .unwrap()
            .doc;
        let retracted = store.retract(&doc.id).unwrap().unwrap();
        assert!(retracted.retracted_at.is_some());
        assert!(store.summaries(None, false).unwrap().is_empty());
        assert_eq!(store.summaries(None, true).unwrap().len(), 1);
        store.publish(publish(None, "b", "1"), |_| true).unwrap();
        assert_eq!(store.summaries(None, false).unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn summaries_filter_by_author() {
        let (store, dir) = store();
        store.publish(publish(None, "a", "1"), |_| true).unwrap();
        let mut other = publish(Some(3), "b", "2");
        other.session_id = "agent002".into();
        store.publish(other, |_| true).unwrap();
        let authors = BTreeSet::from(["agent002".to_owned()]);
        let mine = store.summaries(Some(&authors), false).unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].doc.pr_number, Some(3));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_db_reads_as_empty() {
        let store = OwnerDocStore::new(std::env::temp_dir().join("sm-owner-docs-absent/q.db"));
        assert!(store.summaries(None, true).unwrap().is_empty());
        assert!(store.get("abcd1234").unwrap().is_none());
    }

    #[test]
    fn git_blob_sha_matches_git() {
        // `printf 'hello\n' | git hash-object --stdin`
        assert_eq!(
            git_blob_sha(b"hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
        // `git hash-object /dev/null`
        assert_eq!(
            git_blob_sha(b""),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
    }

    #[test]
    fn path_and_repo_validation_rejects_traversal() {
        assert!(validate_repo_path("specs/memo.html").is_ok());
        for bad in ["", "/etc/passwd", "../x", "a/../b", "a//b", "a/./b", "a\\b"] {
            assert!(validate_repo_path(bad).is_err(), "{bad}");
        }
        assert!(validate_repo_slug("rajeshgoli/session-manager").is_ok());
        for bad in ["noslash", "a/b/c", "../b", "a/..", "a b/c"] {
            assert!(validate_repo_slug(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn cache_round_trips_and_prunes_idle_entries() {
        let (_, dir) = store();
        let cache = DocCache::new(dir.join("doc_cache"));
        let sha = "a".repeat(40);
        assert!(cache.get("acme/widgets", &sha, "docs/memo.md").is_none());
        cache
            .put("acme/widgets", &sha, "docs/memo.md", b"# Memo\n")
            .unwrap();
        assert_eq!(
            cache.get("acme/widgets", &sha, "docs/memo.md").unwrap(),
            b"# Memo\n"
        );
        assert!(cache
            .put("acme/widgets", "HEAD", "docs/memo.md", b"")
            .is_err());
        assert!(cache.put("acme/widgets", &sha, "../escape", b"").is_err());
        assert_eq!(cache.prune(Duration::from_secs(3600)).unwrap(), 0);
        let entry = cache
            .root()
            .join("acme/widgets")
            .join(&sha)
            .join("docs/memo.md");
        fs::File::options()
            .write(true)
            .open(&entry)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(7200))
            .unwrap();
        assert_eq!(cache.prune(Duration::from_secs(3600)).unwrap(), 1);
        assert!(!cache.root().join("acme").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn renders_by_extension() {
        let client = "<script>client()</script>";
        let html = b"<html><body><p>as-is</p></body></html>";
        assert_eq!(
            render_doc_page("a/memo.HTML", "t", html, client),
            b"<html><body><p data-sm-line=\"1\">as-is</p><script>client()</script></body></html>"
        );
        let md = String::from_utf8(render_doc_page(
            "a.md",
            "T <1>",
            b"# Hi\n\n| a |\n|---|\n| b |\n",
            client,
        ))
        .unwrap();
        assert!(md.contains("<h1 data-sm-line=\"1\">Hi</h1>"), "{md}");
        assert!(md.contains("<table>"));
        assert!(md.contains("<title>T &lt;1&gt;</title>"));
        assert!(
            md.contains("</article>\n<script>client()</script></body>"),
            "{md}"
        );
        let text = String::from_utf8(render_doc_page(
            "notes.txt",
            "t",
            b"<script>x</script>",
            client,
        ))
        .unwrap();
        assert!(text.contains("<pre>&lt;script&gt;x&lt;/script&gt;</pre>"));
        assert!(
            text.contains("</pre>\n<script>client()</script></body>"),
            "{text}"
        );
    }

    #[test]
    fn default_titles() {
        assert_eq!(
            default_doc_title("m.html", b"<head><TITLE>\n  Decision &amp; memo </title>"),
            "Decision & memo"
        );
        assert_eq!(
            default_doc_title("m.md", b"intro\n## Readout: 1449 ##\n"),
            "Readout: 1449"
        );
        assert_eq!(default_doc_title("docs/m.md", b"#hashtag\n"), "m.md");
        assert_eq!(default_doc_title("a/b/log.txt", b"x"), "log.txt");
    }
}
