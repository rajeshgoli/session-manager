//! The Board row (sm#2249): board events that reached the owner's phone
//! ("Ready in lane 3…", "Lane done…", "Started #n…") collected into one
//! Inbox row, so a ping is findable without one row per lane. The row is
//! unread while a pinged event is newer than both the owner's last look at
//! the Board and their last read of this row. Opening it on the web goes to
//! the newest event's lane; `GET /inbox/thread/board` lists the events for
//! clients that open every row as a thread.

use super::*;

pub(crate) const BOARD_THREAD: &str = "board";

/// Pinged board notices from the last 7 days, newest first.
fn pinged(state: &AppState) -> Result<Vec<crate::owner_push::Notice>, ApiError> {
    let owner = super::super::follows::follow_owner_id(&state.config, None);
    Ok(super::super::follows::push_store(state)
        .list_notices(&owner, OffsetDateTime::now_utc())?
        .into_iter()
        .filter(|notice| {
            notice.kind.starts_with("board_") && notice.notified_via.as_deref() == Some("push")
        })
        .collect())
}

fn line(notice: &crate::owner_push::Notice) -> String {
    if notice.body.is_empty() || notice.body == notice.title {
        notice.title.clone()
    } else {
        format!("{} · {}", notice.title, notice.body)
    }
}

/// The Board row, or none when no board event pinged the owner this week.
pub(super) fn board_row(state: &AppState) -> Result<Option<InboxRow>, ApiError> {
    let notices = pinged(state)?;
    let Some(newest) = notices.first() else {
        return Ok(None);
    };
    let owner = super::super::follows::follow_owner_id(&state.config, None);
    let board_seen = super::super::board::board_store(state)
        .seen(&owner)?
        .map(|(at, _)| norm(&at));
    let row_read = inbox_store(state)
        .marks()?
        .remove(BOARD_THREAD)
        .and_then(|marks| marks.last_read_at)
        .map(|at| norm(&at));
    let seen = board_seen.max(row_read);
    let unread = notices
        .iter()
        .filter(|notice| {
            seen.as_deref()
                .is_none_or(|seen| norm(&notice.created_at).as_str() > seen)
        })
        .count();
    Ok(Some(InboxRow {
        thread_key: BOARD_THREAD.to_owned(),
        kind: "board",
        title: "Board".to_owned(),
        repo: String::new(),
        status: "live".to_owned(),
        verdict: None,
        pr_number: None,
        author: None,
        group: if unread > 0 { "new" } else { "earlier" },
        preview: line(newest),
        newest_at: norm(&newest.created_at),
        message_count: notices.len(),
        revision_count: 0,
        doc_count: 0,
        open_asks: 0,
        url: newest.reader_path.clone(),
        done: false,
        session_id: None,
        doc_id: None,
        doc_url: None,
        folded_by: None,
        agents: vec![format!(
            "{} board {}",
            notices.len(),
            if notices.len() == 1 {
                "update"
            } else {
                "updates"
            }
        )],
        items: notices.len(),
    }))
}

/// The Board row as a thread: each pinged event, linking to its lane.
pub(super) fn board_thread(state: &AppState) -> Result<Response, ApiError> {
    let notices = pinged(state)?;
    inbox_store(state).mark_read(BOARD_THREAD, 0)?;
    let items: Vec<Value> = notices
        .iter()
        .rev()
        .map(|notice| {
            let text = line(notice);
            json!({"kind": "event", "at": norm(&notice.created_at), "text": text,
                "link": notice.reader_path,
                "html": format!("<div class=\"b ev\"><a href=\"{}\">{}</a></div>",
                    escape_html(&notice.reader_path), escape_html(&text))})
        })
        .collect();
    Ok(Json(
        json!({"thread_key": BOARD_THREAD, "title": "Board", "status": "live",
        "repo": "", "can_send": false, "reply_to": null, "reply_options": [],
        "items": items, "review_asks": []}),
    )
    .into_response())
}
