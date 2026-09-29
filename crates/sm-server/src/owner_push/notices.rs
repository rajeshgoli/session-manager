//! Owner notices (sm#1580): one phone notification per agent message and
//! per `--review` publish, delivered by the follow worker with the follow
//! rules (push, retry, email fallback), and closed without sending once the
//! owner has answered. Spec: `specs/1580_app_messages_replace_email.html`,
//! appendices C2 and F.

use super::*;

pub const NOTICE_MESSAGE: &str = "message";
pub const NOTICE_REVIEW_REQUESTED: &str = "review_requested";
/// The push that takes a shown notice's notification off the phone (sm#1643).
pub const NOTICE_WITHDRAW: &str = "withdraw";
/// Notices stay listed for the app this long.
pub const RECENT_NOTICE_WINDOW: Duration = Duration::days(7);
/// How far back the repair pass looks for subjects without a notice.
pub const NOTICE_REPAIR_WINDOW: Duration = Duration::hours(24);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notice {
    pub id: String,
    #[serde(skip)]
    pub user_id: String,
    pub kind: String,
    pub session_id: String,
    pub session_name: String,
    pub subject_id: String,
    pub title: String,
    pub body: String,
    pub reader_path: String,
    pub blocking: bool,
    pub created_at: String,
    pub notify_after: String,
    pub push_attempts: i64,
    pub last_push_error: Option<String>,
    pub notified_at: Option<String>,
    pub notified_via: Option<String>,
    pub acked_at: Option<String>,
    pub email_sent_at: Option<String>,
}

/// A notice to create. `title` and `body` are the notification's text
/// (appendix F3); the email subject joins them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewNotice {
    pub user_id: String,
    pub kind: String,
    pub session_id: String,
    pub session_name: String,
    pub subject_id: String,
    pub title: String,
    pub body: String,
    pub reader_path: String,
    pub blocking: bool,
}

impl NewNotice {
    /// A message: `{agent}` or `{agent} needs you`, then the message title.
    pub fn message(
        user_id: &str,
        session_id: &str,
        session_name: &str,
        message_id: &str,
        message_title: &str,
        blocking: bool,
    ) -> Self {
        Self {
            user_id: user_id.to_owned(),
            kind: NOTICE_MESSAGE.to_owned(),
            session_id: session_id.to_owned(),
            session_name: session_name.to_owned(),
            subject_id: message_id.to_owned(),
            title: if blocking {
                format!("{session_name} needs you")
            } else {
                session_name.to_owned()
            },
            body: message_title.to_owned(),
            reader_path: format!("/messages/{message_id}"),
            blocking,
        }
    }

    /// A `--review` publish: `{agent} asks for your review`, then the doc title.
    pub fn review_requested(
        user_id: &str,
        session_id: &str,
        session_name: &str,
        publish_id: i64,
        doc_title: &str,
        reader_path: &str,
    ) -> Self {
        Self {
            user_id: user_id.to_owned(),
            kind: NOTICE_REVIEW_REQUESTED.to_owned(),
            session_id: session_id.to_owned(),
            session_name: session_name.to_owned(),
            subject_id: publish_id.to_string(),
            title: format!("{session_name} asks for your review"),
            body: doc_title.to_owned(),
            reader_path: reader_path.to_owned(),
            blocking: false,
        }
    }
}

impl Notice {
    /// The FCM data payload (appendix F3); every value is a string.
    pub fn data(&self, unread_count: i64) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("kind".to_owned(), self.kind.clone()),
            ("notice_id".to_owned(), self.id.clone()),
            ("session_id".to_owned(), self.session_id.clone()),
            ("title".to_owned(), self.title.clone()),
            ("body".to_owned(), self.body.clone()),
            ("reader_path".to_owned(), self.reader_path.clone()),
            (
                "blocking".to_owned(),
                if self.blocking { "1" } else { "0" }.to_owned(),
            ),
            ("unread_count".to_owned(), unread_count.to_string()),
        ])
    }

    /// The fallback email's subject (appendix F4).
    pub fn email_subject(&self) -> String {
        format!("{}: {}", self.title, self.body)
    }
}

pub(super) const NOTICE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS owner_notices (
  id               TEXT PRIMARY KEY,
  user_id          TEXT NOT NULL,
  kind             TEXT NOT NULL,
  session_id       TEXT NOT NULL,
  session_name     TEXT NOT NULL,
  subject_id       TEXT NOT NULL,
  title            TEXT NOT NULL,
  body             TEXT NOT NULL,
  reader_path      TEXT NOT NULL,
  blocking         INTEGER NOT NULL DEFAULT 0,
  created_at       TEXT NOT NULL,
  notify_after     TEXT NOT NULL,
  push_attempts    INTEGER NOT NULL DEFAULT 0,
  last_push_error  TEXT,
  notified_at      TEXT,
  notified_via     TEXT,
  acked_at         TEXT,
  email_sent_at    TEXT
);
CREATE INDEX IF NOT EXISTS owner_notices_pending ON owner_notices(notified_at, notify_after);
CREATE UNIQUE INDEX IF NOT EXISTS owner_notices_subject ON owner_notices(kind, subject_id);
-- A shown notice whose notification the phone was told to remove (sm#1643).
CREATE TABLE IF NOT EXISTS owner_notice_withdrawals (
  notice_id     TEXT PRIMARY KEY,
  withdrawn_at  TEXT NOT NULL
);
"#;

const NOTICE_COLUMNS: &str = "id, user_id, kind, session_id, session_name, subject_id, title, \
     body, reader_path, blocking, created_at, notify_after, push_attempts, last_push_error, \
     notified_at, notified_via, acked_at, email_sent_at";

fn notice_from_row(row: &Row<'_>) -> rusqlite::Result<Notice> {
    Ok(Notice {
        id: row.get(0)?,
        user_id: row.get(1)?,
        kind: row.get(2)?,
        session_id: row.get(3)?,
        session_name: row.get(4)?,
        subject_id: row.get(5)?,
        title: row.get(6)?,
        body: row.get(7)?,
        reader_path: row.get(8)?,
        blocking: row.get::<_, i64>(9)? != 0,
        created_at: row.get(10)?,
        notify_after: row.get(11)?,
        push_attempts: row.get(12)?,
        last_push_error: row.get(13)?,
        notified_at: row.get(14)?,
        notified_via: row.get(15)?,
        acked_at: row.get(16)?,
        email_sent_at: row.get(17)?,
    })
}

impl OwnerPushStore {
    /// Creates the subject's notice unless it has one. Returns whether this
    /// call created it; a racing create and repair leave one row.
    pub fn create_notice(&self, notice: &NewNotice, now: OffsetDateTime) -> Result<bool> {
        let conn = self.open()?;
        for _ in 0..16 {
            let id = format!("not_{}", random_base32(12));
            let now = format_ts(now);
            match conn.execute(
                "INSERT INTO owner_notices (id, user_id, kind, session_id, session_name, \
                   subject_id, title, body, reader_path, blocking, created_at, notify_after) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11) \
                 ON CONFLICT(kind, subject_id) DO NOTHING",
                params![
                    id,
                    notice.user_id,
                    notice.kind,
                    notice.session_id,
                    notice.session_name,
                    notice.subject_id,
                    notice.title,
                    notice.body,
                    notice.reader_path,
                    notice.blocking,
                    now
                ],
            ) {
                Ok(changed) => return Ok(changed > 0),
                // Only the primary key can still collide: pick another id.
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::ConstraintViolation => {}
                Err(error) => return Err(error.into()),
            }
        }
        anyhow::bail!("could not allocate a unique notice id")
    }

    pub fn notice(&self, id: &str) -> Result<Option<Notice>> {
        let conn = self.open()?;
        Ok(conn
            .query_row(
                &format!("SELECT {NOTICE_COLUMNS} FROM owner_notices WHERE id = ?1"),
                params![id],
                notice_from_row,
            )
            .optional()?)
    }

    pub fn notice_for_subject(&self, kind: &str, subject_id: &str) -> Result<Option<Notice>> {
        let conn = self.open()?;
        Ok(conn
            .query_row(
                &format!(
                    "SELECT {NOTICE_COLUMNS} FROM owner_notices WHERE kind = ?1 AND subject_id = ?2"
                ),
                params![kind, subject_id],
                notice_from_row,
            )
            .optional()?)
    }

    /// The phone showed the notice. Returns false when it isn't the owner's.
    pub fn ack_notice(&self, user_id: &str, id: &str, now: OffsetDateTime) -> Result<bool> {
        let conn = self.open()?;
        let exists = conn
            .query_row(
                "SELECT 1 FROM owner_notices WHERE id = ?1 AND user_id = ?2",
                params![id, user_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if exists {
            conn.execute(
                "UPDATE owner_notices SET acked_at = ?2 WHERE id = ?1 AND acked_at IS NULL",
                params![id, format_ts(now)],
            )?;
        }
        Ok(exists)
    }

    /// The owner's notices from the last 7 days, newest first.
    pub fn list_notices(&self, user_id: &str, now: OffsetDateTime) -> Result<Vec<Notice>> {
        let cutoff = format_ts(now - RECENT_NOTICE_WINDOW);
        self.query_notices(
            "WHERE user_id = ?1 AND created_at >= ?2 ORDER BY created_at DESC, rowid DESC",
            params![user_id, cutoff],
        )
    }

    fn pending_notices(&self, now: OffsetDateTime) -> Result<Vec<Notice>> {
        self.query_notices(
            "WHERE notified_at IS NULL AND notify_after <= ?1 ORDER BY created_at, rowid",
            params![format_ts(now)],
        )
    }

    fn notice_ack_fallback_due(&self, now: OffsetDateTime) -> Result<Vec<Notice>> {
        Ok(self
            .query_notices(
                "WHERE notified_via = 'push' AND acked_at IS NULL AND email_sent_at IS NULL \
                 ORDER BY created_at, rowid",
                params![],
            )?
            .into_iter()
            .filter(|notice| {
                notice
                    .notified_at
                    .as_deref()
                    .and_then(parse_ts)
                    .is_some_and(|notified_at| notified_at + ACK_FALLBACK <= now)
                    && parse_ts(&notice.notify_after).is_none_or(|retry_at| retry_at <= now)
            })
            .collect())
    }

    /// Notices the phone showed (acknowledged) in the last 7 days and has
    /// not been told to remove.
    fn shown_notices(&self, now: OffsetDateTime) -> Result<Vec<Notice>> {
        self.query_notices(
            "WHERE acked_at IS NOT NULL AND created_at >= ?1 \
               AND id NOT IN (SELECT notice_id FROM owner_notice_withdrawals) \
             ORDER BY created_at, rowid",
            params![format_ts(now - RECENT_NOTICE_WINDOW)],
        )
    }

    fn record_withdrawal(&self, id: &str, now: OffsetDateTime) -> Result<()> {
        self.open()?.execute(
            "INSERT OR IGNORE INTO owner_notice_withdrawals (notice_id, withdrawn_at) \
             VALUES (?1, ?2)",
            params![id, format_ts(now)],
        )?;
        Ok(())
    }

    fn save_notice_delivery(&self, notice: &Notice) -> Result<()> {
        self.open()?.execute(
            "UPDATE owner_notices SET notify_after = ?2, push_attempts = ?3, \
               last_push_error = ?4, notified_at = ?5, notified_via = ?6, email_sent_at = ?7 \
             WHERE id = ?1",
            params![
                notice.id,
                notice.notify_after,
                notice.push_attempts,
                notice.last_push_error,
                notice.notified_at,
                notice.notified_via,
                notice.email_sent_at,
            ],
        )?;
        Ok(())
    }

    fn query_notices(&self, clause: &str, params: impl rusqlite::Params) -> Result<Vec<Notice>> {
        let conn = self.open()?;
        let mut statement = conn.prepare(&format!(
            "SELECT {NOTICE_COLUMNS} FROM owner_notices {clause}"
        ))?;
        let rows = statement
            .query_map(params, notice_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

fn random_base32(len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut bytes = vec![0u8; len];
    OsRng.fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|byte| ALPHABET[usize::from(byte & 31)] as char)
        .collect()
}

/// The rest of the server, as notice delivery sees it.
pub trait NoticeWorld {
    /// Whether the notice's subject still wants the owner (appendix F2): a
    /// message still new, read or needs-you; a review request still the
    /// doc's latest publish and still requested.
    fn still_wanted(&self, notice: &Notice) -> Result<bool>;
    /// Whether the owner opened the notice's subject after the notice was
    /// created: the message, or the requested revision of the doc.
    fn opened(&self, notice: &Notice) -> Result<bool>;
    /// The sender's unread messages to the owner, for `unread_count`.
    fn unread_count(&self, notice: &Notice) -> Result<i64>;
    /// Every message and review-requesting publish created since `since`,
    /// as notices: the repair pass inserts the missing ones.
    fn notice_candidates(&self, since: OffsetDateTime) -> Result<Vec<NewNotice>>;
}

pub trait NoticeMailer {
    fn send(&self, notice: &Notice) -> Result<(), MailError>;
}

/// Creates the notices a failed insert left missing. A subject no longer
/// wanted still gets its row, which delivery closes at once.
pub fn repair_notices(
    store: &OwnerPushStore,
    world: &dyn NoticeWorld,
    now: OffsetDateTime,
) -> Result<usize> {
    let mut created = 0;
    for candidate in world.notice_candidates(now - NOTICE_REPAIR_WINDOW)? {
        created += usize::from(store.create_notice(&candidate, now)?);
    }
    Ok(created)
}

/// Sends due notices and their unacknowledged-push email fallback, with the
/// follow rules. Returns one line per problem worth logging.
pub fn deliver_notices(
    store: &OwnerPushStore,
    world: &dyn NoticeWorld,
    sender: Option<&dyn PushSender>,
    mailer: &dyn NoticeMailer,
    now: OffsetDateTime,
) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    for mut notice in store.pending_notices(now)? {
        if !world.still_wanted(&notice)? {
            close_resolved(&mut notice, now);
            store.save_notice_delivery(&notice)?;
            continue;
        }
        let tokens = match sender {
            Some(_) => store.valid_tokens(&notice.user_id)?,
            None => Vec::new(),
        };
        let mut use_email = sender.is_none() || tokens.is_empty();
        if let (Some(sender), false) = (sender, use_email) {
            let unread = if notice.kind == NOTICE_MESSAGE {
                world.unread_count(&notice)?
            } else {
                0
            };
            let data = notice.data(unread);
            let mut delivered = false;
            let mut retryable = Vec::new();
            for token in &tokens {
                match sender.send(&token.token, &data) {
                    Ok(()) => delivered = true,
                    Err(PushError::InvalidToken(detail)) => {
                        store.record_token_error(token, &detail, Some(now))?;
                    }
                    Err(PushError::Retryable(detail)) => {
                        store.record_token_error(token, &detail, None)?;
                        retryable.push(format!("{}: {detail}", token.device_name));
                    }
                }
            }
            if delivered {
                notice.notified_at = Some(format_ts(now));
                notice.notified_via = Some("push".to_owned());
                notice.last_push_error = retryable.first().cloned();
            } else if retryable.is_empty() {
                use_email = true;
            } else {
                notice.push_attempts += 1;
                notice.last_push_error = Some(retryable.join("; "));
                match usize::try_from(notice.push_attempts - 1)
                    .ok()
                    .and_then(|index| PUSH_RETRY_DELAYS_SECONDS.get(index))
                {
                    Some(delay) => notice.notify_after = format_ts(now + Duration::seconds(*delay)),
                    None => use_email = true,
                }
            }
        }
        if use_email {
            let due_since = parse_ts(&notice.created_at);
            match mailer.send(&notice) {
                Ok(()) => {}
                Err(MailError::Transient(detail)) if within_email_retry(due_since, now) => {
                    problems.push(format!(
                        "notice {} email failed, retrying: {detail}",
                        notice.id
                    ));
                    notice.last_push_error = Some(format!("email failed: {detail}"));
                    notice.notify_after = format_ts(now + EMAIL_RETRY_DELAY);
                    store.save_notice_delivery(&notice)?;
                    continue;
                }
                Err(error) => {
                    problems.push(format!("notice {} email failed: {error}", notice.id));
                    notice.last_push_error = Some(match (&error, sender) {
                        (MailError::Unavailable(_), None) => "no channel".to_owned(),
                        _ => format!("email failed: {error}"),
                    });
                }
            }
            notice.notified_at = Some(format_ts(now));
            notice.notified_via = Some("email".to_owned());
            notice.email_sent_at = Some(format_ts(now));
        }
        store.save_notice_delivery(&notice)?;
    }
    for mut notice in store.notice_ack_fallback_due(now)? {
        if !world.still_wanted(&notice)? {
            close_resolved(&mut notice, now);
            store.save_notice_delivery(&notice)?;
            continue;
        }
        let due_since = notice
            .notified_at
            .as_deref()
            .and_then(parse_ts)
            .map(|notified_at| notified_at + ACK_FALLBACK);
        match mailer.send(&notice) {
            Ok(()) => {}
            Err(MailError::Transient(detail)) if within_email_retry(due_since, now) => {
                problems.push(format!(
                    "notice {} fallback email failed, retrying: {detail}",
                    notice.id
                ));
                notice.last_push_error = Some(format!("email failed: {detail}"));
                notice.notify_after = format_ts(now + EMAIL_RETRY_DELAY);
                store.save_notice_delivery(&notice)?;
                continue;
            }
            Err(error) => {
                problems.push(format!(
                    "notice {} fallback email failed: {error}",
                    notice.id
                ));
                notice.last_push_error = Some(format!("email failed: {error}"));
            }
        }
        notice.email_sent_at = Some(format_ts(now));
        store.save_notice_delivery(&notice)?;
    }
    Ok(problems)
}

/// Tells the phone to remove each shown notification the owner no longer
/// needs: its subject was answered or opened (sm#1643). The app removes the
/// notification only while it still shows that notice, so a newer notice from
/// the same agent stays. A push that fails retryably is tried on the next pass.
pub fn withdraw_notices(
    store: &OwnerPushStore,
    world: &dyn NoticeWorld,
    sender: Option<&dyn PushSender>,
    now: OffsetDateTime,
) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let Some(sender) = sender else {
        return Ok(problems);
    };
    for notice in store.shown_notices(now)? {
        if world.still_wanted(&notice)? && !world.opened(&notice)? {
            continue;
        }
        let data = BTreeMap::from([
            ("kind".to_owned(), NOTICE_WITHDRAW.to_owned()),
            ("notice_id".to_owned(), notice.id.clone()),
        ]);
        let mut retry = false;
        for token in store.valid_tokens(&notice.user_id)? {
            match sender.send(&token.token, &data) {
                Ok(()) => {}
                Err(PushError::InvalidToken(detail)) => {
                    store.record_token_error(&token, &detail, Some(now))?;
                }
                Err(PushError::Retryable(detail)) => {
                    store.record_token_error(&token, &detail, None)?;
                    problems.push(format!("notice {} withdrawal failed: {detail}", notice.id));
                    retry = true;
                }
            }
        }
        if !retry {
            store.record_withdrawal(&notice.id, now)?;
        }
    }
    Ok(problems)
}

/// The owner already answered: nothing is sent, now or as a fallback.
fn close_resolved(notice: &mut Notice, now: OffsetDateTime) {
    if notice.notified_at.is_none() {
        notice.notified_at = Some(format_ts(now));
    }
    notice.notified_via = Some("resolved".to_owned());
}
