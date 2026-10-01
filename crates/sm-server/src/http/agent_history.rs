//! `GET /history/agents` (sm#1661): the agents that are no longer live,
//! newest first, for the Android History screen, whose job is restoring
//! them like `sm watch --restore`. Retired and stopped agents are both
//! listed (both restore); each row says whether this server can restore it
//! and carries the tickets, PRs and docs it worked on
//! (`HistoryData::agent_work`). Restore itself is `POST /sessions/{id}/restore`.

use super::*;
use crate::turn_messages::TurnMessage;
use crate::work_history::{AgentWork, HistoryData};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub(super) const AGENT_HISTORY_SCHEMA_VERSION: u32 = 1;
const DEFAULT_LIMIT: usize = 30;
const MAX_LIMIT: usize = 100;
/// Characters of the last turn message a History row carries.
const LAST_TURN_CHARS: usize = 300;

#[derive(Debug, Default, Deserialize)]
pub(super) struct AgentHistoryParams {
    /// Case-insensitive substring of the name, alias, role or working
    /// directory, or an id prefix; a number (`1768`, `#1768`) also matches
    /// the agents that claimed a ticket or PR with that number.
    #[serde(default)]
    q: Option<String>,
    /// The previous page's `next_before`.
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct AgentRow {
    id: String,
    name: String,
    provider: String,
    model: Option<String>,
    role: Option<String>,
    working_dir: String,
    node: String,
    parent_session_id: Option<String>,
    /// `retired` (ended by `sm retire` or `sm kill`) or `stopped` (a crash,
    /// a provider exit).
    state: &'static str,
    /// The auto-retire sweep retired it (sm#1839); History says
    /// "retired automatically".
    retired_automatically: bool,
    /// When it stopped: `stopped_at`, else `completed_at`, else
    /// `last_activity`, the order `sm watch --restore` sorts by.
    ended_at: String,
    /// Its last `sm status` text.
    last_status: Option<String>,
    /// What it wrote at the end of its latest turn, cut to 300 characters.
    last_turn: Option<TurnMessage>,
    restorable: bool,
    /// Why this server cannot restore it; null when `restorable`.
    unrestorable_reason: Option<String>,
    work: AgentWork,
}

#[derive(Debug, Serialize)]
struct AgentHistoryPage {
    schema_version: u32,
    agents: Vec<AgentRow>,
    /// The cursor for the next older page; null on the last page.
    next_before: Option<String>,
    /// Agents matching `q` across every page.
    total: usize,
}

/// `(ended_at, id)`: rows sort descending on the pair.
type Key = (i128, String);

fn ended_at(record: &SessionRecord) -> &str {
    [record.stopped_at.as_deref(), record.completed_at.as_deref()]
        .into_iter()
        .flatten()
        .find(|value| !value.trim().is_empty())
        .unwrap_or(&record.last_activity)
}

fn key(record: &SessionRecord) -> Key {
    let nanos = crate::work_history::parse_time(ended_at(record))
        .map_or(i128::MIN, time::OffsetDateTime::unix_timestamp_nanos);
    (nanos, record.id.clone())
}

fn encode_cursor(key: &Key) -> String {
    URL_SAFE_NO_PAD.encode(format!("{}|{}", key.0, key.1))
}

fn decode_cursor(value: &str) -> Option<Key> {
    let text = String::from_utf8(URL_SAFE_NO_PAD.decode(value.trim()).ok()?).ok()?;
    let (nanos, id) = text.split_once('|')?;
    Some((nanos.parse().ok()?, id.to_owned()))
}

fn display_name(record: &SessionRecord) -> String {
    record
        .cached_display_name()
        .or_else(|| record.friendly_name.clone())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| record.name.clone())
}

fn matches_query(record: &SessionRecord, query: &str) -> bool {
    let query = query.to_lowercase();
    record.id.starts_with(&query)
        || [
            Some(display_name(record)),
            Some(record.name.clone()),
            record.friendly_name.clone(),
            record.role.clone(),
            Some(record.working_dir.clone()),
        ]
        .into_iter()
        .flatten()
        .chain(record.aliases.iter().cloned())
        .any(|text| text.to_lowercase().contains(&query))
}

/// The checks `restore_core_session_with_runtime` makes without touching
/// tmux or the provider's files; a missing resume id surfaces on Restore.
fn unrestorable_reason(record: &SessionRecord) -> Option<String> {
    if !is_primary_node(&record.node) {
        return Some(format!(
            "Runs on node {}; this server restores only its own agents",
            record.node
        ));
    }
    if !matches!(record.provider.as_str(), "claude" | "codex" | "codex-fork") {
        return Some(format!("Provider {} cannot be restored", record.provider));
    }
    None
}

fn row(
    record: SessionRecord,
    work: &mut BTreeMap<String, AgentWork>,
    turns: &mut BTreeMap<String, TurnMessage>,
) -> AgentRow {
    let reason = unrestorable_reason(&record);
    AgentRow {
        last_turn: turns.remove(&record.id).map(|turn| TurnMessage {
            text: match turn.text.char_indices().nth(LAST_TURN_CHARS) {
                Some((cut, _)) => format!("{}…", &turn.text[..cut]),
                None => turn.text,
            },
            at: turn.at,
        }),
        name: display_name(&record),
        state: if record.is_retired() {
            "retired"
        } else {
            "stopped"
        },
        retired_automatically: record.auto_retired(),
        ended_at: ended_at(&record).to_owned(),
        last_status: record
            .agent_status_text
            .clone()
            .filter(|text| !text.trim().is_empty()),
        restorable: reason.is_none(),
        unrestorable_reason: reason,
        work: work.remove(&record.id).unwrap_or_default(),
        id: record.id,
        provider: record.provider,
        model: record.model,
        role: record.role,
        working_dir: record.working_dir,
        node: record.node,
        parent_session_id: record.parent_session_id,
    }
}

pub(super) async fn get_agent_history(
    State(state): State<Arc<AppState>>,
    Query(params): Query<AgentHistoryParams>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    if let Some(shell) = web::shell_page(&state, &request) {
        return Ok(shell);
    }
    let before = match params.before.as_deref().map(str::trim) {
        Some(cursor) if !cursor.is_empty() => {
            Some(decode_cursor(cursor).ok_or_else(|| ApiError::Status {
                status: StatusCode::BAD_REQUEST,
                detail: "Invalid before cursor".to_owned(),
            })?)
        }
        _ => None,
    };
    let query = params.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
    let data = HistoryData::load(&expand_home(&state.config.sm_send.db_path))?;
    // `1768` or `#1768` also finds the agents that claimed that number.
    let claimants = query
        .and_then(|q| q.trim_start_matches('#').parse::<i64>().ok())
        .map(|number| data.sessions_claiming(number))
        .unwrap_or_default();
    let mut records: Vec<SessionRecord> = state
        .session_store
        .list_sessions(true)?
        .into_iter()
        .filter(SessionRecord::is_stopped)
        .filter(|record| {
            query.is_none_or(|q| matches_query(record, q)) || claimants.contains(record.id.as_str())
        })
        .collect();
    let total = records.len();
    records.sort_by_cached_key(|record| std::cmp::Reverse(key(record)));
    if let Some(before) = &before {
        records.retain(|record| key(record) < *before);
    }
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let next_before = (records.len() > limit).then(|| encode_cursor(&key(&records[limit - 1])));
    records.truncate(limit);
    let mut work = data.agent_work(&records.iter().map(|r| r.id.as_str()).collect());
    let mut turns = state
        .session_store
        .turn_message_store()
        .map(|store| store.last_turns())
        .transpose()?
        .unwrap_or_default();
    let agents = records
        .into_iter()
        .map(|record| row(record, &mut work, &mut turns))
        .collect();
    Ok(Json(serde_json::to_value(AgentHistoryPage {
        schema_version: AGENT_HISTORY_SCHEMA_VERSION,
        agents,
        next_before,
        total,
    })?)
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips_and_rejects_garbage() {
        let key = (1_700_000_000_000_000_000_i128, "abc12345".to_owned());
        assert_eq!(decode_cursor(&encode_cursor(&key)), Some(key));
        assert_eq!(decode_cursor("not a cursor!"), None);
    }
}
