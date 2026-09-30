//! The ticket clock on the board payload (sm#1710 appendix D7): gathers
//! each in-progress or needs-you ticket's facts for `board::clock`.

use super::*;
use crate::analytics_time::ThreadInterval;
use crate::board::clock::{self, Activity, ClockFacts, ClockJob, JobPhase, CLOCK_HOURS};
use crate::board::model::{Board, Key, ModelInput, TicketState};
use crate::work_claims::{canonical_repo, HolderState};

/// `clock_hours`: 3, 6 or 24; 3 when absent.
pub(super) fn clock_hours(value: Option<i64>) -> Result<i64, ApiError> {
    match value {
        None => Ok(clock::DEFAULT_CLOCK_HOURS),
        Some(hours) if CLOCK_HOURS.contains(&hours) => Ok(hours),
        Some(_) => Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "clock_hours must be 3, 6 or 24".into(),
        }),
    }
}

fn parse_time(value: &str) -> Option<time::OffsetDateTime> {
    crate::queue::parse_queue_timestamp(value)
}

fn from_ms(at: i64) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(at) * 1_000_000).ok()
}

/// Each clocked ticket's clock object.
pub(super) fn clocks(
    state: &AppState,
    board: &Board,
    input: &ModelInput,
    hours: i64,
    now: time::OffsetDateTime,
) -> anyhow::Result<BTreeMap<Key, Value>> {
    let tickets: Vec<&Key> = board
        .facts
        .iter()
        .filter(|(_, facts)| matches!(facts.state, TicketState::InProgress | TicketState::NeedsYou))
        .map(|(key, _)| key)
        .collect();
    if tickets.is_empty() {
        return Ok(BTreeMap::new());
    }
    let stall = state.config.board.stall();
    let window = (now - time::Duration::hours(hours), now);
    let jobs = ticket_jobs(state, input, window.0)?;
    let session_list = state.session_store.list_sessions(true)?;
    let sessions: BTreeMap<&str, &SessionRecord> = session_list
        .iter()
        .map(|record| (record.id.as_str(), record))
        .collect();
    let paths = analytics::TimePaths::new(&state.config);
    let holders: Vec<&str> = tickets
        .iter()
        .filter_map(|key| board.facts[*key].holder.as_ref())
        .map(|holder| holder.session_id.as_str())
        .collect();
    let turn_ends = crate::analytics_time::last_turn_ends(paths.activity_db(), &holders)?;
    let reviews = RetainedQueueStore::list_active_codex_review_requests_from_path(&expand_home(
        &state.config.sm_send.db_path,
    ))?;
    let claim_store = claims::work_claim_store(state);

    let intervals = cached_intervals(state, hours, &session_list, &paths, window.0 - stall, now)?;

    let mut clocks = BTreeMap::new();
    for key in tickets {
        let facts = &board.facts[key];
        let holder = facts.holder.as_ref();
        let record = holder.and_then(|holder| sessions.get(holder.session_id.as_str()));
        let activity = holder.and_then(|holder| {
            let record = record.copied();
            let last_activity = record.and_then(|record| parse_time(&record.last_activity));
            match holder.state {
                HolderState::Working => {
                    let turn_start = record
                        .and_then(|record| record.activity_turn_start_hook_at.as_deref())
                        .and_then(parse_time);
                    Some((
                        Activity::Working,
                        turn_start.or(last_activity).unwrap_or(now),
                    ))
                }
                HolderState::Idle => {
                    // The Stop hook, or the last recorded turn end for agents
                    // without hooks.
                    let hook = record
                        .and_then(|record| record.activity_hook_at.as_deref())
                        .and_then(parse_time);
                    let turn_end = turn_ends.get(&holder.session_id).copied().and_then(from_ms);
                    let since = hook.max(turn_end).or(last_activity).unwrap_or(now);
                    Some((Activity::Idle, since))
                }
                HolderState::Stopped | HolderState::Retired => None,
            }
        });
        let no_agent_since = if activity.is_some() {
            None
        } else {
            let released = claim_store
                .claims_for_item(&key.0, key.1)?
                .iter()
                .filter_map(|claim| claim.ended_at.as_deref().and_then(parse_time))
                .max();
            let stopped = input
                .holders
                .get(key)
                .into_iter()
                .flatten()
                .filter_map(|holder| sessions.get(holder.session_id.as_str()))
                .filter_map(|record| record.stopped_at.as_deref().and_then(parse_time))
                .max();
            released.max(stopped)
        };
        let review = holder.and_then(|holder| {
            reviews
                .iter()
                .filter(|review| {
                    review.requester_session_id.as_deref() == Some(holder.session_id.as_str())
                        || review.notify_session_id == holder.session_id
                })
                .filter(|review| {
                    facts.prs.iter().any(|pr| {
                        pr.number == review.pr_number
                            && canonical_repo(&pr.repo) == canonical_repo(&review.repo)
                    })
                })
                .filter_map(|review| Some((review.pr_number, parse_time(&review.requested_at)?)))
                .max_by_key(|&(_, requested)| requested)
        });
        let clock_facts = ClockFacts {
            needs_you: facts.needs_you.as_ref().map(|needs_you| {
                (
                    parse_time(&needs_you.created_at).unwrap_or(now),
                    needs_you.text.clone(),
                )
            }),
            holder: activity,
            no_agent_since,
            jobs: jobs.get(key).cloned().unwrap_or_default(),
            review,
        };
        let ball = clock::ball(&clock_facts, now, stall);
        let mut ticket_intervals = intervals.get(key).cloned().unwrap_or_default();
        if let Some((Activity::Working, since)) = activity {
            // The recorder writes a turn when it ends, so the open turn is
            // not in the timeline yet.
            ticket_intervals.push(ThreadInterval {
                part: "model",
                from: clock::ms(since),
                to: clock::ms(now),
            });
        }
        let strip = clock::segments(&ticket_intervals, &clock_facts.jobs, window, stall);
        clocks.insert(
            key.clone(),
            clock::clock_json(&ball, clock::segments_json(&strip)),
        );
    }
    Ok(clocks)
}

pub(super) type ClockCache = BTreeMap<i64, (std::time::Instant, Arc<ThreadIntervals>)>;
type ThreadIntervals = BTreeMap<Key, Vec<ThreadInterval>>;
const CLOCK_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Every ticket's timeline over `[from, now)`, cached for 60 s per
/// `clock_hours`; the ball, the jobs and the open turn are read on every
/// request.
fn cached_intervals(
    state: &AppState,
    hours: i64,
    session_list: &[SessionRecord],
    paths: &analytics::TimePaths,
    from: time::OffsetDateTime,
    now: time::OffsetDateTime,
) -> anyhow::Result<Arc<ThreadIntervals>> {
    let hit = state.board_clock_cache.lock().ok().and_then(|cache| {
        cache
            .get(&hours)
            .filter(|(at, _)| at.elapsed() < CLOCK_CACHE_TTL)
            .map(|(_, intervals)| intervals.clone())
    });
    if let Some(intervals) = hit {
        return Ok(intervals);
    }
    let live = analytics::live_sessions(session_list);
    let intervals = Arc::new(crate::analytics_time::thread_intervals(
        &paths.sources(&live),
        from,
        now,
    )?);
    if let Ok(mut cache) = state.board_clock_cache.lock() {
        cache.insert(hours, (std::time::Instant::now(), intervals.clone()));
    }
    Ok(intervals)
}

/// Queue jobs by ticket (D7): active ones, and those that ended in the
/// window. A job's tickets are its `rank_tickets`, or else its requester's
/// active claims. Service jobs run for good and are left out.
fn ticket_jobs(
    state: &AppState,
    input: &ModelInput,
    window_start: time::OffsetDateTime,
) -> anyhow::Result<BTreeMap<Key, Vec<ClockJob>>> {
    let path = expand_home(&state.config.queue_runner_state_dir().to_string_lossy())
        .join("queue_runner.db");
    // Lexically comparable with both stored timestamp styles.
    let since = window_start.format(time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second]"
    ))?;
    let records = RetainedQueueStore::list_queue_jobs_from_path(
        &path,
        QueueJobFilters {
            finished_since: Some(since),
            ..Default::default()
        },
    )?;
    let ranks = crate::queue::queue_ticket_ranks(&expand_home(&state.config.sm_send.db_path));
    let positions: BTreeMap<String, usize> =
        crate::queue::pending_queue_job_consideration_order(&records, &ranks)
            .into_iter()
            .enumerate()
            .map(|(index, id)| (id, index + 1))
            .collect();
    let mut held: BTreeMap<String, Vec<Key>> = BTreeMap::new();
    for view in claims::work_claim_store(state).active_claims()? {
        let claim = view.claim;
        let repo = canonical_repo(&claim.repo);
        let numbers: Vec<i64> = if claim.kind == "pr" {
            input
                .pr_tickets
                .get(&(repo.clone(), claim.number))
                .into_iter()
                .flatten()
                .copied()
                .collect()
        } else {
            vec![claim.number]
        };
        held.entry(claim.session_id)
            .or_default()
            .extend(numbers.into_iter().map(|number| (repo.clone(), number)));
    }

    let mut jobs: BTreeMap<Key, Vec<ClockJob>> = BTreeMap::new();
    for record in &records {
        if record.job_type == "service" {
            continue;
        }
        let Some(queued_at) = parse_time(&record.queued_at) else {
            continue;
        };
        let phase = match record.state.as_str() {
            "pending" => JobPhase::Waiting,
            "running" => JobPhase::Running,
            _ => JobPhase::Ended,
        };
        let tickets: Vec<Key> = match record.rank_tickets.as_deref() {
            Some(tickets) if !tickets.is_empty() => tickets
                .iter()
                .map(|(repo, number)| (canonical_repo(repo), *number))
                .collect(),
            _ => record
                .requester_session_id
                .as_deref()
                .and_then(|session| held.get(session))
                .cloned()
                .unwrap_or_default(),
        };
        let job = ClockJob {
            job_type: record.job_type.clone(),
            phase,
            queued_at,
            started_at: record.started_at.as_deref().and_then(parse_time),
            finished_at: record.finished_at.as_deref().and_then(parse_time),
            timeout_seconds: record.timeout_seconds,
            holding_reason: record.holding_reason.clone(),
            position: positions.get(&record.id).copied(),
            // Quiet detection (D6.6) ships with the Queue page's server
            // part; rule 3a applies once it fills these.
            quiet_since: None,
            cpu_seconds: None,
        };
        for key in tickets {
            jobs.entry(key).or_default().push(job.clone());
        }
    }
    Ok(jobs)
}
