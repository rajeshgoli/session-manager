//! Analytics › Spend (sm#1662, ticket #1675): what used the weekly quota,
//! as a tree Root → Repo → Thread → Agent in percent of the week.
//!
//! Every ledger turn is priced by `quota_rates`, attributed to a thread by
//! `work_attribution`, then scaled so each weekly window's rows add up to
//! that window's meter reading (scale capped at 0.8–1.25; the rest shows as
//! "Not in the ledger"). Codex cloud PR reviews count 0.05% each. See
//! `specs/1662_analytics_redesign.html`, appendices B, C and F.1.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::quota_rates::{self, Tokens, CLOUD_REVIEW_PERCENT};
use crate::work_attribution::{nanos, Attribution, Attributor, Basis};
use crate::work_claims::{canonical_repo, history_path};

const SCALE_MIN: f64 = 0.8;
const SCALE_MAX: f64 = 1.25;
/// A gap smaller than this, either way, is rounding.
const GAP_THRESHOLD: f64 = 0.1;
/// Burn samples of one account whose window starts lie this close are one
/// window: Codex reports its start with a second or two of jitter.
const WINDOW_JOIN_NANOS: i128 = 3_600 * 1_000_000_000;
/// No pace line in a window's first hour (as `usage_report`).
const MIN_PACE_ELAPSED_NANOS: i128 = 3_600 * 1_000_000_000;
const NANOS_PER_SECOND: i128 = 1_000_000_000;
/// Windows starting earlier than this cannot touch a four-week range.
const WINDOWS_LOOKBACK_NANOS: i128 = 36 * 86_400 * NANOS_PER_SECOND;
const OWNER_AGENT: &str = "owner";
const UNASSIGNED_SEAT: &str = "unassigned";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SpendRange {
    Week,
    LastWeek,
    FourWeeks,
}

impl SpendRange {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "week" => Some(Self::Week),
            "last_week" => Some(Self::LastWeek),
            "4w" => Some(Self::FourWeeks),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Week => "week",
            Self::LastWeek => "last_week",
            Self::FourWeeks => "4w",
        }
    }
}

pub fn parse_provider(value: &str) -> Option<&'static str> {
    match value {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        _ => None,
    }
}

fn window_kind(provider: &str) -> &'static str {
    if provider == "codex" {
        "codex_10080"
    } else {
        "weekly_all"
    }
}

/// Where the Spend report reads from.
pub struct SpendSources<'a> {
    pub usage_db: &'a Path,
    pub queue_db: &'a Path,
    /// Configured account labels; the `accounts` table's label otherwise.
    pub account_labels: &'a BTreeMap<String, String>,
    /// Sessions that are live (not stopped or retired).
    pub live_sessions: &'a BTreeSet<String>,
    /// A ledger `project_key` → its repo (`work_attribution::folder_repo`).
    pub repo_of: &'a dyn Fn(&str) -> String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpendReport {
    pub generated_at: String,
    pub provider: &'static str,
    pub range: &'static str,
    pub start: String,
    pub end: String,
    pub rates_fitted_at: &'static str,
    pub meters: Vec<Meter>,
    pub total: Total,
    pub basis: BTreeMap<&'static str, f64>,
    pub parts_legend: Vec<LegendEntry>,
    pub notes: Vec<String>,
    pub root: SpendNode,
}

#[derive(Debug, Clone, Serialize)]
pub struct Meter {
    pub account_key: String,
    pub label: Option<String>,
    pub percent: f64,
    pub observed_at: String,
    pub window_start: String,
    pub resets_at: String,
    pub fitted_percent: f64,
    pub scale: f64,
    /// Meter minus the scaled estimates: positive is "Not in the ledger".
    pub gap: f64,
    pub pace: Option<Pace>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pace {
    /// The meter reaches 100% at `at`, before the reset.
    RunsOut { at: String },
    /// The meter's projected reading at the reset.
    OnPace { percent: f64 },
}

#[derive(Debug, Clone, Serialize)]
pub struct Total {
    pub percent: f64,
    pub tokens: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LegendEntry {
    pub key: String,
    pub label: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpendNode {
    pub id: String,
    pub kind: &'static str,
    pub label: String,
    pub state: Option<String>,
    pub history_path: Option<String>,
    pub session_id: Option<String>,
    pub session_status: Option<&'static str>,
    pub percent: f64,
    pub tokens: i64,
    /// Percent by model family, or `review` for cloud reviews.
    pub parts: BTreeMap<String, f64>,
    /// Sorted by percent, largest first; agents have none.
    pub children: Vec<SpendNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<ModelRow>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelRow {
    pub model: String,
    pub effort: Option<String>,
    pub turns: i64,
    pub percent: f64,
    pub tokens: TokenBreakdown,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct TokenBreakdown {
    pub input: i64,
    pub output: i64,
    pub cache_write: i64,
    pub cache_read: i64,
}

impl From<&Tokens> for TokenBreakdown {
    fn from(tokens: &Tokens) -> Self {
        Self {
            input: tokens.input,
            output: tokens.output,
            cache_write: tokens.cache_write_5m + tokens.cache_write_1h,
            cache_read: tokens.cache_read,
        }
    }
}

/// One weekly window of one account: the burn samples whose starts lie
/// within an hour of each other, read at the latest sample.
#[derive(Debug, Clone)]
struct Window {
    account_key: String,
    starts: BTreeSet<String>,
    start: i128,
    resets_at: i128,
    start_text: String,
    resets_text: String,
    percent: f64,
    observed_at: i128,
    observed_text: String,
}

impl Window {
    fn contains(&self, at: i128) -> bool {
        self.start <= at && at < self.resets_at
    }
}

/// Ledger turns of one seat, minute, folder, model and window.
#[derive(Debug, Clone)]
struct TurnGroup {
    account_key: String,
    seat_id: String,
    project_key: String,
    minute: i128,
    model: String,
    effort: Option<String>,
    window_start: Option<String>,
    turns: i64,
    tokens: Tokens,
}

#[derive(Debug, Clone)]
struct LandedReview {
    repo: String,
    pr: i64,
    requester: Option<String>,
    landed_at: i128,
}

#[derive(Debug, Clone)]
struct TimelineSpan {
    account_key: String,
    from: i128,
    to: Option<i128>,
}

/// The provider whose current weekly meters, summed over its accounts,
/// read higher; Claude on a tie or when neither has one.
pub fn default_provider(usage_db: &Path, now: OffsetDateTime) -> Result<&'static str> {
    if !usage_db.exists() {
        return Ok("claude");
    }
    let usage = open_read_only(usage_db)?;
    let now = now.unix_timestamp_nanos();
    let current = |provider: &str| -> Result<f64> {
        let accounts = provider_accounts(&usage, provider)?;
        let windows = load_windows(&usage, provider, &accounts, now - WINDOWS_LOOKBACK_NANOS)?;
        Ok(current_windows(&windows, now)
            .into_iter()
            .map(|i| windows[i].percent)
            .sum())
    };
    Ok(if current("codex")? > current("claude")? {
        "codex"
    } else {
        "claude"
    })
}

pub fn spend_report(
    sources: &SpendSources<'_>,
    provider: &'static str,
    range: SpendRange,
    now: OffsetDateTime,
) -> Result<SpendReport> {
    let generated_at = format_nanos(now.unix_timestamp_nanos());
    let now_ns = now.unix_timestamp_nanos();
    if !sources.usage_db.exists() {
        let start = format_nanos(now_ns - 7 * 86_400 * NANOS_PER_SECOND);
        return Ok(empty_report(
            generated_at.clone(),
            provider,
            range,
            start,
            generated_at,
        ));
    }
    let usage = open_read_only(sources.usage_db)?;
    let accounts = provider_accounts(&usage, provider)?;
    let labels = account_labels(&usage, sources.account_labels)?;
    let windows = load_windows(&usage, provider, &accounts, now_ns - WINDOWS_LOOKBACK_NANOS)?;

    // The windows whose meters the range reports, and the turns it keeps.
    let four_weeks_from = now_ns - 28 * 86_400 * NANOS_PER_SECOND;
    let selected: Vec<usize> = match range {
        SpendRange::Week => current_windows(&windows, now_ns),
        SpendRange::LastWeek => previous_windows(&windows, now_ns),
        SpendRange::FourWeeks => (0..windows.len())
            .filter(|&i| windows[i].resets_at > four_weeks_from && windows[i].start < now_ns)
            .collect(),
    };
    let (start, end) = match range {
        SpendRange::FourWeeks => (four_weeks_from, now_ns),
        _ => (
            selected
                .iter()
                .map(|&i| windows[i].start)
                .min()
                .unwrap_or(now_ns - 7 * 86_400 * NANOS_PER_SECOND),
            selected
                .iter()
                .map(|&i| windows[i].resets_at)
                .max()
                .unwrap_or(now_ns),
        ),
    };
    // Scaling needs every turn of a window, including the part of a window
    // that starts before a four-week range.
    let load_from = selected
        .iter()
        .map(|&i| windows[i].start)
        .min()
        .unwrap_or(start)
        .min(start)
        - WINDOW_JOIN_NANOS;
    let load_to = end.max(now_ns) + WINDOW_JOIN_NANOS;
    let turns = load_turns(&usage, provider, &accounts, load_from, load_to)?;
    let reviews = if provider == "codex" {
        load_reviews(sources.queue_db, load_from, load_to)?
    } else {
        Vec::new()
    };
    let timeline = load_timeline(&usage, provider)?;
    let attributor = Attributor::load(sources.queue_db, &usage)?;

    // Each turn and review's window, then each window's fitted sum.
    let turn_windows: Vec<Option<usize>> = turns
        .iter()
        .map(|turn| turn_window(&windows, turn))
        .collect();
    let review_windows: Vec<Option<(String, Option<usize>)>> = reviews
        .iter()
        .map(|review| {
            account_at(&timeline, review.landed_at).map(|account| {
                let window = windows.iter().position(|window| {
                    window.account_key == account && window.contains(review.landed_at)
                });
                (account, window)
            })
        })
        .collect();
    let mut fitted = vec![0.0; windows.len()];
    let mut notes = BTreeSet::new();
    let mut turn_fitted = Vec::with_capacity(turns.len());
    for (turn, window) in turns.iter().zip(&turn_windows) {
        let pricing = quota_rates::pricing(provider, &turn.model);
        if pricing.needs_note {
            notes.insert(format!(
                "{} has no fitted rate; priced as {}",
                turn.model,
                quota_rates::family_label(quota_rates::standard_family(provider))
            ));
        }
        let percent = quota_rates::fitted_percent(provider, &turn.model, &turn.tokens);
        if let Some(window) = window {
            fitted[*window] += percent;
        }
        turn_fitted.push((pricing.family, percent));
    }
    for placed in review_windows.iter().flatten() {
        if let (_, Some(window)) = placed {
            fitted[*window] += CLOUD_REVIEW_PERCENT;
        }
    }
    let scales: Vec<f64> = windows
        .iter()
        .zip(&fitted)
        .map(|(window, fitted)| scale(window.percent, *fitted))
        .collect();
    let selected_set: BTreeSet<usize> = selected.iter().copied().collect();
    let in_range = |window: Option<usize>, at: i128| match range {
        SpendRange::FourWeeks => at >= four_weeks_from && at < now_ns,
        _ => window.is_some_and(|window| selected_set.contains(&window)),
    };

    // Leaves: (repo, thread, agent).
    let mut tree = TreeBuilder::default();
    let mut basis: BTreeMap<&'static str, f64> = [
        (Basis::Claim, 0.0),
        (Basis::Parent, 0.0),
        (Basis::Name, 0.0),
        (Basis::None, 0.0),
    ]
    .into_iter()
    .map(|(basis, value)| (basis.key(), value))
    .collect();
    let mut folder_repos: HashMap<&str, String> = HashMap::new();
    let mut attributions: HashMap<(&str, i128, &str), Attribution> = HashMap::new();
    for ((turn, window), (family, fitted_percent)) in
        turns.iter().zip(&turn_windows).zip(&turn_fitted)
    {
        if !in_range(*window, turn.minute) {
            continue;
        }
        let percent = fitted_percent * window.map_or(1.0, |window| scales[window]);
        let folder = folder_repos
            .entry(turn.project_key.as_str())
            .or_insert_with(|| (sources.repo_of)(&turn.project_key))
            .clone();
        let attribution = attributions
            .entry((
                turn.seat_id.as_str(),
                turn.minute,
                turn.project_key.as_str(),
            ))
            .or_insert_with(|| attributor.attribute(&turn.seat_id, turn.minute, &folder))
            .clone();
        *basis.entry(attribution.basis.key()).or_default() += percent;
        let leaf = tree.leaf(&attribution.repo, attribution.thread, &turn.seat_id);
        leaf.add(
            family,
            percent,
            &turn.model,
            turn.effort.as_deref(),
            turn.turns,
            &turn.tokens,
        );
    }
    for (review, placed) in reviews.iter().zip(&review_windows) {
        let window = placed.as_ref().and_then(|(_, window)| *window);
        if !in_range(window, review.landed_at) {
            continue;
        }
        let percent = CLOUD_REVIEW_PERCENT * window.map_or(1.0, |window| scales[window]);
        *basis.entry(Basis::Claim.key()).or_default() += percent;
        let repo = canonical_repo(&review.repo);
        let thread = attributor.pr_thread(&repo, review.pr);
        let agent = review.requester.as_deref().unwrap_or(OWNER_AGENT);
        tree.leaf(&repo, Some(thread), agent).add(
            "review",
            percent,
            "codex-cloud-review",
            None,
            1,
            &Tokens::default(),
        );
    }

    // Meters, gaps and the pace line.
    let mut gap = 0.0;
    let mut meters = Vec::new();
    for &i in &selected {
        let window = &windows[i];
        let window_gap = window.percent - scales[i] * fitted[i];
        let share = match range {
            SpendRange::FourWeeks => overlap_share(window, four_weeks_from, now_ns),
            _ => 1.0,
        };
        gap += window_gap * share;
        meters.push(Meter {
            account_key: window.account_key.clone(),
            label: labels.get(&window.account_key).cloned(),
            percent: window.percent,
            observed_at: window.observed_text.clone(),
            window_start: window.start_text.clone(),
            resets_at: window.resets_text.clone(),
            fitted_percent: round(fitted[i]),
            scale: round(scales[i]),
            gap: round(window_gap),
            pace: (range == SpendRange::Week).then(|| pace(window)).flatten(),
        });
    }
    meters.sort_by(|left, right| {
        left.account_key
            .cmp(&right.account_key)
            .then_with(|| left.window_start.cmp(&right.window_start))
    });
    if gap <= -GAP_THRESHOLD {
        notes.insert(format!(
            "Estimates exceed the meter by {:.1} points",
            gap.abs()
        ));
    }

    let names = |seat: &str| -> (String, Option<String>, Option<&'static str>) {
        match seat {
            OWNER_AGENT => ("You".to_owned(), None, None),
            UNASSIGNED_SEAT => ("Unassigned".to_owned(), None, None),
            _ => (
                attributor.name(seat).unwrap_or(seat).to_owned(),
                Some(seat.to_owned()),
                Some(if sources.live_sessions.contains(seat) {
                    "running"
                } else {
                    "stopped"
                }),
            ),
        }
    };
    let mut root = tree.build(&attributor, &names);
    if gap >= GAP_THRESHOLD {
        root.percent += gap;
        root.children.push(SpendNode {
            id: "gap".to_owned(),
            kind: "gap",
            label: "Not in the ledger".to_owned(),
            state: None,
            history_path: None,
            session_id: None,
            session_status: None,
            percent: gap,
            tokens: 0,
            parts: BTreeMap::new(),
            children: Vec::new(),
            models: None,
        });
        sort_nodes(&mut root.children);
    }
    let mut family_totals: Vec<(String, f64)> = root
        .parts
        .iter()
        .map(|(key, value)| (key.clone(), *value))
        .collect();
    family_totals.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    let parts_legend = family_totals
        .into_iter()
        .map(|(key, _)| LegendEntry {
            label: quota_rates::family_label(&key),
            key,
        })
        .collect();
    round_node(&mut root);
    Ok(SpendReport {
        generated_at,
        provider,
        range: range.id(),
        start: format_nanos(start),
        end: format_nanos(end),
        rates_fitted_at: quota_rates::FITTED_AT,
        meters,
        total: Total {
            percent: root.percent,
            tokens: root.tokens,
        },
        basis: basis
            .into_iter()
            .map(|(key, value)| (key, round(value)))
            .collect(),
        parts_legend,
        notes: notes.into_iter().collect(),
        root,
    })
}

fn empty_report(
    generated_at: String,
    provider: &'static str,
    range: SpendRange,
    start: String,
    end: String,
) -> SpendReport {
    SpendReport {
        generated_at,
        provider,
        range: range.id(),
        start,
        end,
        rates_fitted_at: quota_rates::FITTED_AT,
        meters: Vec::new(),
        total: Total {
            percent: 0.0,
            tokens: 0,
        },
        basis: ["claim", "parent", "name", "none"]
            .into_iter()
            .map(|key| (key, 0.0))
            .collect(),
        parts_legend: Vec::new(),
        notes: Vec::new(),
        root: TreeBuilder::default().build(&Attributor::default(), &|seat: &str| {
            (seat.to_owned(), None, None)
        }),
    }
}

/// `clamp(meter / fitted, 0.8, 1.25)`; 1 when nothing was fitted.
fn scale(meter: f64, fitted: f64) -> f64 {
    if fitted <= 0.0 {
        1.0
    } else {
        (meter / fitted).clamp(SCALE_MIN, SCALE_MAX)
    }
}

/// The share of a window's elapsed time that lies in `[from, to)`.
fn overlap_share(window: &Window, from: i128, to: i128) -> f64 {
    let elapsed_end = window.resets_at.min(to);
    let whole = elapsed_end - window.start;
    if whole <= 0 {
        return 0.0;
    }
    let overlap = elapsed_end.min(to) - window.start.max(from);
    (overlap.max(0) as f64 / whole as f64).clamp(0.0, 1.0)
}

/// Linear burn since the window started, as `usage_report`'s projection.
fn pace(window: &Window) -> Option<Pace> {
    let elapsed = window.observed_at - window.start;
    let horizon = window.resets_at - window.observed_at;
    if elapsed < MIN_PACE_ELAPSED_NANOS || horizon <= 0 {
        return None;
    }
    let rate = window.percent.max(0.0) / elapsed as f64;
    let projected = window.percent + rate * horizon as f64;
    if projected >= 100.0 && rate > 0.0 {
        let until_full = ((100.0 - window.percent).max(0.0) / rate) as i128;
        Some(Pace::RunsOut {
            at: format_nanos(window.observed_at + until_full),
        })
    } else {
        Some(Pace::OnPace {
            percent: round(projected),
        })
    }
}

fn current_windows(windows: &[Window], now: i128) -> Vec<usize> {
    let mut latest: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, window) in windows.iter().enumerate() {
        if window.resets_at <= now || window.start > now {
            continue;
        }
        let entry = latest.entry(window.account_key.as_str()).or_insert(i);
        if windows[*entry].start < window.start {
            *entry = i;
        }
    }
    latest.into_values().collect()
}

/// Per account: the window before its current one, or, for an account with
/// no current window, its latest window if that reset within the last week.
fn previous_windows(windows: &[Window], now: i128) -> Vec<usize> {
    let current = current_windows(windows, now);
    let accounts: BTreeSet<&str> = windows.iter().map(|w| w.account_key.as_str()).collect();
    let mut chosen = Vec::new();
    for account in accounts {
        let current_start = current
            .iter()
            .map(|&i| &windows[i])
            .find(|window| window.account_key == account)
            .map(|window| window.start);
        let cutoff = current_start.unwrap_or(now);
        let previous = windows
            .iter()
            .enumerate()
            .filter(|(_, window)| {
                window.account_key == account && window.resets_at <= cutoff + WINDOW_JOIN_NANOS
            })
            .filter(|(_, window)| window.start < cutoff)
            .max_by_key(|(_, window)| window.start);
        if let Some((i, window)) = previous {
            if current_start.is_some() || window.resets_at > now - 7 * 86_400 * NANOS_PER_SECOND {
                chosen.push(i);
            }
        }
    }
    chosen
}

fn turn_window(windows: &[Window], turn: &TurnGroup) -> Option<usize> {
    let of_account = || {
        windows
            .iter()
            .enumerate()
            .filter(|(_, window)| window.account_key == turn.account_key)
    };
    if let Some(start) = &turn.window_start {
        if let Some((i, _)) = of_account().find(|(_, window)| window.starts.contains(start)) {
            return Some(i);
        }
    }
    of_account()
        .find(|(_, window)| window.contains(turn.minute))
        .map(|(i, _)| i)
}

fn account_at(timeline: &[TimelineSpan], at: i128) -> Option<String> {
    timeline
        .iter()
        .find(|span| span.from <= at && span.to.is_none_or(|to| at < to))
        .map(|span| span.account_key.clone())
}

#[derive(Debug, Default)]
struct Leaf {
    percent: f64,
    tokens: Tokens,
    parts: BTreeMap<String, f64>,
    models: BTreeMap<(String, Option<String>), (i64, f64, Tokens)>,
}

impl Leaf {
    fn add(
        &mut self,
        family: &str,
        percent: f64,
        model: &str,
        effort: Option<&str>,
        turns: i64,
        tokens: &Tokens,
    ) {
        self.percent += percent;
        self.tokens.add(tokens);
        *self.parts.entry(family.to_owned()).or_default() += percent;
        let entry = self
            .models
            .entry((model.to_owned(), effort.map(str::to_owned)))
            .or_default();
        entry.0 += turns;
        entry.1 += percent;
        entry.2.add(tokens);
    }
}

type LeafKey = (String, Option<i64>, String);
/// An agent id → its label, session id and session status.
type AgentNames<'a> = dyn Fn(&str) -> (String, Option<String>, Option<&'static str>) + 'a;

#[derive(Debug, Default)]
struct TreeBuilder {
    leaves: BTreeMap<LeafKey, Leaf>,
}

impl TreeBuilder {
    fn leaf(&mut self, repo: &str, thread: Option<i64>, agent: &str) -> &mut Leaf {
        self.leaves
            .entry((repo.to_owned(), thread, agent.to_owned()))
            .or_default()
    }

    fn build(self, attributor: &Attributor, names: &AgentNames<'_>) -> SpendNode {
        let mut repos: BTreeMap<String, BTreeMap<Option<i64>, Vec<SpendNode>>> = BTreeMap::new();
        for ((repo, thread, agent), leaf) in self.leaves {
            let (label, session_id, session_status) = names(&agent);
            let mut models: Vec<ModelRow> = leaf
                .models
                .into_iter()
                .map(|((model, effort), (turns, percent, tokens))| ModelRow {
                    model,
                    effort,
                    turns,
                    percent,
                    tokens: TokenBreakdown::from(&tokens),
                })
                .collect();
            models.sort_by(|left, right| {
                right
                    .percent
                    .total_cmp(&left.percent)
                    .then_with(|| left.model.cmp(&right.model))
            });
            repos
                .entry(repo)
                .or_default()
                .entry(thread)
                .or_default()
                .push(SpendNode {
                    id: format!("a:{agent}"),
                    kind: "agent",
                    label,
                    state: None,
                    history_path: None,
                    session_id,
                    session_status,
                    percent: leaf.percent,
                    tokens: leaf.tokens.total(),
                    parts: leaf.parts,
                    children: Vec::new(),
                    models: Some(models),
                });
        }
        let repo_nodes = repos
            .into_iter()
            .map(|(repo, threads)| {
                let thread_nodes = threads
                    .into_iter()
                    .map(|(thread, agents)| {
                        let meta = thread_meta(attributor, &repo, thread);
                        let mut node = parent_node(meta.id, "thread", meta.label, agents);
                        node.state = meta.state;
                        node.history_path = meta.history_path;
                        node
                    })
                    .collect();
                let label = repo.rsplit('/').next().unwrap_or(&repo).to_owned();
                parent_node(format!("r:{repo}"), "repo", label, thread_nodes)
            })
            .collect();
        parent_node("root".to_owned(), "root", "All".to_owned(), repo_nodes)
    }
}

/// A thread node's id, label, state and History link, shared with Time.
pub(crate) struct ThreadMeta {
    pub id: String,
    pub label: String,
    pub state: Option<String>,
    pub history_path: Option<String>,
}

pub(crate) fn thread_meta(attributor: &Attributor, repo: &str, thread: Option<i64>) -> ThreadMeta {
    let item = thread.and_then(|number| attributor.item(repo, number));
    let label = match (thread, item) {
        (None, _) => "No ticket".to_owned(),
        (Some(number), Some(item)) if !item.title.is_empty() => {
            format!("#{number} {}", item.title)
        }
        (Some(number), _) => format!("#{number}"),
    };
    let id = match thread {
        Some(number) => format!("t:{repo}#{number}"),
        None => format!("t:{repo}#none"),
    };
    ThreadMeta {
        id,
        label,
        state: item.map(|item| item.state.clone()),
        history_path: thread
            .filter(|_| repo.contains('/'))
            .map(|number| history_path(repo, number)),
    }
}

fn parent_node(
    id: String,
    kind: &'static str,
    label: String,
    children: Vec<SpendNode>,
) -> SpendNode {
    let mut node = SpendNode {
        id,
        kind,
        label,
        state: None,
        history_path: None,
        session_id: None,
        session_status: None,
        percent: 0.0,
        tokens: 0,
        parts: BTreeMap::new(),
        children,
        models: None,
    };
    for child in &node.children {
        node.percent += child.percent;
        node.tokens += child.tokens;
        for (key, value) in &child.parts {
            *node.parts.entry(key.clone()).or_default() += value;
        }
    }
    sort_nodes(&mut node.children);
    node
}

fn sort_nodes(nodes: &mut [SpendNode]) {
    nodes.sort_by(|left, right| {
        right
            .percent
            .total_cmp(&left.percent)
            .then_with(|| left.id.cmp(&right.id))
    });
}

/// Four decimals; never `-0`.
fn round(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0 + 0.0
}

fn round_node(node: &mut SpendNode) {
    node.percent = round(node.percent);
    for value in node.parts.values_mut() {
        *value = round(*value);
    }
    for model in node.models.iter_mut().flatten() {
        model.percent = round(model.percent);
    }
    for child in &mut node.children {
        round_node(child);
    }
}

pub(crate) fn open_read_only(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open {}", path.display()))?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(conn)
}

pub(crate) fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn provider_accounts(usage: &Connection, provider: &str) -> Result<Vec<String>> {
    if !table_exists(usage, "accounts")? {
        return Ok(Vec::new());
    }
    let mut statement =
        usage.prepare("SELECT account_key FROM accounts WHERE provider = ?1 ORDER BY 1")?;
    let accounts = statement
        .query_map([provider], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(accounts)
}

fn account_labels(
    usage: &Connection,
    configured: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let mut labels = BTreeMap::new();
    if table_exists(usage, "accounts")? {
        let mut statement =
            usage.prepare("SELECT account_key, label FROM accounts WHERE label IS NOT NULL")?;
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (key, label) = row?;
            labels.insert(key, label);
        }
    }
    labels.extend(configured.iter().map(|(k, v)| (k.clone(), v.clone())));
    Ok(labels)
}

/// Latest sample per burn window starting at or after `since`, joined
/// into windows per account.
fn load_windows(
    usage: &Connection,
    provider: &str,
    accounts: &[String],
    since: i128,
) -> Result<Vec<Window>> {
    if !table_exists(usage, "burn_samples")? {
        return Ok(Vec::new());
    }
    // Two steps so the first is a covering-index range scan.
    let mut latest = usage.prepare(
        "SELECT window_start, MAX(observed_at) FROM burn_samples
          WHERE account_key = ?1 AND window_kind = ?2 AND window_scope IS NULL
            AND window_start >= ?3
          GROUP BY window_start ORDER BY window_start",
    )?;
    let mut sample = usage.prepare(
        "SELECT resets_at, percent FROM burn_samples
          WHERE account_key = ?1 AND window_kind = ?2 AND window_scope IS NULL
            AND window_start = ?3 AND observed_at = ?4
          ORDER BY id DESC LIMIT 1",
    )?;
    let kind = window_kind(provider);
    let since = lexical_bound(since);
    let mut windows: Vec<Window> = Vec::new();
    for account_key in accounts {
        let starts = latest
            .query_map(params![account_key, kind, since], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (start_text, observed_text) in starts {
            let Some((resets_text, percent)) = sample
                .query_row(
                    params![account_key, kind, start_text, observed_text],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
                )
                .optional()?
            else {
                continue;
            };
            let (Some(start), Some(resets_at), Some(observed_at)) = (
                nanos(&start_text),
                nanos(&resets_text),
                nanos(&observed_text),
            ) else {
                continue;
            };
            if let Some(last) = windows.last_mut().filter(|last| {
                &last.account_key == account_key && (start - last.start).abs() <= WINDOW_JOIN_NANOS
            }) {
                last.starts.insert(start_text);
                if observed_at > last.observed_at {
                    last.percent = percent;
                    last.observed_at = observed_at;
                    last.observed_text = observed_text;
                    last.resets_at = resets_at;
                    last.resets_text = resets_text;
                }
                continue;
            }
            windows.push(Window {
                account_key: account_key.clone(),
                starts: BTreeSet::from([start_text.clone()]),
                start,
                resets_at,
                start_text,
                resets_text,
                percent,
                observed_at,
                observed_text,
            });
        }
    }
    Ok(windows)
}

fn load_turns(
    usage: &Connection,
    provider: &str,
    accounts: &[String],
    from: i128,
    to: i128,
) -> Result<Vec<TurnGroup>> {
    if accounts.is_empty() || !table_exists(usage, "message_ledger")? {
        return Ok(Vec::new());
    }
    let has_windows = table_exists(usage, "message_window")?;
    let window_join = if has_windows {
        "LEFT JOIN message_window AS mapped
           ON mapped.msg_id = ledger.msg_id AND mapped.window_kind = ?4"
    } else {
        "LEFT JOIN (SELECT NULL AS window_start, ?4 AS k) AS mapped ON 0"
    };
    let sql = format!(
        "SELECT ledger.account_key, ledger.seat_id, ledger.project_key, ledger.bucket_ts,
                ledger.model, ledger.effort, mapped.window_start, COUNT(*),
                SUM(ledger.input_tokens), SUM(ledger.output_tokens),
                SUM(ledger.cache_write_5m), SUM(ledger.cache_write_1h),
                SUM(ledger.cache_read_tokens)
           FROM message_ledger AS ledger
           {window_join}
          WHERE ledger.account_key = ?1
            AND ledger.bucket_ts >= ?2 AND ledger.bucket_ts < ?3
            AND ledger.model != '<synthetic>'
          GROUP BY ledger.account_key, ledger.seat_id, ledger.project_key, ledger.bucket_ts,
                   ledger.model, ledger.effort, mapped.window_start"
    );
    let mut statement = usage.prepare(&sql)?;
    let from = lexical_bound(from);
    let to = lexical_bound(to);
    let mut groups = Vec::new();
    for account in accounts {
        let rows =
            statement.query_map(params![account, from, to, window_kind(provider)], |row| {
                Ok((
                    TurnGroup {
                        account_key: row.get(0)?,
                        seat_id: row.get(1)?,
                        project_key: row.get(2)?,
                        minute: 0,
                        model: row.get(4)?,
                        effort: row.get(5)?,
                        window_start: row.get(6)?,
                        turns: row.get(7)?,
                        tokens: Tokens {
                            input: row.get(8)?,
                            output: row.get(9)?,
                            cache_write_5m: row.get(10)?,
                            cache_write_1h: row.get(11)?,
                            cache_read: row.get(12)?,
                        },
                    },
                    row.get::<_, String>(3)?,
                ))
            })?;
        for row in rows {
            let (mut group, bucket_ts) = row?;
            let Some(minute) = nanos(&bucket_ts) else {
                continue;
            };
            group.minute = minute;
            groups.push(group);
        }
    }
    Ok(groups)
}

fn load_reviews(queue_db: &Path, from: i128, to: i128) -> Result<Vec<LandedReview>> {
    if !queue_db.exists() {
        return Ok(Vec::new());
    }
    let conn = open_read_only(queue_db)?;
    if !table_exists(&conn, "codex_review_request_registrations")? {
        return Ok(Vec::new());
    }
    let mut statement = conn.prepare(
        "SELECT repo, pr_number, requester_session_id, review_landed_at
           FROM codex_review_request_registrations
          WHERE review_landed_at IS NOT NULL",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(repo, pr, requester, landed)| {
            let landed_at = nanos(&landed)?;
            (from <= landed_at && landed_at < to).then_some(LandedReview {
                repo,
                pr,
                requester: requester.filter(|id| !id.trim().is_empty()),
                landed_at,
            })
        })
        .collect())
}

fn load_timeline(usage: &Connection, provider: &str) -> Result<Vec<TimelineSpan>> {
    if !table_exists(usage, "account_timeline")? {
        return Ok(Vec::new());
    }
    let mut statement = usage
        .prepare("SELECT account_key, from_ts, to_ts FROM account_timeline WHERE provider = ?1")?;
    let rows = statement
        .query_map([provider], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(account_key, from, to)| {
            Some(TimelineSpan {
                account_key,
                from: nanos(&from)?,
                to: match to {
                    Some(to) => Some(nanos(&to)?),
                    None => None,
                },
            })
        })
        .collect())
}

/// `YYYY-MM-DDTHH:MM:SS`: compares correctly against stored RFC 3339 text
/// with any fractional digits.
fn lexical_bound(at: i128) -> String {
    let at = OffsetDateTime::from_unix_timestamp_nanos(at).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    at.format(time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second]"
    ))
    .unwrap_or_default()
}

pub(crate) fn format_nanos(at: i128) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(at)
        .map(|at| at.replace_nanosecond(0).unwrap_or(at))
        .ok()
        .and_then(|at| at.format(&Rfc3339).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
