//! The board model (appendix D): lanes, ticket states, warnings, chains and
//! row order, as a pure function of the board tables, the active ticket
//! claims and the records of agents waiting on the owner.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use time::OffsetDateTime;

use crate::work_claims::HolderState;

/// A ticket: `(owner/name, number)`, the repo in canonical lower case.
pub type Key = (String, i64);

/// How long a ticket that joined a lane carries the "new" mark.
pub const NEW_MARK: time::Duration = time::Duration::hours(24);

/// A `board_items` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub repo: String,
    pub number: i64,
    pub title: String,
    pub url: String,
    /// `open` or `closed`.
    pub state: String,
    pub state_reason: Option<String>,
    pub closed_at: Option<String>,
}

impl Item {
    pub fn key(&self) -> Key {
        (self.repo.clone(), self.number)
    }

    pub fn is_open(&self) -> bool {
        self.state == "open"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EdgeKind {
    /// A starts-after link (GitHub "blocked by").
    After,
    /// A sub-issue link: the parent waits on the child.
    SubIssue,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::After => "after",
            Self::SubIssue => "sub_issue",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "after" => Some(Self::After),
            "sub_issue" => Some(Self::SubIssue),
            _ => None,
        }
    }
}

/// A `board_edges` row: `waiter` waits on `blocker`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub waiter: Key,
    pub blocker: Key,
    pub kind: EdgeKind,
    /// `github` or `sm:<session id>` / `sm:owner`.
    pub source: String,
}

/// A `board_prs` row: a pull request that will close the issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrRef {
    pub repo: String,
    pub number: i64,
    /// `OPEN`, `MERGED` or `CLOSED`.
    pub state: String,
    pub url: String,
}

/// An active lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    pub id: i64,
    pub goal: Key,
    pub rank: i64,
    pub added_at: String,
    pub added_by: String,
    pub added_by_name: String,
}

/// A live claim on a ticket, with its holder as the session record says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub session_id: String,
    pub name: String,
    pub state: HolderState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WaitingKind {
    Message,
    Review,
}

/// A waiting-on-you record (D1): an unanswered blocking message, or a doc
/// publish waiting for the owner's review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitingRecord {
    pub kind: WaitingKind,
    /// The sender or publisher.
    pub session_id: String,
    /// Reviews: the PR the publish names, `(repo, pr)`.
    pub pr: Option<(String, i64)>,
    pub text: String,
    pub url: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketState {
    NeedsYou,
    CloseReady,
    Ready,
    InProgress,
    Blocked,
    Done,
}

impl TicketState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NeedsYou => "needs_you",
            Self::CloseReady => "close_ready",
            Self::Ready => "ready",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::Done => "done",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "needs_you" => Some(Self::NeedsYou),
            "close_ready" => Some(Self::CloseReady),
            "ready" => Some(Self::Ready),
            "in_progress" => Some(Self::InProgress),
            "blocked" => Some(Self::Blocked),
            "done" => Some(Self::Done),
            _ => None,
        }
    }
}

pub const WARN_WORKING_WHILE_BLOCKED: &str = "working_while_blocked";
pub const WARN_HOLDER_STOPPED: &str = "holder_stopped";
pub const WARN_MERGED_NOT_CLOSED: &str = "merged_not_closed";
pub const WARN_CYCLE: &str = "cycle";
pub const WARN_STALE: &str = "stale";

/// A lane member as the last recompute wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub state: TicketState,
    pub joined_at: String,
}

/// Everything the model reads.
#[derive(Debug, Clone, Default)]
pub struct ModelInput {
    pub items: BTreeMap<Key, Item>,
    pub edges: Vec<Edge>,
    pub prs: BTreeMap<Key, Vec<PrRef>>,
    /// Active lanes, in rank order.
    pub lanes: Vec<Lane>,
    /// Live claims per ticket, retired holders included.
    pub holders: BTreeMap<Key, Vec<Holder>>,
    /// `work_links`: `(repo, pr)` to the ticket numbers it is for.
    pub pr_tickets: BTreeMap<(String, i64), BTreeSet<i64>>,
    pub waiting: Vec<WaitingRecord>,
    /// Repos whose reads are failing (C4).
    pub stale: BTreeSet<String>,
    /// `board_members` per lane id.
    pub members: BTreeMap<i64, BTreeMap<Key, Member>>,
    /// Repos the "not in any lane" list covers.
    pub read_repos: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NeedsYou {
    pub kind: WaitingKind,
    pub text: String,
    pub url: String,
    #[serde(skip)]
    pub created_at: String,
}

/// One ticket's lane-independent facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketFacts {
    pub item: Item,
    pub state: TicketState,
    pub needs_you: Option<NeedsYou>,
    pub holder: Option<Holder>,
    pub prs: Vec<PrRef>,
    /// Every ticket it waits on, in key order.
    pub waits_on: Vec<Key>,
    pub warnings: Vec<&'static str>,
    pub sub_issues_done: bool,
    pub sub_issues: Vec<Key>,
    pub sub_issues_closed: usize,
}

/// A ticket's row in a lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub key: Key,
    pub chain: Option<i64>,
    pub on_longest_chain: bool,
    /// Other active lanes that contain the ticket: `(lane id, rank)`.
    pub also_in: Vec<(i64, i64)>,
    pub new: bool,
    pub joined_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneView {
    pub lane: Lane,
    pub rows: Vec<Row>,
    pub longest_chain: Vec<Key>,
    /// Each loop's tickets, sorted.
    pub cycles: Vec<Vec<Key>>,
    pub stale: bool,
}

impl LaneView {
    pub fn count(&self, board: &Board, state: TicketState) -> usize {
        self.rows
            .iter()
            .filter(|row| board.facts[&row.key].state == state)
            .count()
    }

    pub fn contains(&self, key: &Key) -> bool {
        self.rows.iter().any(|row| &row.key == key)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Board {
    pub facts: BTreeMap<Key, TicketFacts>,
    pub lanes: Vec<LaneView>,
    /// Per read repo, its open tickets in no active lane, in row order.
    pub other: Vec<(String, Vec<Key>)>,
    /// Active lanes whose goal is closed: the recompute ends them (D5).
    pub goal_closed: Vec<Lane>,
}

impl Board {
    pub fn lane(&self, id: i64) -> Option<&LaneView> {
        self.lanes.iter().find(|lane| lane.lane.id == id)
    }
}

/// `repo#N` shortened against `base`: `#N` in the same repo, else
/// `name#N`.
pub fn short_ref(key: &Key, base: &str) -> String {
    if key.0 == base {
        format!("#{}", key.1)
    } else {
        format!("{}#{}", key.0.rsplit('/').next().unwrap_or(&key.0), key.1)
    }
}

fn placeholder(key: &Key) -> Item {
    Item {
        repo: key.0.clone(),
        number: key.1,
        title: String::new(),
        url: format!("https://github.com/{}/issues/{}", key.0, key.1),
        state: "open".to_owned(),
        state_reason: None,
        closed_at: None,
    }
}

fn is_open(input: &ModelInput, key: &Key) -> bool {
    input.items.get(key).is_none_or(Item::is_open)
}

/// Computes the board. `now` places the "new" mark; a member the last
/// recompute did not write joins now, or at the lane's add when the lane
/// has never been recomputed.
pub fn compute(input: &ModelInput, now: OffsetDateTime) -> Board {
    let mut waits_on: BTreeMap<Key, BTreeSet<Key>> = BTreeMap::new();
    let mut sub_issues: BTreeMap<Key, BTreeSet<Key>> = BTreeMap::new();
    for edge in &input.edges {
        waits_on
            .entry(edge.waiter.clone())
            .or_default()
            .insert(edge.blocker.clone());
        if edge.kind == EdgeKind::SubIssue {
            sub_issues
                .entry(edge.waiter.clone())
                .or_default()
                .insert(edge.blocker.clone());
        }
    }
    let cycle_of = cycles(input, &waits_on);

    // Lanes: goal-closed ones end; the rest get their members.
    let mut goal_closed = Vec::new();
    let mut lane_members: Vec<(Lane, BTreeSet<Key>)> = Vec::new();
    for lane in &input.lanes {
        if !is_open(input, &lane.goal) {
            goal_closed.push(lane.clone());
            continue;
        }
        lane_members.push((lane.clone(), members_of(input, &waits_on, &lane.goal)));
    }

    // Facts for every ticket any lane or the other list shows.
    let mut shown: BTreeSet<Key> = lane_members
        .iter()
        .flat_map(|(_, members)| members.iter().cloned())
        .collect();
    let in_lanes = shown.clone();
    let mut other_keys: BTreeMap<String, Vec<Key>> = BTreeMap::new();
    for item in input.items.values() {
        if item.is_open()
            && input.read_repos.contains(&item.repo)
            && !in_lanes.contains(&item.key())
        {
            other_keys
                .entry(item.repo.clone())
                .or_default()
                .push(item.key());
            shown.insert(item.key());
        }
    }
    // Blockers too, so every "waits on" shows its blocker's state.
    let blockers: Vec<Key> = shown
        .iter()
        .flat_map(|key| waits_on.get(key).into_iter().flatten().cloned())
        .collect();
    shown.extend(blockers);
    let waiting = waiting_by_ticket(input);
    let mut facts = BTreeMap::new();
    for key in &shown {
        facts.insert(
            key.clone(),
            ticket_facts(input, key, &waits_on, &sub_issues, &cycle_of, &waiting),
        );
    }

    let mut lanes = Vec::new();
    for (lane, members) in &lane_members {
        lanes.push(lane_view(
            input,
            lane,
            members,
            &lane_members,
            &waits_on,
            &cycle_of,
            &facts,
            now,
        ));
    }

    let other = input
        .read_repos
        .iter()
        .map(|repo| {
            let mut keys = other_keys.remove(repo).unwrap_or_default();
            keys.sort_by(|a, b| {
                facts[a]
                    .state
                    .cmp(&facts[b].state)
                    .then_with(|| a.1.cmp(&b.1))
            });
            (repo.clone(), keys)
        })
        .collect();

    Board {
        facts,
        lanes,
        other,
        goal_closed,
    }
}

/// D1 lane members: from the goal, every ticket an open member waits on;
/// closed members are leaves.
fn members_of(
    input: &ModelInput,
    waits_on: &BTreeMap<Key, BTreeSet<Key>>,
    goal: &Key,
) -> BTreeSet<Key> {
    let mut members = BTreeSet::from([goal.clone()]);
    let mut frontier = vec![goal.clone()];
    while let Some(ticket) = frontier.pop() {
        if !is_open(input, &ticket) {
            continue;
        }
        for blocker in waits_on.get(&ticket).into_iter().flatten() {
            if members.insert(blocker.clone()) {
                frontier.push(blocker.clone());
            }
        }
    }
    members
}

/// D3: the loop each open ticket sits in, as its component's sorted keys.
/// Tarjan's algorithm over open tickets; a component counts when it has
/// more than one ticket or a ticket waits on itself.
fn cycles(input: &ModelInput, waits_on: &BTreeMap<Key, BTreeSet<Key>>) -> BTreeMap<Key, Vec<Key>> {
    let nodes: Vec<Key> = waits_on
        .iter()
        .flat_map(|(waiter, blockers)| std::iter::once(waiter).chain(blockers.iter()))
        .filter(|key| is_open(input, key))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let index_of: BTreeMap<&Key, usize> = nodes.iter().enumerate().map(|(i, k)| (k, i)).collect();
    let adjacency: Vec<Vec<usize>> = nodes
        .iter()
        .map(|key| {
            waits_on
                .get(key)
                .into_iter()
                .flatten()
                .filter_map(|blocker| index_of.get(blocker).copied())
                .collect()
        })
        .collect();

    // Iterative Tarjan.
    let count = nodes.len();
    let mut index = vec![usize::MAX; count];
    let mut low = vec![0; count];
    let mut on_stack = vec![false; count];
    let mut stack = Vec::new();
    let mut next_index = 0;
    let mut components: Vec<Vec<usize>> = Vec::new();
    for root in 0..count {
        if index[root] != usize::MAX {
            continue;
        }
        let mut call: Vec<(usize, usize)> = vec![(root, 0)];
        index[root] = next_index;
        low[root] = next_index;
        next_index += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&mut (node, ref mut edge)) = call.last_mut() {
            if *edge < adjacency[node].len() {
                let next = adjacency[node][*edge];
                *edge += 1;
                if index[next] == usize::MAX {
                    index[next] = next_index;
                    low[next] = next_index;
                    next_index += 1;
                    stack.push(next);
                    on_stack[next] = true;
                    call.push((next, 0));
                } else if on_stack[next] {
                    low[node] = low[node].min(index[next]);
                }
                continue;
            }
            call.pop();
            if let Some(&(parent, _)) = call.last() {
                low[parent] = low[parent].min(low[node]);
            }
            if low[node] == index[node] {
                let mut component = Vec::new();
                while let Some(member) = stack.pop() {
                    on_stack[member] = false;
                    component.push(member);
                    if member == node {
                        break;
                    }
                }
                components.push(component);
            }
        }
    }

    let mut cycle_of = BTreeMap::new();
    for component in components {
        let looped = component.len() > 1 || adjacency[component[0]].contains(&component[0]);
        if !looped {
            continue;
        }
        let mut keys: Vec<Key> = component.iter().map(|i| nodes[*i].clone()).collect();
        keys.sort();
        for key in &keys {
            cycle_of.insert(key.clone(), keys.clone());
        }
    }
    cycle_of
}

/// The waiting-on-you record each ticket shows: the newest that applies.
fn waiting_by_ticket(input: &ModelInput) -> BTreeMap<Key, &WaitingRecord> {
    let mut by_session: BTreeMap<&str, Vec<&Key>> = BTreeMap::new();
    for (key, holders) in &input.holders {
        for holder in holders {
            by_session
                .entry(holder.session_id.as_str())
                .or_default()
                .push(key);
        }
    }
    let mut result: BTreeMap<Key, &WaitingRecord> = BTreeMap::new();
    for record in &input.waiting {
        let mut tickets: BTreeSet<Key> = by_session
            .get(record.session_id.as_str())
            .into_iter()
            .flatten()
            .map(|key| (*key).clone())
            .collect();
        if record.kind == WaitingKind::Review {
            if let Some((repo, pr)) = &record.pr {
                for number in input
                    .pr_tickets
                    .get(&(repo.clone(), *pr))
                    .into_iter()
                    .flatten()
                {
                    tickets.insert((repo.clone(), *number));
                }
            }
        }
        for key in tickets {
            let newer = result
                .get(&key)
                .is_none_or(|current| record.created_at > current.created_at);
            if newer {
                result.insert(key, record);
            }
        }
    }
    result
}

fn holder_rank(state: HolderState) -> u8 {
    match state {
        HolderState::Working => 0,
        HolderState::Idle => 1,
        HolderState::Stopped => 2,
        HolderState::Retired => 3,
    }
}

fn ticket_facts(
    input: &ModelInput,
    key: &Key,
    waits_on: &BTreeMap<Key, BTreeSet<Key>>,
    sub_issues: &BTreeMap<Key, BTreeSet<Key>>,
    cycle_of: &BTreeMap<Key, Vec<Key>>,
    waiting: &BTreeMap<Key, &WaitingRecord>,
) -> TicketFacts {
    let item = input
        .items
        .get(key)
        .cloned()
        .unwrap_or_else(|| placeholder(key));
    let blockers: Vec<Key> = waits_on.get(key).into_iter().flatten().cloned().collect();
    let open_blocker = blockers.iter().any(|blocker| is_open(input, blocker));
    let holder = input
        .holders
        .get(key)
        .into_iter()
        .flatten()
        .filter(|holder| holder.state != HolderState::Retired)
        .min_by_key(|holder| holder_rank(holder.state))
        .cloned();
    let prs = input.prs.get(key).cloned().unwrap_or_default();
    let open_pr = prs.iter().any(|pr| pr.state == "OPEN");
    let merged_pr = prs.iter().any(|pr| pr.state == "MERGED");
    let stale = input.stale.contains(&key.0);
    let any_stale = stale || blockers.iter().any(|b| input.stale.contains(&b.0));
    let in_cycle = cycle_of.contains_key(key);
    let needs_you = waiting.get(key).map(|record| NeedsYou {
        kind: record.kind,
        text: record.text.clone(),
        url: record.url.clone(),
        created_at: record.created_at.clone(),
    });

    let children = sub_issues.get(key);
    let sub_issues_closed = children.map_or(0, |children| {
        children
            .iter()
            .filter(|child| !is_open(input, child))
            .count()
    });
    let sub_issues_done = children
        .is_some_and(|children| !children.is_empty() && sub_issues_closed == children.len());
    let state = if !item.is_open() {
        TicketState::Done
    } else if needs_you.is_some() {
        TicketState::NeedsYou
    } else if holder.is_some() || open_pr {
        TicketState::InProgress
    } else if sub_issues_done && !open_blocker && !any_stale && !in_cycle {
        TicketState::CloseReady
    } else if !open_blocker && !any_stale && !in_cycle {
        TicketState::Ready
    } else {
        TicketState::Blocked
    };

    let mut warnings = Vec::new();
    if item.is_open() {
        if state == TicketState::InProgress && open_blocker {
            warnings.push(WARN_WORKING_WHILE_BLOCKED);
        }
        if holder
            .as_ref()
            .is_some_and(|holder| holder.state == HolderState::Stopped)
        {
            warnings.push(WARN_HOLDER_STOPPED);
        }
        if merged_pr {
            warnings.push(WARN_MERGED_NOT_CLOSED);
        }
        if in_cycle {
            warnings.push(WARN_CYCLE);
        }
        if stale || (state == TicketState::Blocked && any_stale && !open_blocker && !in_cycle) {
            warnings.push(WARN_STALE);
        }
    }

    TicketFacts {
        item,
        state,
        needs_you: if state == TicketState::NeedsYou {
            needs_you
        } else {
            None
        },
        holder,
        prs,
        waits_on: blockers,
        warnings,
        sub_issues_done,
        sub_issues: children.into_iter().flatten().cloned().collect(),
        sub_issues_closed,
    }
}

#[allow(clippy::too_many_arguments)]
fn lane_view(
    input: &ModelInput,
    lane: &Lane,
    members: &BTreeSet<Key>,
    all_lanes: &[(Lane, BTreeSet<Key>)],
    waits_on: &BTreeMap<Key, BTreeSet<Key>>,
    cycle_of: &BTreeMap<Key, Vec<Key>>,
    facts: &BTreeMap<Key, TicketFacts>,
    now: OffsetDateTime,
) -> LaneView {
    let open: BTreeSet<&Key> = members
        .iter()
        .filter(|key| facts[*key].item.is_open())
        .collect();
    let same_cycle = |a: &Key, b: &Key| {
        a == b
            || cycle_of
                .get(a)
                .is_some_and(|component| component.contains(b))
    };
    // waiters[t]: open members that wait on t, loop edges removed.
    let mut waiters: BTreeMap<&Key, Vec<&Key>> = BTreeMap::new();
    let mut fan_out: BTreeMap<&Key, usize> = BTreeMap::new();
    for waiter in &open {
        for blocker in waits_on.get(*waiter).into_iter().flatten() {
            let Some(blocker) = open.get(blocker) else {
                continue;
            };
            if *blocker != *waiter {
                *fan_out.entry(blocker).or_default() += 1;
            }
            if !same_cycle(waiter, blocker) {
                waiters.entry(blocker).or_default().push(waiter);
            }
        }
    }

    let mut chain: BTreeMap<&Key, i64> = BTreeMap::new();
    for key in &open {
        chain_of(key, &lane.goal, &waiters, &mut chain, 0);
    }

    // Longest chain: highest chain, ties to the lower repo then number.
    let pick = |candidates: &mut dyn Iterator<Item = &Key>| -> Option<Key> {
        candidates
            .map(|key| (chain.get(key).copied().unwrap_or(1), key))
            .max_by(|(ca, ka), (cb, kb)| ca.cmp(cb).then_with(|| kb.cmp(ka)))
            .map(|(_, key)| key.clone())
    };
    let mut longest = Vec::new();
    if let Some(start) = pick(&mut open.iter().copied()) {
        let mut current = start;
        loop {
            longest.push(current.clone());
            if current == lane.goal || longest.len() > open.len() {
                break;
            }
            let next = pick(&mut waiters.get(&current).into_iter().flatten().copied());
            match next {
                Some(next) if !longest.contains(&next) => current = next,
                _ => break,
            }
        }
    }

    let first_recompute = !input.members.contains_key(&lane.id);
    let previous = input.members.get(&lane.id);
    let added_at = crate::owner_push::parse_ts(&lane.added_at);
    let mut rows: Vec<Row> = members
        .iter()
        .map(|key| {
            let joined_at = previous
                .and_then(|members| members.get(key))
                .map(|member| member.joined_at.clone())
                .unwrap_or_else(|| {
                    if first_recompute {
                        lane.added_at.clone()
                    } else {
                        crate::owner_push::format_ts(now)
                    }
                });
            let joined = crate::owner_push::parse_ts(&joined_at);
            let new = match (joined, added_at) {
                (Some(joined), Some(added)) => joined > added && now - joined < NEW_MARK,
                _ => false,
            };
            Row {
                key: key.clone(),
                chain: open
                    .contains(key)
                    .then(|| chain.get(key).copied().unwrap_or(1)),
                on_longest_chain: longest.contains(key),
                also_in: all_lanes
                    .iter()
                    .filter(|(other, members)| other.id != lane.id && members.contains(key))
                    .map(|(other, _)| (other.id, other.rank))
                    .collect(),
                new,
                joined_at,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        let fa = &facts[&a.key];
        let fb = &facts[&b.key];
        fa.state.cmp(&fb.state).then_with(|| {
            if fa.state == TicketState::Done {
                fb.item
                    .closed_at
                    .cmp(&fa.item.closed_at)
                    .then_with(|| a.key.cmp(&b.key))
            } else {
                b.chain
                    .cmp(&a.chain)
                    .then_with(|| {
                        let na = fan_out.get(&a.key).copied().unwrap_or(0);
                        let nb = fan_out.get(&b.key).copied().unwrap_or(0);
                        nb.cmp(&na)
                    })
                    .then_with(|| a.key.cmp(&b.key))
            }
        })
    });

    let cycles: Vec<Vec<Key>> = open
        .iter()
        .filter_map(|key| cycle_of.get(*key).cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    LaneView {
        lane: lane.clone(),
        rows,
        longest_chain: longest,
        cycles,
        stale: input.stale.contains(&lane.goal.0),
    }
}

/// chain(t) = 1 + the highest chain among its waiters; the goal is 1.
fn chain_of<'a>(
    key: &'a Key,
    goal: &Key,
    waiters: &BTreeMap<&'a Key, Vec<&'a Key>>,
    chain: &mut BTreeMap<&'a Key, i64>,
    depth: usize,
) -> i64 {
    if let Some(value) = chain.get(key) {
        return *value;
    }
    if key == goal || depth > 10_000 {
        chain.insert(key, 1);
        return 1;
    }
    let best = waiters
        .get(key)
        .into_iter()
        .flatten()
        .map(|waiter| chain_of(waiter, goal, waiters, chain, depth + 1))
        .max()
        .unwrap_or(0);
    chain.insert(key, best + 1);
    best + 1
}
