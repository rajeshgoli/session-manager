//! Analytics › Time (sm#1662, ticket #1678): what the agents' wall-clock
//! time went to, as a tree Root → Repo → Thread → Agent in seconds.
//!
//! An agent's timeline runs from its first recorded turn to its last (or to
//! now while it is live). Inside a turn, each instant goes to the innermost
//! tool span, else "model". Between turns it goes to the open wait that ends
//! last (queue job, Codex review, a question to the owner, child agents);
//! with none open, it is "parked" when the agent holds no claim, else "you"
//! or "idle" by who typed the prompt that ended the gap. Each instant is attributed to a thread by
//! `work_attribution`. See `specs/1662_analytics_redesign.html`,
//! appendices E and F.2.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
};

use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;
use time::OffsetDateTime;

use crate::analytics_spend::{
    format_nanos, open_read_only, table_exists, thread_meta, LegendEntry,
};
use crate::work_attribution::{nanos, Attribution, Attributor};

const NANOS_PER_MS: i128 = 1_000_000;
const HOUR_MS: i64 = 3_600_000;
/// A prompt within this long of an sm delivery was typed by sm.
const PROMPT_MATCH_MS: i64 = 20_000;
const UNKNOWN_REPO: &str = "unknown";
/// The usage ledger's bucket for transcripts bound to no agent. It merges
/// many sessions, so only its turns count: its gaps are not an agent's.
const UNASSIGNED_SEAT: &str = "unassigned";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TimeRange {
    Day,
    Week,
    Month,
}

impl TimeRange {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "24h" => Some(Self::Day),
            "7d" => Some(Self::Week),
            "30d" => Some(Self::Month),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "30d",
        }
    }

    fn millis(self) -> i64 {
        match self {
            Self::Day => 24 * HOUR_MS,
            Self::Week => 7 * 24 * HOUR_MS,
            Self::Month => 30 * 24 * HOUR_MS,
        }
    }
}

/// `parts` keys in display order.
const PARTS: [(&str, &str); 7] = [
    ("model", "Model working"),
    ("tools", "Tools"),
    ("queue", "Waiting on queue"),
    ("review", "Waiting on review"),
    ("you", "Waiting on you"),
    ("agents", "Waiting on agents"),
    ("idle", "Idle"),
];

/// Tool kinds as `activity_ledger` stores them (appendix D.3).
const TOOL_KINDS: [(&str, &str); 10] = [
    ("read", "Reading code"),
    ("edit", "Editing"),
    ("git", "Git & GitHub"),
    ("sm", "sm commands"),
    ("build", "Builds & tests inline"),
    ("shell", "Other shell"),
    ("subagent", "Subagents"),
    ("web", "Web & browser"),
    ("approval", "Approval checks"),
    ("other", "Other tools"),
];

/// Where the Time report reads from.
pub struct TimeSources<'a> {
    pub activity_db: &'a Path,
    pub usage_db: &'a Path,
    /// `message_queue.db`: claims, reviews, owner messages and deliveries.
    pub queue_db: &'a Path,
    /// `queue-runner/queue_runner.db`: queue jobs.
    pub queue_runner_db: &'a Path,
    /// Live (not stopped) sessions → whether each is in a turn right now.
    pub live_sessions: &'a BTreeMap<String, bool>,
    /// A `project_key` → its repo (`work_attribution::folder_repo`).
    pub repo_of: &'a dyn Fn(&str) -> String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimeReport {
    pub generated_at: String,
    pub range: &'static str,
    pub start: String,
    pub end: String,
    pub total: TimeTotal,
    pub parts_legend: Vec<LegendEntry>,
    pub tool_legend: Vec<LegendEntry>,
    pub root: TimeNode,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimeTotal {
    pub active_seconds: i64,
    pub parked_seconds: i64,
    /// Agents with active time in the range.
    pub agents: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimeNode {
    pub id: String,
    pub kind: &'static str,
    pub label: String,
    pub state: Option<String>,
    pub history_path: Option<String>,
    pub session_id: Option<String>,
    pub session_status: Option<&'static str>,
    /// The sum of `parts`.
    pub active_seconds: i64,
    pub parked_seconds: i64,
    /// Seconds per bucket; zero buckets are omitted.
    pub parts: BTreeMap<&'static str, i64>,
    /// `parts.tools` by tool kind; zero kinds are omitted.
    pub tools: BTreeMap<String, i64>,
    /// Turns started in the range; agents only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns: Option<i64>,
    /// Sorted by active time, then parked time, largest first.
    pub children: Vec<TimeNode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitKind {
    You,
    Review,
    Queue,
    Agents,
}

impl WaitKind {
    fn key(self) -> &'static str {
        match self {
            Self::You => "you",
            Self::Review => "review",
            Self::Queue => "queue",
            Self::Agents => "agents",
        }
    }

    /// Breaks a tie between waits that end together: you › review › queue ›
    /// agents.
    fn rank(self) -> u8 {
        match self {
            Self::You => 3,
            Self::Review => 2,
            Self::Queue => 1,
            Self::Agents => 0,
        }
    }
}

/// Something an idle agent waits on, over `[opened, closed)`; `None` is
/// still open.
#[derive(Debug, Clone, Copy)]
struct Wait {
    kind: WaitKind,
    opened: i64,
    closed: Option<i64>,
}

#[derive(Debug, Clone)]
struct Turn {
    started: i64,
    ended: i64,
    prompt_at: Option<i64>,
}

#[derive(Debug, Clone)]
struct Span {
    started: i64,
    ended: i64,
    kind: String,
}

/// Where one stretch of an agent's time went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bucket<'a> {
    Part(&'static str),
    Tool(&'a str),
    Parked,
}

/// One agent's recorded activity around a range. Times are Unix ms.
#[derive(Debug, Default)]
struct SeatActivity {
    /// First turn start over everything recorded.
    first_start: i64,
    /// Last turn end, or now while the agent is live.
    end: i64,
    /// Turns overlapping the range, by start.
    turns: Vec<Turn>,
    /// Tool spans overlapping the range, by start.
    spans: Vec<Span>,
    waits: Vec<Wait>,
}

/// Splits `seat`'s time in `[range_start, range_end)` into buckets and
/// hands each stretch `[from, to)` to `emit` with its thread.
fn classify(
    seat: &str,
    activity: &SeatActivity,
    (range_start, range_end): (i64, i64),
    attributor: &Attributor,
    folder_repo: &str,
    prompt_from_you: &dyn Fn(&Turn) -> bool,
    mut emit: impl FnMut(&Attribution, Bucket<'_>, i64, i64),
) {
    let lo = activity.first_start.max(range_start);
    let hi = activity.end.min(range_end);
    if lo >= hi {
        return;
    }
    // Overlapping turns (two transcripts at once) merge; a merged turn's
    // prompt is its first turn's.
    let mut merged: Vec<(i64, i64, bool)> = Vec::new();
    for turn in &activity.turns {
        if let Some(last) = merged.last_mut().filter(|last| turn.started <= last.1) {
            last.1 = last.1.max(turn.ended);
            continue;
        }
        merged.push((turn.started, turn.ended, prompt_from_you(turn)));
    }
    // Claim boundaries in ms, rounded up so each stretch starting at one
    // sees the claim state that holds for all of it.
    let claim_points: Vec<i64> = attributor
        .claim_boundaries(seat)
        .into_iter()
        .map(ceil_ms)
        .collect();
    let mut points = vec![lo, hi];
    for &(start, end, _) in &merged {
        points.extend([start, end]);
    }
    for span in &activity.spans {
        points.extend([span.started, span.ended]);
    }
    for wait in &activity.waits {
        points.push(wait.opened);
        points.extend(wait.closed);
    }
    points.extend(claim_points.iter().copied());
    points.retain(|&point| lo <= point && point <= hi);
    points.sort_unstable();
    points.dedup();

    let mut turn_index = 0;
    let mut span_index = 0;
    let mut open_spans: Vec<usize> = Vec::new();
    let mut attribution: Option<(usize, Attribution)> = None;
    for pair in points.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        while turn_index < merged.len() && merged[turn_index].1 <= from {
            turn_index += 1;
        }
        while span_index < activity.spans.len() && activity.spans[span_index].started <= from {
            open_spans.push(span_index);
            span_index += 1;
        }
        open_spans.retain(|&i| activity.spans[i].ended > from);
        let in_turn = merged
            .get(turn_index)
            .is_some_and(|&(start, _, _)| start <= from);
        if !in_turn && seat == UNASSIGNED_SEAT {
            continue;
        }
        let at = i128::from(from) * NANOS_PER_MS;
        let bucket = if in_turn {
            // The innermost span: latest start, then latest end.
            open_spans
                .iter()
                .max_by_key(|&&i| (activity.spans[i].started, activity.spans[i].ended, i))
                .map_or(Bucket::Part("model"), |&i| {
                    Bucket::Tool(activity.spans[i].kind.as_str())
                })
        } else if let Some(wait) = activity
            .waits
            .iter()
            .filter(|wait| wait.opened <= from && wait.closed.is_none_or(|closed| from < closed))
            .max_by_key(|wait| (wait.closed.unwrap_or(i64::MAX), wait.kind.rank()))
        {
            Bucket::Part(wait.kind.key())
        } else if !attributor.holds_claim_at(seat, at) {
            // No claim and nothing pending: finished, or never had work
            // (reviewers and scouts that do one job and sit unretired).
            Bucket::Parked
        } else if merged.get(turn_index).is_some_and(|turn| turn.2) {
            // The gap ends with a prompt the owner typed.
            Bucket::Part("you")
        } else {
            Bucket::Part("idle")
        };
        let segment = claim_points.partition_point(|&point| point <= from);
        if attribution
            .as_ref()
            .is_none_or(|(cached, _)| *cached != segment)
        {
            attribution = Some((segment, attributor.attribute(seat, at, folder_repo)));
        }
        let (_, attribution) = attribution.as_ref().expect("set above");
        emit(attribution, bucket, from, to);
    }
}

fn ceil_ms(at: i128) -> i64 {
    let ms = at.div_euclid(NANOS_PER_MS) + i128::from(at.rem_euclid(NANOS_PER_MS) != 0);
    i64::try_from(ms).unwrap_or(i64::MAX)
}

fn millis(value: &str) -> Option<i64> {
    nanos(value).and_then(|at| i64::try_from(at.div_euclid(NANOS_PER_MS)).ok())
}

/// Per-owner-prompt evidence: when the owner's inbox replies and all sm
/// deliveries reached each seat, sorted.
#[derive(Debug, Default)]
struct Deliveries {
    owner_replies: HashMap<String, Vec<i64>>,
    sm: HashMap<String, Vec<i64>>,
}

impl Deliveries {
    /// A prompt is the owner's when an inbox reply reached the seat within
    /// 20 s of it, or when no sm delivery did. sm types every delivery into
    /// the terminal, so the transcript alone cannot tell them apart.
    fn typed_by_you(&self, seat: &str, prompt_at: Option<i64>) -> bool {
        let Some(prompt_at) = prompt_at else {
            return false;
        };
        let near = |times: Option<&Vec<i64>>| {
            times.is_some_and(|times| {
                let from = times.partition_point(|&at| at < prompt_at - PROMPT_MATCH_MS);
                times
                    .get(from)
                    .is_some_and(|&at| at <= prompt_at + PROMPT_MATCH_MS)
            })
        };
        near(self.owner_replies.get(seat)) || !near(self.sm.get(seat))
    }
}

#[derive(Debug, Default)]
struct Leaf {
    parts: BTreeMap<&'static str, i64>,
    tools: BTreeMap<String, i64>,
    parked: i64,
    turns: i64,
}

impl Leaf {
    fn add(&mut self, bucket: Bucket<'_>, ms: i64) {
        match bucket {
            Bucket::Part(key) => *self.parts.entry(key).or_default() += ms,
            Bucket::Tool(kind) => *self.tools.entry(kind.to_owned()).or_default() += ms,
            Bucket::Parked => self.parked += ms,
        }
    }
}

type LeafKey = (String, Option<i64>, String);

/// Every agent's recorded activity around a range, ready to classify.
struct Timelines {
    attributor: Attributor,
    deliveries: Deliveries,
    /// `(seat, activity, folder repo)`, by seat.
    seats: Vec<(String, SeatActivity, String)>,
}

impl Timelines {
    /// Classifies every seat over `range`; `emit` gets the seat too.
    fn classify(
        &self,
        range: (i64, i64),
        mut emit: impl FnMut(&str, &Attribution, Bucket<'_>, i64, i64),
    ) {
        for (seat, activity, folder) in &self.seats {
            classify(
                seat,
                activity,
                range,
                &self.attributor,
                folder,
                &|turn| self.deliveries.typed_by_you(seat, turn.prompt_at),
                |attribution, bucket, from, to| emit(seat, attribution, bucket, from, to),
            );
        }
    }
}

/// Loads what `classify` needs for `[range_start, range_end)`; `None`
/// without an activity database.
fn load_timelines(
    sources: &TimeSources<'_>,
    (range_start, range_end): (i64, i64),
    now_ms: i64,
) -> Result<Option<Timelines>> {
    if !sources.activity_db.exists() {
        return Ok(None);
    }
    let activity = open_read_only(sources.activity_db)?;
    let bounds = load_seat_bounds(&activity)?;
    let mut turns = load_turns(&activity, range_start, range_end)?;
    let mut spans = load_spans(&activity, range_start, range_end)?;
    drop(activity);

    let usage = if sources.usage_db.exists() {
        open_read_only(sources.usage_db)?
    } else {
        Connection::open_in_memory()?
    };
    let attributor = Attributor::load(sources.queue_db, &usage)?;
    let folders = load_project_keys(&usage)?;
    drop(usage);
    let mut waits = load_waits(sources, range_start, range_end)?;
    // Waiting on agents: a child's first turn start to its last turn end,
    // still open while it is live and in a turn.
    for (child, &(first, last)) in &bounds {
        let Some(parent) = attributor.parent(child) else {
            continue;
        };
        let working = sources.live_sessions.get(child).copied().unwrap_or(false);
        waits.entry(parent.to_owned()).or_default().push(Wait {
            kind: WaitKind::Agents,
            opened: first,
            closed: (!working).then_some(last),
        });
    }
    let deliveries = load_deliveries(sources.queue_db, range_start - PROMPT_MATCH_MS)?;

    let mut repos: HashMap<String, String> = HashMap::new();
    let mut seats = Vec::new();
    for (seat, &(first_start, last_end)) in &bounds {
        let end = if sources.live_sessions.contains_key(seat) {
            last_end.max(now_ms)
        } else {
            last_end
        };
        if first_start >= range_end || end <= range_start {
            continue;
        }
        let seat_activity = SeatActivity {
            first_start,
            end,
            turns: turns.remove(seat).unwrap_or_default(),
            spans: spans.remove(seat).unwrap_or_default(),
            waits: waits.remove(seat).unwrap_or_default(),
        };
        let folder = match folders.get(seat) {
            Some(project_key) => repos
                .entry(project_key.clone())
                .or_insert_with(|| (sources.repo_of)(project_key))
                .clone(),
            None => UNKNOWN_REPO.to_owned(),
        };
        seats.push((seat.clone(), seat_activity, folder));
    }
    Ok(Some(Timelines {
        attributor,
        deliveries,
        seats,
    }))
}

pub fn time_report(
    sources: &TimeSources<'_>,
    range: TimeRange,
    now: OffsetDateTime,
) -> Result<TimeReport> {
    let now_ms = i64::try_from(now.unix_timestamp_nanos().div_euclid(NANOS_PER_MS))?;
    let (range_start, range_end) = (now_ms - range.millis(), now_ms);
    let report = |root: TimeNode, agents: usize| TimeReport {
        generated_at: format_nanos(now.unix_timestamp_nanos()),
        range: range.id(),
        start: format_nanos(i128::from(range_start) * NANOS_PER_MS),
        end: format_nanos(i128::from(range_end) * NANOS_PER_MS),
        total: TimeTotal {
            active_seconds: root.active_seconds,
            parked_seconds: root.parked_seconds,
            agents,
        },
        parts_legend: legend(&PARTS),
        tool_legend: legend(&TOOL_KINDS),
        root,
    };
    let Some(timelines) = load_timelines(sources, (range_start, range_end), now_ms)? else {
        return Ok(report(empty_node("root", "root", "All"), 0));
    };

    let mut leaves: BTreeMap<LeafKey, Leaf> = BTreeMap::new();
    timelines.classify(
        (range_start, range_end),
        |seat, attribution, bucket, from, to| {
            leaves
                .entry((
                    attribution.repo.clone(),
                    attribution.thread,
                    seat.to_owned(),
                ))
                .or_default()
                .add(bucket, to - from);
        },
    );
    for (seat, activity, folder) in &timelines.seats {
        for turn in &activity.turns {
            if range_start <= turn.started && turn.started < range_end {
                let attribution = timelines.attributor.attribute(
                    seat,
                    i128::from(turn.started) * NANOS_PER_MS,
                    folder,
                );
                leaves
                    .entry((attribution.repo, attribution.thread, seat.clone()))
                    .or_default()
                    .turns += 1;
            }
        }
    }

    let root = build_tree(leaves, &timelines.attributor, sources.live_sessions);
    let agents = count_active_agents(&root);
    Ok(report(root, agents))
}

/// One agent's stretch of a ticket's time over `[from, to)` in Unix ms.
/// `part` is a `PARTS` key; tool time is "tools".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadInterval {
    pub part: &'static str,
    pub from: i64,
    pub to: i64,
}

/// Every ticket's parts over `[from, to)` (the board's ticket clock,
/// sm#1710 appendix D7), as the Time report attributes them. Parked time
/// and time on no ticket are left out. Agents on one ticket at once give
/// overlapping intervals.
pub fn thread_intervals(
    sources: &TimeSources<'_>,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<BTreeMap<(String, i64), Vec<ThreadInterval>>> {
    let ms = |at: OffsetDateTime| i64::try_from(at.unix_timestamp_nanos().div_euclid(NANOS_PER_MS));
    let (from, to) = (ms(from)?, ms(to)?);
    let mut threads: BTreeMap<(String, i64), Vec<ThreadInterval>> = BTreeMap::new();
    let Some(timelines) = load_timelines(sources, (from, to), to)? else {
        return Ok(threads);
    };
    timelines.classify((from, to), |_, attribution, bucket, start, end| {
        let Some(thread) = attribution.thread else {
            return;
        };
        let part = match bucket {
            Bucket::Part(part) => part,
            Bucket::Tool(_) => "tools",
            Bucket::Parked => return,
        };
        let intervals = threads
            .entry((attribution.repo.clone(), thread))
            .or_default();
        match intervals.last_mut() {
            Some(last) if last.part == part && last.to == start => last.to = end,
            _ => intervals.push(ThreadInterval {
                part,
                from: start,
                to: end,
            }),
        }
    });
    Ok(threads)
}

fn legend(entries: &[(&str, &'static str)]) -> Vec<LegendEntry> {
    entries
        .iter()
        .map(|&(key, label)| LegendEntry {
            key: key.to_owned(),
            label,
        })
        .collect()
}

fn round_seconds(ms: i64) -> i64 {
    (ms + 500).div_euclid(1_000)
}

fn empty_node(id: &str, kind: &'static str, label: &str) -> TimeNode {
    TimeNode {
        id: id.to_owned(),
        kind,
        label: label.to_owned(),
        state: None,
        history_path: None,
        session_id: None,
        session_status: None,
        active_seconds: 0,
        parked_seconds: 0,
        parts: BTreeMap::new(),
        tools: BTreeMap::new(),
        turns: None,
        children: Vec::new(),
    }
}

fn build_tree(
    leaves: BTreeMap<LeafKey, Leaf>,
    attributor: &Attributor,
    live_sessions: &BTreeMap<String, bool>,
) -> TimeNode {
    let mut repos: BTreeMap<String, BTreeMap<Option<i64>, Vec<TimeNode>>> = BTreeMap::new();
    for ((repo, thread, seat), leaf) in leaves {
        let mut node = empty_node(
            &format!("a:{seat}"),
            "agent",
            attributor.name(&seat).unwrap_or(&seat),
        );
        // Seconds are rounded once per leaf; every parent is an exact sum.
        node.tools = leaf
            .tools
            .into_iter()
            .map(|(kind, ms)| (kind, round_seconds(ms)))
            .filter(|(_, seconds)| *seconds > 0)
            .collect();
        node.parts = leaf
            .parts
            .into_iter()
            .map(|(key, ms)| (key, round_seconds(ms)))
            .collect();
        let tools: i64 = node.tools.values().sum();
        node.parts.insert("tools", tools);
        node.parts.retain(|_, seconds| *seconds > 0);
        node.active_seconds = node.parts.values().sum();
        node.parked_seconds = round_seconds(leaf.parked);
        node.turns = Some(leaf.turns);
        if node.active_seconds == 0 && node.parked_seconds == 0 && leaf.turns == 0 {
            continue;
        }
        node.session_status = Some(if live_sessions.contains_key(&seat) {
            "running"
        } else {
            "stopped"
        });
        node.session_id = Some(seat);
        repos
            .entry(repo)
            .or_default()
            .entry(thread)
            .or_default()
            .push(node);
    }
    let repo_nodes = repos
        .into_iter()
        .map(|(repo, threads)| {
            let thread_nodes = threads
                .into_iter()
                .map(|(thread, agents)| {
                    let meta = thread_meta(attributor, &repo, thread);
                    let mut node = parent_node(&meta.id, "thread", &meta.label, agents);
                    node.state = meta.state;
                    node.history_path = meta.history_path;
                    node
                })
                .collect();
            let label = repo.rsplit('/').next().unwrap_or(&repo);
            parent_node(&format!("r:{repo}"), "repo", label, thread_nodes)
        })
        .collect();
    parent_node("root", "root", "All", repo_nodes)
}

fn parent_node(id: &str, kind: &'static str, label: &str, children: Vec<TimeNode>) -> TimeNode {
    let mut node = empty_node(id, kind, label);
    for child in &children {
        node.active_seconds += child.active_seconds;
        node.parked_seconds += child.parked_seconds;
        for (key, seconds) in &child.parts {
            *node.parts.entry(*key).or_default() += seconds;
        }
        for (kind, seconds) in &child.tools {
            *node.tools.entry(kind.clone()).or_default() += seconds;
        }
    }
    node.children = children;
    node.children.sort_by(|left, right| {
        right
            .active_seconds
            .cmp(&left.active_seconds)
            .then_with(|| right.parked_seconds.cmp(&left.parked_seconds))
            .then_with(|| left.id.cmp(&right.id))
    });
    node
}

fn count_active_agents(root: &TimeNode) -> usize {
    fn walk<'a>(node: &'a TimeNode, seats: &mut BTreeSet<&'a str>) {
        if node.kind == "agent" && node.active_seconds > 0 {
            seats.extend(node.session_id.as_deref());
        }
        for child in &node.children {
            walk(child, seats);
        }
    }
    let mut seats = BTreeSet::new();
    walk(root, &mut seats);
    seats.len()
}

/// Each seat's first turn start and last turn end over everything recorded.
/// Each of `seats`' last recorded turn end, Unix ms.
pub fn last_turn_ends(activity_db: &Path, seats: &[&str]) -> Result<BTreeMap<String, i64>> {
    let mut ends = BTreeMap::new();
    if seats.is_empty() || !activity_db.exists() {
        return Ok(ends);
    }
    let activity = open_read_only(activity_db)?;
    if !table_exists(&activity, "activity_turns")? {
        return Ok(ends);
    }
    let mut statement =
        activity.prepare("SELECT MAX(ended_at_ms) FROM activity_turns WHERE seat_id = ?1")?;
    for seat in seats {
        if let Some(end) = statement.query_row([seat], |row| row.get::<_, Option<i64>>(0))? {
            ends.insert((*seat).to_owned(), end);
        }
    }
    Ok(ends)
}

fn load_seat_bounds(activity: &Connection) -> Result<BTreeMap<String, (i64, i64)>> {
    if !table_exists(activity, "activity_turns")? {
        return Ok(BTreeMap::new());
    }
    let mut statement = activity.prepare(
        "SELECT seat_id, MIN(started_at_ms), MAX(ended_at_ms) FROM activity_turns
          GROUP BY seat_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?)))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn load_turns(activity: &Connection, from: i64, to: i64) -> Result<HashMap<String, Vec<Turn>>> {
    let mut turns: HashMap<String, Vec<Turn>> = HashMap::new();
    if !table_exists(activity, "activity_turns")? {
        return Ok(turns);
    }
    let mut statement = activity.prepare(
        "SELECT seat_id, started_at_ms, ended_at_ms, prompt_at_ms FROM activity_turns
          WHERE ended_at_ms > ?1 AND started_at_ms < ?2
          ORDER BY seat_id, started_at_ms, ended_at_ms",
    )?;
    let rows = statement.query_map(params![from, to], |row| {
        Ok((
            row.get::<_, String>(0)?,
            Turn {
                started: row.get(1)?,
                ended: row.get(2)?,
                prompt_at: row.get(3)?,
            },
        ))
    })?;
    for row in rows {
        let (seat, turn) = row?;
        turns.entry(seat).or_default().push(turn);
    }
    Ok(turns)
}

fn load_spans(activity: &Connection, from: i64, to: i64) -> Result<HashMap<String, Vec<Span>>> {
    let mut spans: HashMap<String, Vec<Span>> = HashMap::new();
    if !table_exists(activity, "activity_spans")? {
        return Ok(spans);
    }
    let mut statement = activity.prepare(
        "SELECT seat_id, started_at_ms, ended_at_ms, kind FROM activity_spans
          WHERE ended_at_ms > ?1 AND started_at_ms < ?2 AND ended_at_ms > started_at_ms
          ORDER BY seat_id, started_at_ms, ended_at_ms",
    )?;
    let rows = statement.query_map(params![from, to], |row| {
        Ok((
            row.get::<_, String>(0)?,
            Span {
                started: row.get(1)?,
                ended: row.get(2)?,
                kind: row.get(3)?,
            },
        ))
    })?;
    for row in rows {
        let (seat, span) = row?;
        spans.entry(seat).or_default().push(span);
    }
    Ok(spans)
}

/// Each seat's latest working folder (`project_key`) from `seat_meta`.
fn load_project_keys(usage: &Connection) -> Result<HashMap<String, String>> {
    let mut keys = HashMap::new();
    if !table_exists(usage, "seat_meta")? {
        return Ok(keys);
    }
    let mut statement = usage
        .prepare("SELECT seat_id, project_key FROM seat_meta ORDER BY seat_id, observed_at DESC")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (seat, project_key) = row?;
        keys.entry(seat).or_insert(project_key);
    }
    Ok(keys)
}

/// Queue, review and owner waits per seat that overlap `[from, to)`.
fn load_waits(sources: &TimeSources<'_>, from: i64, to: i64) -> Result<HashMap<String, Vec<Wait>>> {
    let mut waits: HashMap<String, Vec<Wait>> = HashMap::new();
    let mut add = |seats: &[Option<String>], kind: WaitKind, opened: i64, closed: Option<i64>| {
        if opened >= to || closed.is_some_and(|closed| closed <= from.max(opened)) {
            return;
        }
        let seats: BTreeSet<&str> = seats
            .iter()
            .flatten()
            .map(String::as_str)
            .filter(|seat| !seat.is_empty())
            .collect();
        for seat in seats {
            waits.entry(seat.to_owned()).or_default().push(Wait {
                kind,
                opened,
                closed,
            });
        }
    };

    if sources.queue_runner_db.exists() {
        let conn = open_read_only(sources.queue_runner_db)?;
        if table_exists(&conn, "queue_jobs")? {
            let mut statement = conn.prepare(
                "SELECT requester_session_id, notify_session_id, queued_at,
                        COALESCE(completion_notified_at, finished_at)
                   FROM queue_jobs WHERE type != 'service'",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?;
            for row in rows {
                let (requester, notify, queued, closed) = row?;
                let Some(opened) = millis(&queued) else {
                    continue;
                };
                add(
                    &[requester, notify],
                    WaitKind::Queue,
                    opened,
                    closed.as_deref().and_then(millis),
                );
            }
        }
    }

    if !sources.queue_db.exists() {
        return Ok(waits);
    }
    let conn = open_read_only(sources.queue_db)?;
    if table_exists(&conn, "codex_review_request_registrations")? {
        let mut statement = conn.prepare(
            "SELECT requester_session_id, notify_session_id, requested_at, review_landed_at,
                    superseded_at, is_active, last_polled_at
               FROM codex_review_request_registrations",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?;
        for row in rows {
            let (requester, notify, requested, landed, superseded, active, polled) = row?;
            let Some(opened) = millis(&requested) else {
                continue;
            };
            // An inactive registration that never landed stopped waiting at
            // its last poll (or at once, if it never polled).
            let closed = landed
                .or(superseded)
                .as_deref()
                .and_then(millis)
                .or_else(|| {
                    (active == Some(0))
                        .then(|| polled.as_deref().and_then(millis).unwrap_or(opened))
                });
            add(&[requester, notify], WaitKind::Review, opened, closed);
        }
    }
    if table_exists(&conn, "owner_messages")? {
        let replies = table_exists(&conn, "owner_message_replies")?;
        let sql = if replies {
            "SELECT message.sender_session_id, message.created_at,
                    (SELECT MIN(reply.created_at) FROM owner_message_replies AS reply
                      WHERE reply.message_id = message.id),
                    message.handled_at
               FROM owner_messages AS message"
        } else {
            "SELECT sender_session_id, created_at, NULL, handled_at FROM owner_messages"
        };
        let mut statement = conn.prepare(sql)?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (sender, created, replied, handled) = row?;
            let Some(opened) = millis(&created) else {
                continue;
            };
            let closed = replied
                .as_deref()
                .and_then(millis)
                .or_else(|| handled.as_deref().and_then(millis));
            add(&[Some(sender)], WaitKind::You, opened, closed);
        }
    }
    if table_exists(&conn, "owner_doc_publishes")? {
        let mut reviews: HashMap<String, Vec<i64>> = HashMap::new();
        if table_exists(&conn, "owner_doc_reviews")? {
            let mut statement =
                conn.prepare("SELECT doc_id, submitted_at FROM owner_doc_reviews")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (doc, submitted) = row?;
                if let Some(submitted) = millis(&submitted) {
                    reviews.entry(doc).or_default().push(submitted);
                }
            }
            for times in reviews.values_mut() {
                times.sort_unstable();
            }
        }
        let mut statement = conn.prepare(
            "SELECT session_id, doc_id, published_at, review_dismissed_at
               FROM owner_doc_publishes WHERE review_requested = 1",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (session, doc, published, dismissed) = row?;
            let Some(opened) = millis(&published) else {
                continue;
            };
            let reviewed = reviews
                .get(&doc)
                .and_then(|times| times.get(times.partition_point(|&at| at < opened)).copied());
            let closed = reviewed.or_else(|| dismissed.as_deref().and_then(millis));
            add(&[Some(session)], WaitKind::You, opened, closed);
        }
    }
    Ok(waits)
}

/// Owner inbox replies and sm deliveries at or after `since`.
fn load_deliveries(queue_db: &Path, since: i64) -> Result<Deliveries> {
    let mut deliveries = Deliveries::default();
    if !queue_db.exists() {
        return Ok(deliveries);
    }
    let conn = open_read_only(queue_db)?;
    let since_text = format_nanos(i128::from(since) * NANOS_PER_MS);
    // Stored times are RFC 3339 UTC, so text order is time order to the
    // second; the exact bound is applied after parsing.
    let bound = since_text.trim_end_matches('Z').to_owned();
    let read = |sql: &str, into: &mut HashMap<String, Vec<i64>>| -> Result<()> {
        let mut statement = conn.prepare(sql)?;
        let rows = statement.query_map([&bound], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (seat, at) = row?;
            if let Some(at) = millis(&at).filter(|&at| at >= since) {
                into.entry(seat).or_default().push(at);
            }
        }
        Ok(())
    };
    if table_exists(&conn, "owner_message_replies")? {
        read(
            "SELECT delivered_to_session_id, created_at FROM owner_message_replies
              WHERE created_at >= ?1",
            &mut deliveries.owner_replies,
        )?;
    }
    if table_exists(&conn, "message_queue")? {
        read(
            "SELECT target_session_id, delivered_at FROM message_queue
              WHERE delivered_at IS NOT NULL AND delivered_at >= ?1",
            &mut deliveries.sm,
        )?;
    }
    for times in deliveries
        .owner_replies
        .values_mut()
        .chain(deliveries.sm.values_mut())
    {
        times.sort_unstable();
    }
    Ok(deliveries)
}

#[cfg(test)]
mod tests;
