//! The experiment record and its state machine (B.2, B.3), the reversals
//! the guard, the kill switch, a 👎 and the owner can cause, and the
//! server-run measurement (F.2).
//!
//! `planned → building → dark → live → measuring → kept | reverted`; shadow
//! experiments pass `shadowed → awaiting_owner` before building, proposals
//! go `planned → awaiting_owner → abandoned`. A switchless experiment, or
//! one with `change_class = rows`, reverts through `rolling_back` and
//! reaches `reverted` only once rollback is confirmed.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

use super::metrics::{self, Baseline, GuardSpec, MetricSpec, Sample};
use super::switches::{self, Switches};
use super::{
    goal_claims, linked_prs, tickets_under_goal, Actor, AutopmStore, Effect, Fail, Out, Refusal,
    Settings, DAY_MS, HOUR_MS,
};

pub const CHANGE_CLASSES: [&str; 4] = ["rows", "outbound", "agent_text", "scripts"];
const PRE_LIVE: [&str; 5] = ["planned", "building", "shadowed", "awaiting_owner", "dark"];
/// States that count toward `max_live`.
const LIVE_STATES: [&str; 3] = ["live", "measuring", "rolling_back"];
const KEPT_SWITCH_DAYS: i64 = 30;
const MAX_PROPOSAL_WORDS: usize = 400;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Experiment {
    pub id: String,
    pub cycle_ticket: i64,
    pub title: String,
    pub hypothesis: String,
    pub path: String,
    pub target_repo: Option<String>,
    pub switch_key: Option<String>,
    pub change_class: Option<String>,
    pub plan_path: Option<String>,
    pub plan_pr: Option<i64>,
    pub plan_commit: Option<String>,
    pub plan_reviewed_sha: Option<String>,
    pub tickets_json: Value,
    pub metric_json: Option<Value>,
    pub guards_json: Option<Value>,
    pub proposal_md: Option<String>,
    pub window_days: i64,
    pub baseline_until_ms: Option<i64>,
    pub baseline_json: Option<Value>,
    pub live_at_ms: Option<i64>,
    pub measure_at_ms: Option<i64>,
    pub extended: bool,
    pub state: String,
    pub decision_note: Option<String>,
    pub votes_up: i64,
    pub votes_down: i64,
    pub points_est_json: Option<Value>,
    pub points_spent_json: Option<Value>,
    pub revert_commit: Option<String>,
    pub restore_ok_at_ms: Option<i64>,
    pub kept_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl Experiment {
    pub fn tickets(&self) -> Vec<i64> {
        self.tickets_json
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_i64)
            .collect()
    }

    /// Reverts through `rolling_back`: no switch, or rows rewritten.
    pub fn rolls_back(&self) -> bool {
        self.switch_key.is_none() || self.change_class.as_deref() == Some("rows")
    }

    /// The measurement window: `window_days`, plus 14 once extended.
    pub fn window(&self) -> i64 {
        self.window_days + if self.extended { 14 } else { 0 }
    }
}

const COLUMNS: &str = "id, cycle_ticket, title, hypothesis, path, target_repo, switch_key,
    change_class, plan_path, plan_pr, plan_commit, plan_reviewed_sha, tickets_json, metric_json,
    guards_json, proposal_md, window_days, baseline_until_ms, baseline_json, live_at_ms,
    measure_at_ms, extended, state, decision_note, votes_up, votes_down, points_est_json,
    points_spent_json, revert_commit, restore_ok_at_ms, kept_at_ms, created_at_ms, updated_at_ms";

fn json_col(text: Option<String>) -> Option<Value> {
    text.and_then(|text| serde_json::from_str(&text).ok())
}

fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Experiment> {
    Ok(Experiment {
        id: row.get(0)?,
        cycle_ticket: row.get(1)?,
        title: row.get(2)?,
        hypothesis: row.get(3)?,
        path: row.get(4)?,
        target_repo: row.get(5)?,
        switch_key: row.get(6)?,
        change_class: row.get(7)?,
        plan_path: row.get(8)?,
        plan_pr: row.get(9)?,
        plan_commit: row.get(10)?,
        plan_reviewed_sha: row.get(11)?,
        tickets_json: json_col(row.get(12)?).unwrap_or_else(|| json!([])),
        metric_json: json_col(row.get(13)?),
        guards_json: json_col(row.get(14)?),
        proposal_md: row.get(15)?,
        window_days: row.get(16)?,
        baseline_until_ms: row.get(17)?,
        baseline_json: json_col(row.get(18)?),
        live_at_ms: row.get(19)?,
        measure_at_ms: row.get(20)?,
        extended: row.get(21)?,
        state: row.get(22)?,
        decision_note: row.get(23)?,
        votes_up: row.get(24)?,
        votes_down: row.get(25)?,
        points_est_json: json_col(row.get(26)?),
        points_spent_json: json_col(row.get(27)?),
        revert_commit: row.get(28)?,
        restore_ok_at_ms: row.get(29)?,
        kept_at_ms: row.get(30)?,
        created_at_ms: row.get(31)?,
        updated_at_ms: row.get(32)?,
    })
}

pub fn get(conn: &Connection, id: &str) -> rusqlite::Result<Option<Experiment>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM autopm_experiments WHERE id = ?1"),
        [id],
        from_row,
    )
    .optional()
}

pub fn list(conn: &Connection) -> rusqlite::Result<Vec<Experiment>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM autopm_experiments ORDER BY created_at_ms, id"
    ))?;
    let rows = stmt.query_map([], from_row)?;
    rows.collect()
}

fn require(conn: &Connection, id: &str) -> Out<Experiment> {
    get(conn, id)?.ok_or_else(|| Refusal::not_found(format!("no experiment {id}")).into())
}

/// A pull request as the gates need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pull {
    /// `open`, `closed` or `merged`.
    pub state: String,
    pub base: String,
    pub head_sha: String,
    pub merge_commit: Option<String>,
}

/// GitHub and git as the gates need them, in the session-manager repo.
pub trait Git: Send + Sync {
    fn pull(&self, pr: i64) -> Result<Pull, String>;
    /// A file's text at a commit; `None` when it does not exist there.
    fn file_at(&self, path: &str, commit: &str) -> Result<Option<String>, String>;
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, String>;
    fn close_issue(&self, number: i64, comment: &str) -> Result<(), String>;
}

/// Everything a call needs: the store, the live switches, GitHub, the
/// owner's settings, the running build's commit and the time.
pub struct Ctx<'a> {
    pub store: &'a AutopmStore,
    pub switches: &'a Switches,
    pub git: &'a dyn Git,
    pub settings: &'a Settings,
    /// `SM_BUILD_SHA`: the commit the running server was built from.
    pub build_sha: Option<&'a str>,
    pub now: i64,
}

fn bad(gate: &str, detail: impl Into<String>) -> Fail {
    Refusal::bad(gate, detail).into()
}

fn refuse(gate: &str, detail: impl Into<String>) -> Fail {
    Refusal::gate(gate, detail).into()
}

fn valid_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("x-") else {
        return false;
    };
    let Some((cycle, slug)) = rest.split_once('-') else {
        return false;
    };
    let cycle = cycle.as_bytes();
    id.len() <= 80
        && cycle.len() == 7
        && cycle[..4].iter().all(u8::is_ascii_digit)
        && cycle[4] == b'w'
        && cycle[5..].iter().all(u8::is_ascii_digit)
        && !slug.is_empty()
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn valid_repo(repo: &str) -> bool {
    repo.split_once('/').is_some_and(|(owner, name)| {
        [owner, name].iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
    })
}

fn text_field(body: &Map<String, Value>, key: &str) -> Out<Option<String>> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text.trim().to_owned())),
        Some(_) => Err(bad("invalid", format!("{key} must be a non-empty string"))),
    }
}

fn int_field(body: &Map<String, Value>, key: &str) -> Out<Option<i64>> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .filter(|n| *n > 0)
            .map(Some)
            .ok_or_else(|| bad("invalid", format!("{key} must be a positive integer"))),
    }
}

fn tickets_field(value: &Value) -> Out<Value> {
    let tickets = value
        .as_array()
        .filter(|tickets| tickets.iter().all(|t| t.as_i64().is_some_and(|n| n > 0)))
        .ok_or_else(|| bad("invalid", "tickets_json must be a list of issue numbers"))?;
    let unique: BTreeSet<i64> = tickets.iter().filter_map(Value::as_i64).collect();
    Ok(json!(unique.into_iter().collect::<Vec<_>>()))
}

fn points_field(value: &Value) -> Out<Value> {
    let object = value
        .as_object()
        .filter(|object| {
            !object.is_empty()
                && object.iter().all(|(provider, points)| {
                    super::PROVIDERS.contains(&provider.as_str())
                        && points.as_f64().is_some_and(|p| p.is_finite() && p >= 0.0)
                })
        })
        .ok_or_else(|| {
            bad(
                "invalid",
                "points_est_json must map claude and codex to points",
            )
        })?;
    Ok(Value::Object(object.clone()))
}

fn window_field(value: &Value) -> Out<i64> {
    value
        .as_i64()
        .filter(|days| *days >= 14 && days % 2 == 0)
        .ok_or_else(|| bad("invalid", "window_days must be even and at least 14"))
}

const CREATE_FIELDS: [&str; 16] = [
    "id",
    "cycle_ticket",
    "title",
    "hypothesis",
    "path",
    "target_repo",
    "switch_key",
    "change_class",
    "plan_path",
    "plan_pr",
    "tickets_json",
    "metric_json",
    "guards_json",
    "proposal_md",
    "window_days",
    "points_est_json",
];
const PROPOSAL_FIELDS: [&str; 7] = [
    "id",
    "cycle_ticket",
    "title",
    "hypothesis",
    "path",
    "target_repo",
    "proposal_md",
];

fn changed_by(actor: Actor) -> &'static str {
    match actor {
        Actor::Owner => "owner",
        Actor::Autopm => "autopm",
        Actor::Server => "autopm",
    }
}

/// `POST /autopm/experiments` (B.2, B.3). Creates the switch row, off,
/// when the experiment has a `switch_key`; the key must be new, so an
/// experiment can only ever flip the switch it created.
pub fn create(ctx: &Ctx<'_>, actor: Actor, body: &Value) -> Out<Experiment> {
    if ctx.settings.paused_at.is_some() {
        return Err(refuse(
            "paused",
            "the auto PM is paused; Resume in Settings first",
        ));
    }
    let body = body
        .as_object()
        .ok_or_else(|| bad("invalid", "body must be a JSON object"))?;
    if let Some(unknown) = body.keys().find(|k| !CREATE_FIELDS.contains(&k.as_str())) {
        return Err(bad("unknown_field", format!("unknown field {unknown}")));
    }
    let id = text_field(body, "id")?.ok_or_else(|| bad("invalid", "id is required"))?;
    if !valid_id(&id) {
        return Err(bad(
            "invalid",
            "id must be x-<year>w<week>-<slug>, e.g. x-2026w43-decision-card",
        ));
    }
    let cycle_ticket = int_field(body, "cycle_ticket")?
        .ok_or_else(|| bad("invalid", "cycle_ticket is required"))?;
    let title = text_field(body, "title")?.ok_or_else(|| bad("invalid", "title is required"))?;
    let hypothesis =
        text_field(body, "hypothesis")?.ok_or_else(|| bad("invalid", "hypothesis is required"))?;
    let path = text_field(body, "path")?.unwrap_or_default();
    let present = |key: &str| body.get(key).is_some_and(|v| !v.is_null());
    let mut row = Experiment {
        id: id.clone(),
        cycle_ticket,
        title,
        hypothesis,
        path: path.clone(),
        target_repo: None,
        switch_key: None,
        change_class: None,
        plan_path: None,
        plan_pr: None,
        plan_commit: None,
        plan_reviewed_sha: None,
        tickets_json: json!([]),
        metric_json: None,
        guards_json: None,
        proposal_md: None,
        window_days: 14,
        baseline_until_ms: None,
        baseline_json: None,
        live_at_ms: None,
        measure_at_ms: None,
        extended: false,
        state: "planned".into(),
        decision_note: None,
        votes_up: 0,
        votes_down: 0,
        points_est_json: None,
        points_spent_json: None,
        revert_commit: None,
        restore_ok_at_ms: None,
        kept_at_ms: None,
        created_at_ms: ctx.now,
        updated_at_ms: ctx.now,
    };
    match path.as_str() {
        "proposal" => {
            if let Some(field) = CREATE_FIELDS
                .iter()
                .find(|key| !PROPOSAL_FIELDS.contains(key) && present(key))
            {
                return Err(bad(
                    "proposal_fields",
                    format!("a proposal takes no {field}"),
                ));
            }
            let repo = text_field(body, "target_repo")?
                .ok_or_else(|| bad("invalid", "a proposal needs target_repo"))?;
            if !valid_repo(&repo) || crate::work_claims::canonical_repo(&repo) == super::REPO {
                return Err(bad(
                    "invalid",
                    "target_repo must be owner/name, not session-manager",
                ));
            }
            let text = text_field(body, "proposal_md")?
                .ok_or_else(|| bad("invalid", "a proposal needs proposal_md"))?;
            if text.split_whitespace().count() > MAX_PROPOSAL_WORDS {
                return Err(bad("invalid", "proposal_md is at most 400 words"));
            }
            row.target_repo = Some(repo);
            row.proposal_md = Some(text);
        }
        "switch" | "shadow" => {
            for field in ["target_repo", "proposal_md"] {
                if present(field) {
                    return Err(bad("invalid", format!("{field} is for proposals only")));
                }
            }
            row.switch_key = text_field(body, "switch_key")?;
            match (&row.switch_key, path.as_str()) {
                (None, "switch") => {
                    return Err(bad("invalid", "a switch experiment needs switch_key"))
                }
                (Some(key), _) if !switches::valid_key(key) => {
                    return Err(bad("invalid", "switch_key must be autopm.<slug>"))
                }
                _ => {}
            }
            row.change_class = text_field(body, "change_class")?;
            match (row.change_class.as_deref(), path.as_str()) {
                (Some(_), "switch") => {
                    return Err(bad("invalid", "change_class is for shadow experiments"))
                }
                (None, "shadow") => return Err(bad(
                    "invalid",
                    "a shadow experiment needs change_class: rows, outbound, agent_text or scripts",
                )),
                (Some(class), _) if !CHANGE_CLASSES.contains(&class) => {
                    return Err(bad(
                        "invalid",
                        "change_class must be rows, outbound, agent_text or scripts",
                    ))
                }
                _ => {}
            }
            let plan_path = text_field(body, "plan_path")?
                .ok_or_else(|| bad("invalid", "plan_path is required"))?;
            if plan_path != format!("docs/autopm/plans/{id}.html") {
                return Err(bad(
                    "invalid",
                    format!("plan_path must be docs/autopm/plans/{id}.html"),
                ));
            }
            row.plan_path = Some(plan_path);
            row.plan_pr = Some(
                int_field(body, "plan_pr")?.ok_or_else(|| bad("invalid", "plan_pr is required"))?,
            );
            let metric = body
                .get("metric_json")
                .filter(|v| !v.is_null())
                .ok_or_else(|| bad("invalid", "metric_json is required"))?;
            metrics::parse_metric(metric).map_err(|e| bad("invalid", e))?;
            row.metric_json = Some(metric.clone());
            let guards = body
                .get("guards_json")
                .filter(|v| !v.is_null())
                .ok_or_else(|| bad("invalid", "guards_json is required (it may be [])"))?;
            metrics::parse_guards(guards).map_err(|e| bad("invalid", e))?;
            row.guards_json = Some(guards.clone());
            row.points_est_json = Some(points_field(
                body.get("points_est_json")
                    .filter(|v| !v.is_null())
                    .ok_or_else(|| bad("invalid", "points_est_json is required"))?,
            )?);
            if let Some(window) = body.get("window_days").filter(|v| !v.is_null()) {
                row.window_days = window_field(window)?;
            }
            if let Some(tickets) = body.get("tickets_json").filter(|v| !v.is_null()) {
                row.tickets_json = tickets_field(tickets)?;
            }
        }
        _ => {
            return Err(bad("invalid", "path must be switch, shadow or proposal"));
        }
    }
    let mut conn = ctx.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if get(&tx, &id)?.is_some() {
        return Err(refuse("exists", format!("experiment {id} already exists")));
    }
    if let Some(key) = &row.switch_key {
        if switches::get(&tx, key)?.is_some() {
            return Err(refuse(
                "switch_exists",
                format!("switch {key} already exists"),
            ));
        }
        tx.execute(
            "INSERT INTO experiment_switches(key, experiment_id, on_, held, changed_at_ms, changed_by, reason)
             VALUES (?1, ?2, 0, 0, ?3, ?4, ?5)",
            params![key, id, ctx.now, changed_by(actor), format!("created by {id}")],
        )?;
    }
    insert(&tx, &row)?;
    tx.commit()?;
    if row.switch_key.is_some() {
        ctx.switches.reload(&conn)?;
    }
    require(&conn, &id)
}

fn insert(conn: &Connection, x: &Experiment) -> rusqlite::Result<()> {
    let text = |v: &Option<Value>| v.as_ref().map(Value::to_string);
    conn.execute(
        "INSERT INTO autopm_experiments (id, cycle_ticket, title, hypothesis, path, target_repo,
           switch_key, change_class, plan_path, plan_pr, tickets_json, metric_json, guards_json,
           proposal_md, window_days, state, points_est_json, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?18)",
        params![
            x.id,
            x.cycle_ticket,
            x.title,
            x.hypothesis,
            x.path,
            x.target_repo,
            x.switch_key,
            x.change_class,
            x.plan_path,
            x.plan_pr,
            x.tickets_json.to_string(),
            text(&x.metric_json),
            text(&x.guards_json),
            x.proposal_md,
            x.window_days,
            x.state,
            text(&x.points_est_json),
            x.created_at_ms,
        ],
    )?;
    Ok(())
}

fn set_state(
    conn: &Connection,
    id: &str,
    state: &str,
    note: Option<&str>,
    now: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE autopm_experiments SET state = ?2,
           decision_note = COALESCE(?3, decision_note), updated_at_ms = ?4 WHERE id = ?1",
        params![id, state, note, now],
    )?;
    Ok(())
}

/// `plan_merged` (B.3): the plan PR merged into main, the last
/// `sm request-review` run on it completed at the PR's final head, and the
/// plan file at the merge commit names the experiment. The result is
/// recorded once it holds.
pub fn plan_merged(ctx: &Ctx<'_>, x: &Experiment) -> Out<(String, String)> {
    if let (Some(commit), Some(reviewed)) = (&x.plan_commit, &x.plan_reviewed_sha) {
        return Ok((commit.clone(), reviewed.clone()));
    }
    let (Some(pr), Some(plan_path)) = (x.plan_pr, x.plan_path.as_deref()) else {
        return Err(refuse("plan_merged", "a proposal has no plan"));
    };
    let pull = ctx
        .git
        .pull(pr)
        .map_err(|e| refuse("plan_merged", format!("cannot read PR #{pr}: {e}")))?;
    let merge = match (&pull.state[..], &pull.merge_commit) {
        ("merged", Some(merge)) if pull.base == "main" => merge.clone(),
        _ => {
            return Err(refuse(
                "plan_merged",
                format!("plan PR #{pr} is not merged into main"),
            ))
        }
    };
    let conn = ctx.store.open()?;
    let review: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT state, requested_head_sha FROM codex_review_request_registrations
             WHERE repo = ?1 AND pr_number = ?2 ORDER BY requested_at DESC, rowid DESC LIMIT 1",
            params![super::REPO, pr],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .or_else(|error| match &error {
            rusqlite::Error::SqliteFailure(_, Some(text)) if text.contains("no such table") => {
                Ok(None)
            }
            _ => Err(error),
        })?;
    if !matches!(&review, Some((state, Some(head))) if state == "completed" && *head == pull.head_sha)
    {
        return Err(refuse(
            "plan_merged",
            format!(
                "the last sm request-review run on PR #{pr} did not complete at its final head {}",
                &pull.head_sha[..pull.head_sha.len().min(7)]
            ),
        ));
    }
    let text = ctx
        .git
        .file_at(plan_path, &merge)
        .map_err(|e| refuse("plan_merged", format!("cannot read {plan_path}: {e}")))?;
    if !text.is_some_and(|text| text.contains(&x.id)) {
        return Err(refuse(
            "plan_merged",
            format!("{plan_path} at the merge commit does not name {}", x.id),
        ));
    }
    conn.execute(
        "UPDATE autopm_experiments SET plan_commit = ?2, plan_reviewed_sha = ?3 WHERE id = ?1",
        params![x.id, merge, pull.head_sha],
    )?;
    Ok((merge, pull.head_sha))
}

/// `POST /autopm/issues` for an experiment (B.3): its plan merged, the body
/// names it, and it is still being planned or built. sm#2175's route calls this.
pub fn gate_issue(ctx: &Ctx<'_>, id: &str, body: &str) -> Out<()> {
    let x = require(&ctx.store.open()?, id)?;
    plan_merged(ctx, &x)?;
    if !body.contains(id) {
        return Err(refuse(
            "names_experiment",
            format!("the issue body must name {id}"),
        ));
    }
    if !matches!(x.state.as_str(), "planned" | "building") {
        return Err(refuse(
            "state",
            format!(
                "{id} is {}; issues are filed while planned or building",
                x.state
            ),
        ));
    }
    Ok(())
}

/// `POST /autopm/issues {kind: "fix"}` (B.3, B.6): no experiment, a body
/// stating "side effects: none", and a `Tier:` line.
pub fn gate_fix_issue(body: &str) -> Result<(), Refusal> {
    if !body.to_ascii_lowercase().contains("side effects: none") {
        return Err(Refusal::gate(
            "side_effects",
            "a fix must state \"side effects: none\"",
        ));
    }
    if !body
        .lines()
        .any(|line| line.trim_start().starts_with("Tier:"))
    {
        return Err(Refusal::gate("tier", "a fix needs a Tier: line"));
    }
    Ok(())
}

fn live_count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM autopm_experiments WHERE state IN ('live', 'measuring', 'rolling_back')",
        [],
        |row| row.get(0),
    )
}

/// Reads one committed query at the plan's merge commit.
fn query_text(ctx: &Ctx<'_>, commit: &str, path: &str) -> Out<String> {
    ctx.git
        .file_at(path, commit)
        .map_err(|e| refuse("metric_query", format!("cannot read {path}: {e}")))?
        .ok_or_else(|| refuse("metric_query", format!("{path} is not in the plan commit")))
}

fn specs(x: &Experiment) -> Out<(MetricSpec, Vec<GuardSpec>)> {
    let metric = metrics::parse_metric(x.metric_json.as_ref().unwrap_or(&Value::Null))
        .map_err(|e| refuse("metric_json", e))?;
    let guards = metrics::parse_guards(x.guards_json.as_ref().unwrap_or(&json!([])))
        .map_err(|e| refuse("guards_json", e))?;
    Ok((metric, guards))
}

/// The baseline over the 14 days before `until` (F.2), refused when the
/// metric's stat is zero or undefined.
fn compute_baseline(ctx: &Ctx<'_>, x: &Experiment, commit: &str, until: i64) -> Out<Baseline> {
    let (metric, guards) = specs(x)?;
    let from = until - metrics::BASELINE_DAYS * DAY_MS;
    let run = |path: &str, stat| -> Out<Sample> {
        let sql = query_text(ctx, commit, path)?;
        let rows = metrics::run_query(&ctx.store.paths, &sql, from, until)
            .map_err(|e| refuse("metric_query", format!("{path}: {e}")))?;
        Ok(metrics::sample(stat, &rows, from, until))
    };
    let baseline = run(&metric.query, metric.stat)?;
    if baseline.stat.is_none_or(|stat| stat == 0.0) {
        return Err(refuse(
            "baseline_zero",
            "the metric's baseline is zero, so no change can be measured",
        ));
    }
    let guards = guards
        .iter()
        .map(|guard| run(&guard.query, guard.stat))
        .collect::<Out<Vec<_>>>()?;
    Ok(Baseline {
        metric: baseline,
        guards,
    })
}

/// `dark → live`, by switch or by `POST /autopm/experiments/{id}/live`.
fn go_live(ctx: &Ctx<'_>, actor: Actor, x: &Experiment, switch_on: bool) -> Out<Experiment> {
    if !ctx.settings.enabled {
        return Err(refuse(
            "autopm_disabled",
            "Auto PM is off; turn it on first",
        ));
    }
    if x.state != "dark" {
        return Err(refuse(
            "not_dark",
            format!("{} is {}, not dark", x.id, x.state),
        ));
    }
    let (commit, _) = plan_merged(ctx, x)?;
    if live_count(&ctx.store.open()?)? >= ctx.settings.max_live {
        return Err(refuse(
            "max_live",
            format!("{} experiments are already live", ctx.settings.max_live),
        ));
    }
    let baseline = match &x.baseline_json {
        Some(_) => None,
        None => Some(compute_baseline(ctx, x, &commit, ctx.now)?),
    };
    let mut conn = ctx.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = require(&tx, &x.id)?;
    if current.state != "dark" {
        return Err(refuse(
            "not_dark",
            format!("{} is {}, not dark", x.id, current.state),
        ));
    }
    if live_count(&tx)? >= ctx.settings.max_live {
        return Err(refuse("max_live", "the live limit was reached meanwhile"));
    }
    if switch_on {
        let key = x.switch_key.as_deref().expect("switch experiment");
        switches::write(
            &tx,
            key,
            true,
            Some(false),
            changed_by(actor),
            Some("switched on"),
            ctx.now,
        )?;
    }
    tx.execute(
        "UPDATE autopm_experiments SET state = 'live', live_at_ms = ?2, measure_at_ms = ?3,
           extended = 0, baseline_until_ms = COALESCE(baseline_until_ms, ?2),
           baseline_json = COALESCE(baseline_json, ?4), updated_at_ms = ?2
         WHERE id = ?1",
        params![
            x.id,
            ctx.now,
            ctx.now + x.window_days * DAY_MS,
            baseline.map(|b| serde_json::to_string(&b).unwrap_or_default()),
        ],
    )?;
    tx.commit()?;
    if switch_on {
        ctx.switches.reload(&conn)?;
    }
    require(&conn, &x.id)
}

/// `POST /autopm/experiments/{id}/live`: a shadow experiment with no switch,
/// which only the owner's yes brought past `awaiting_owner`.
pub fn go_live_switchless(ctx: &Ctx<'_>, actor: Actor, id: &str) -> Out<Experiment> {
    let x = require(&ctx.store.open()?, id)?;
    if x.path != "shadow" || x.switch_key.is_some() {
        return Err(refuse(
            "has_switch",
            "only an approved shadow experiment without a switch goes live this way",
        ));
    }
    go_live(ctx, actor, &x, false)
}

/// Undoes a `live`, `measuring` or `kept` experiment. A switched change is
/// `reverted` at once; one that rolls back goes to `rolling_back` (its
/// switch, if any, off at once). `held` marks a switch only the owner may
/// turn back on.
fn reverse(
    conn: &Connection,
    x: &Experiment,
    cause: &str,
    by: &str,
    held: bool,
    now: i64,
) -> Out<Vec<Effect>> {
    if let Some(key) = &x.switch_key {
        switches::write(conn, key, false, held.then_some(true), by, Some(cause), now)?;
    }
    let state = if x.rolls_back() {
        "rolling_back"
    } else {
        "reverted"
    };
    let note = match &x.decision_note {
        Some(note) if !note.is_empty() => format!("{note}\n{cause}"),
        _ => cause.to_owned(),
    };
    set_state(conn, &x.id, state, Some(&note), now)?;
    Ok(vec![Effect::WakeAutopm {
        text: format!("[sm autopm] {} {state}: {cause}", x.id),
    }])
}

/// Live claim holders on autopm tickets that get stop messages: every one
/// but an agent on a `kind=revert` ticket, which no limit stops (C.4).
pub fn stoppable_holders(conn: &Connection) -> Out<Vec<(i64, String)>> {
    let under = tickets_under_goal(conn)?;
    let mut holders = Vec::new();
    let mut seen = BTreeSet::new();
    for claim in goal_claims(conn, &under)? {
        if !claim.live || super::fix(conn, claim.number)?.is_some_and(|f| f.kind == "revert") {
            continue;
        }
        if seen.insert(claim.session_id.clone()) {
            holders.push((claim.number, claim.session_id));
        }
    }
    Ok(holders)
}

/// `PUT /autopm/switches/{key} {on, reason}` (B.1).
pub fn set_switch(
    ctx: &Ctx<'_>,
    actor: Actor,
    key: &str,
    on: bool,
    reason: Option<&str>,
) -> Out<(switches::SwitchRow, Vec<Effect>)> {
    let conn = ctx.store.open()?;
    let row =
        switches::get(&conn, key)?.ok_or_else(|| Refusal::not_found(format!("no switch {key}")))?;
    let x = require(&conn, &row.experiment_id)?;
    drop(conn);
    let mut effects = Vec::new();
    match (actor, on) {
        (Actor::Owner, true) if x.state == "reverted" => {
            effects = reenable(ctx, &x, key)?;
        }
        (_, true) if x.state == "dark" => {
            if actor != Actor::Owner && row.held {
                return Err(refuse("held", "only Rajesh can turn this back on"));
            }
            go_live(ctx, actor, &x, true)?;
        }
        (Actor::Owner, true) => {
            let conn = ctx.store.open()?;
            switches::write(&conn, key, true, Some(false), "owner", reason, ctx.now)?;
            ctx.switches.reload(&conn)?;
        }
        (_, true) => {
            if row.held {
                return Err(refuse("held", "only Rajesh can turn this back on"));
            }
            return Err(refuse(
                "not_dark",
                format!("{} is {}, not dark", x.id, x.state),
            ));
        }
        (_, false) => {
            let mut conn = ctx.store.open()?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let x = require(&tx, &x.id)?;
            let by = changed_by(actor);
            if matches!(x.state.as_str(), "live" | "measuring" | "kept") {
                let cause = if actor == Actor::Owner {
                    "Rajesh switched it off"
                } else {
                    "auto PM switched off"
                };
                effects = reverse(&tx, &x, cause, by, false, ctx.now)?;
            } else {
                switches::write(&tx, key, false, None, by, reason, ctx.now)?;
            }
            tx.commit()?;
            ctx.switches.reload(&conn)?;
        }
    }
    let conn = ctx.store.open()?;
    let row = switches::get(&conn, key)?.expect("switch row");
    Ok((row, effects))
}

/// The owner turns a reverted experiment's switch back on (B.1): it is live
/// again, compared with the same frozen baseline, and any cleanup ticket
/// filed to remove it is cancelled.
fn reenable(ctx: &Ctx<'_>, x: &Experiment, key: &str) -> Out<Vec<Effect>> {
    if !ctx.settings.enabled {
        return Err(refuse(
            "autopm_disabled",
            "Auto PM is off; turn Auto PM on first",
        ));
    }
    let mut conn = ctx.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    switches::write(
        &tx,
        key,
        true,
        Some(false),
        "owner",
        Some("Rajesh re-enabled the change"),
        ctx.now,
    )?;
    tx.execute(
        "UPDATE autopm_experiments SET state = 'live', live_at_ms = ?2, measure_at_ms = ?3,
           extended = 0, kept_at_ms = NULL, updated_at_ms = ?2 WHERE id = ?1",
        params![x.id, ctx.now, ctx.now + x.window_days * DAY_MS],
    )?;
    tx.execute(
        "UPDATE autopm_measurements SET superseded_at_ms = ?2
         WHERE experiment_id = ?1 AND superseded_at_ms IS NULL",
        params![x.id, ctx.now],
    )?;
    let cleanups: Vec<i64> = {
        let mut stmt = tx.prepare(
            "SELECT number FROM autopm_fixes WHERE repo = ?1 AND experiment_id = ?2
               AND kind = 'cleanup' AND cancelled_at_ms IS NULL",
        )?;
        let rows = stmt.query_map(params![super::REPO, x.id], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut effects = vec![Effect::WakeAutopm {
        text: format!("[sm autopm] Rajesh re-enabled {}: live again", x.id),
    }];
    for number in cleanups {
        tx.execute(
            "UPDATE autopm_fixes SET cancelled_at_ms = ?3 WHERE repo = ?1 AND number = ?2",
            params![super::REPO, number, ctx.now],
        )?;
        effects.extend(cancelled_cleanup_holds(&tx, number)?);
        effects.push(Effect::CloseIssue {
            number,
            comment: "Rajesh re-enabled the change".into(),
        });
        effects.push(Effect::CancelAutoStart {
            number,
            reason: "Rajesh re-enabled the change".into(),
        });
        for session_id in super::live_holders(&tx, number)? {
            effects.push(Effect::Message {
                session_id,
                text: "[sm autopm] cleanup cancelled: Rajesh re-enabled the change. Close your PR unmerged and stop".into(),
            });
        }
    }
    tx.commit()?;
    ctx.switches.reload(&conn)?;
    Ok(effects)
}

/// Merge holds for every PR linked to a cancelled cleanup ticket.
fn cancelled_cleanup_holds(conn: &Connection, number: i64) -> Out<Vec<Effect>> {
    Ok(linked_prs(conn, number)?
        .into_iter()
        .map(|pr| Effect::MergeHold {
            pr,
            reason: format!("cleanup #{number} was cancelled: Rajesh re-enabled the change"),
        })
        .collect())
}

/// Holds on PRs linked to cancelled cleanup tickets, including PRs opened
/// after the cancellation. The HTTP layer places each once.
pub fn cancelled_cleanup_prs(conn: &Connection) -> Out<Vec<Effect>> {
    let numbers: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT number FROM autopm_fixes WHERE repo = ?1 AND kind = 'cleanup'
               AND cancelled_at_ms IS NOT NULL",
        )?;
        let rows = stmt.query_map([super::REPO], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut effects = Vec::new();
    for number in numbers {
        effects.extend(cancelled_cleanup_holds(conn, number)?);
    }
    Ok(effects)
}

/// `PATCH /autopm/experiments/{id}`: state moves only along the arrows;
/// plan fields change only while planned; `restore_ok` records the restore
/// evidence of a rows experiment rolling back.
pub fn patch(
    ctx: &Ctx<'_>,
    actor: Actor,
    id: &str,
    body: &Value,
) -> Out<(Experiment, Vec<Effect>)> {
    const PLAN_FIELDS: [&str; 8] = [
        "title",
        "hypothesis",
        "plan_pr",
        "metric_json",
        "guards_json",
        "window_days",
        "points_est_json",
        "proposal_md",
    ];
    const FIELDS: [&str; 12] = [
        "state",
        "decision_note",
        "tickets_json",
        "restore_ok",
        "title",
        "hypothesis",
        "plan_pr",
        "metric_json",
        "guards_json",
        "window_days",
        "points_est_json",
        "proposal_md",
    ];
    let body = body
        .as_object()
        .ok_or_else(|| bad("invalid", "body must be a JSON object"))?;
    if let Some(unknown) = body.keys().find(|k| !FIELDS.contains(&k.as_str())) {
        return Err(bad("unknown_field", format!("unknown field {unknown}")));
    }
    let x = require(&ctx.store.open()?, id)?;
    let note = text_field(body, "decision_note")?;
    let target = text_field(body, "state")?;
    // A plan merge is checked before the write transaction: it reads GitHub.
    if target.as_deref() == Some("building") && x.state == "planned" {
        plan_merged(ctx, &x)?;
    }
    let mut conn = ctx.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let x = require(&tx, id)?;
    let mut effects = Vec::new();
    for field in PLAN_FIELDS.iter().filter(|f| body.contains_key(**f)) {
        if x.state != "planned" || x.plan_commit.is_some() {
            return Err(refuse(
                "fixed",
                format!("{field} is fixed once the plan merges"),
            ));
        }
        let proposal_only = *field == "proposal_md";
        let applies = if x.path == "proposal" {
            matches!(*field, "title" | "hypothesis" | "proposal_md")
        } else {
            !proposal_only
        };
        if !applies {
            return Err(bad(
                "proposal_fields",
                format!("{field} does not apply to this experiment"),
            ));
        }
        let value = &body[*field];
        let stored = match *field {
            "title" | "hypothesis" | "proposal_md" => {
                let text = text_field(body, field)?
                    .ok_or_else(|| bad("invalid", format!("{field} is required")))?;
                if proposal_only && text.split_whitespace().count() > MAX_PROPOSAL_WORDS {
                    return Err(bad("invalid", "proposal_md is at most 400 words"));
                }
                Value::String(text)
            }
            "plan_pr" => {
                json!(int_field(body, field)?.ok_or_else(|| bad("invalid", "plan_pr is required"))?)
            }
            "metric_json" => {
                metrics::parse_metric(value).map_err(|e| bad("invalid", e))?;
                Value::String(value.to_string())
            }
            "guards_json" => {
                metrics::parse_guards(value).map_err(|e| bad("invalid", e))?;
                Value::String(value.to_string())
            }
            "window_days" => json!(window_field(value)?),
            _ => Value::String(points_field(value)?.to_string()),
        };
        tx.execute(
            &format!("UPDATE autopm_experiments SET {field} = ?2 WHERE id = ?1"),
            rusqlite::params![id, sql_value(&stored)],
        )?;
    }
    if let Some(tickets) = body.get("tickets_json") {
        if x.path == "proposal" || !matches!(x.state.as_str(), "planned" | "building") {
            return Err(refuse(
                "fixed",
                "tickets are added while planned or building",
            ));
        }
        tx.execute(
            "UPDATE autopm_experiments SET tickets_json = ?2 WHERE id = ?1",
            params![id, tickets_field(tickets)?.to_string()],
        )?;
    }
    if let Some(restore) = body.get("restore_ok") {
        if restore != &Value::Bool(true) {
            return Err(bad("invalid", "restore_ok must be true"));
        }
        if x.state != "rolling_back" || x.change_class.as_deref() != Some("rows") {
            return Err(refuse(
                "restore",
                "restore evidence applies to a rows experiment rolling back",
            ));
        }
        let evidence = note
            .as_deref()
            .ok_or_else(|| bad("invalid", "restore_ok needs the evidence in decision_note"))?;
        tx.execute(
            "UPDATE autopm_experiments SET restore_ok_at_ms = ?2,
               decision_note = COALESCE(decision_note || char(10), '') || ?3 WHERE id = ?1",
            params![id, ctx.now, format!("restore: {evidence}")],
        )?;
    } else if let Some(note) = &note {
        tx.execute(
            "UPDATE autopm_experiments SET decision_note = ?2 WHERE id = ?1",
            params![id, note],
        )?;
    }
    if let Some(target) = target.filter(|target| *target != x.state) {
        let x = require(&tx, id)?;
        let has_note = x
            .decision_note
            .as_deref()
            .is_some_and(|n| !n.trim().is_empty());
        match (x.state.as_str(), target.as_str()) {
            ("planned", "building") if x.path != "proposal" => {
                set_state(&tx, id, "building", None, ctx.now)?;
            }
            ("planned", "awaiting_owner") if x.path == "proposal" => {
                set_state(&tx, id, "awaiting_owner", None, ctx.now)?;
            }
            ("building", "shadowed") if x.path == "shadow" => {
                set_state(&tx, id, "shadowed", None, ctx.now)?;
            }
            ("shadowed", "awaiting_owner") => {
                if !has_note {
                    return Err(refuse(
                        "evidence",
                        "record the shadow evidence in decision_note first",
                    ));
                }
                set_state(&tx, id, "awaiting_owner", None, ctx.now)?;
            }
            ("awaiting_owner", "building") if x.path == "shadow" => {
                if actor != Actor::Owner {
                    return Err(refuse(
                        "owner_answer",
                        "only Rajesh's yes moves a shadow experiment to building",
                    ));
                }
                set_state(&tx, id, "building", None, ctx.now)?;
            }
            (from, "abandoned") if PRE_LIVE.contains(&from) => {
                if !has_note {
                    return Err(refuse("note", "abandoning needs a decision_note"));
                }
                set_state(&tx, id, "abandoned", None, ctx.now)?;
            }
            (_, "abandoned") => {
                return Err(refuse(
                    "not_pre_live",
                    format!("{} is {}; it can leave only by reverted", x.id, x.state),
                ));
            }
            ("live" | "measuring" | "kept", "reverted") => {
                let cause = if actor == Actor::Owner {
                    "Rajesh asked to revert it"
                } else {
                    "auto PM reverted it"
                };
                effects = reverse(&tx, &x, cause, changed_by(actor), false, ctx.now)?;
            }
            ("building", "dark") | ("rolling_back", "reverted") | ("live", "measuring") => {
                return Err(refuse("server_only", "sm makes this move itself"));
            }
            ("dark", "live") => {
                return Err(refuse(
                    "use_switch",
                    "go live by turning the switch on, or POST /autopm/experiments/{id}/live",
                ));
            }
            _ => {
                return Err(refuse(
                    "transition",
                    format!("{} cannot move from {} to {target}", x.id, x.state),
                ));
            }
        }
    }
    tx.execute(
        "UPDATE autopm_experiments SET updated_at_ms = ?2 WHERE id = ?1",
        params![id, ctx.now],
    )?;
    tx.commit()?;
    if !effects.is_empty() {
        ctx.switches.reload(&conn)?;
    }
    Ok((require(&conn, id)?, effects))
}

/// A JSON scalar as a SQL value: strings stay text, numbers stay numbers.
pub(crate) fn sql_value(value: &Value) -> rusqlite::types::Value {
    match value {
        Value::String(text) => rusqlite::types::Value::Text(text.clone()),
        Value::Number(n) => n
            .as_i64()
            .map(rusqlite::types::Value::Integer)
            .unwrap_or_else(|| rusqlite::types::Value::Real(n.as_f64().unwrap_or(0.0))),
        Value::Null => rusqlite::types::Value::Null,
        other => rusqlite::types::Value::Text(other.to_string()),
    }
}

/// The owner's answer to an ask (E.2), for sm#2175's reply parser. A shadow
/// yes moves to `building`; a no, or a proposal answered either way, ends in
/// `abandoned` with `note` (for a proposal yes, "handed to <repo>#N").
pub fn answer(ctx: &Ctx<'_>, id: &str, yes: bool, note: &str) -> Out<Experiment> {
    let conn = ctx.store.open()?;
    let x = require(&conn, id)?;
    if x.state != "awaiting_owner" {
        return Err(refuse(
            "state",
            format!("{id} is {}, not awaiting an answer", x.state),
        ));
    }
    let state = if yes && x.path == "shadow" {
        "building"
    } else {
        "abandoned"
    };
    set_state(&conn, id, state, Some(note), ctx.now)?;
    require(&conn, id)
}

/// Experiments rolling back that have no `kind=revert` ticket yet: the hook
/// sm#2175 files and auto-starts revert tickets from.
pub fn revert_tickets_due(conn: &Connection) -> Out<Vec<Experiment>> {
    let mut due = Vec::new();
    for x in list(conn)?
        .into_iter()
        .filter(|x| x.state == "rolling_back")
    {
        let filed: bool = conn
            .query_row(
                "SELECT 1 FROM autopm_fixes WHERE experiment_id = ?1 AND kind = 'revert'",
                [&x.id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !filed {
            due.push(x);
        }
    }
    Ok(due)
}

/// `POST /client/autopm/feedback` (F.3). A 👎 turns the switch off at once
/// and holds it; on a switchless change it starts the rollback.
pub fn vote(
    ctx: &Ctx<'_>,
    id: &str,
    up: bool,
    note: Option<&str>,
) -> Out<(Experiment, Vec<Effect>)> {
    let mut conn = ctx.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let x = require(&tx, id)?;
    if x.live_at_ms.is_none() {
        return Err(refuse("not_live", format!("{id} has not gone live")));
    }
    tx.execute(
        "INSERT INTO autopm_votes(experiment_id, at_ms, vote, note) VALUES (?1, ?2, ?3, ?4)",
        params![id, ctx.now, if up { "up" } else { "down" }, note],
    )?;
    tx.execute(
        &format!(
            "UPDATE autopm_experiments SET {0} = {0} + 1, updated_at_ms = ?2 WHERE id = ?1",
            if up { "votes_up" } else { "votes_down" }
        ),
        params![id, ctx.now],
    )?;
    let mut effects = Vec::new();
    if !up {
        if matches!(x.state.as_str(), "live" | "measuring" | "kept") {
            effects = reverse(&tx, &x, "Rajesh voted 👎", "feedback", true, ctx.now)?;
        } else {
            effects.push(Effect::WakeAutopm {
                text: format!("[sm autopm] Rajesh voted 👎 on {id} ({})", x.state),
            });
        }
    }
    tx.commit()?;
    ctx.switches.reload(&conn)?;
    Ok((require(&conn, id)?, effects))
}

/// The kill switch (B.5): every switch that is on turns off and is held,
/// its live experiment reverts, and the auto PM and every agent on an
/// autopm ticket are told to stop.
pub fn disable(ctx: &Ctx<'_>) -> Out<Vec<Effect>> {
    let mut conn = ctx.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let on: Vec<switches::SwitchRow> = switches::load(&tx)?.into_iter().filter(|r| r.on).collect();
    for row in &on {
        let x = require(&tx, &row.experiment_id)?;
        if matches!(x.state.as_str(), "live" | "measuring") {
            reverse(&tx, &x, "Auto PM turned off", "killswitch", true, ctx.now)?;
        } else {
            switches::write(
                &tx,
                &row.key,
                false,
                Some(true),
                "killswitch",
                Some("Auto PM turned off"),
                ctx.now,
            )?;
        }
    }
    let text =
        "[sm autopm] disabled by Rajesh: push work in progress, comment on your ticket, stop";
    let mut effects = vec![Effect::WakeAutopm { text: text.into() }];
    for (_, session_id) in stoppable_holders(&tx)? {
        effects.push(Effect::Message {
            session_id,
            text: text.into(),
        });
    }
    tx.commit()?;
    ctx.switches.reload(&conn)?;
    Ok(effects)
}

/// The guard's action (B.4): every switch turned on within the last hour
/// turns off and is held; older ones are the auto PM's to diagnose.
pub fn guard_trip(ctx: &Ctx<'_>, trips: &[super::guard::Trip]) -> Out<Vec<Effect>> {
    let Some(trip) = trips.first() else {
        return Ok(Vec::new());
    };
    let mut conn = ctx.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let recent: Vec<switches::SwitchRow> = switches::load(&tx)?
        .into_iter()
        .filter(|r| r.on && r.changed_at_ms >= ctx.now - super::guard::SCOPE_MS)
        .collect();
    if recent.is_empty() {
        return Ok(Vec::new());
    }
    let cause = format!("{} {} > {}", trip.counter, trip.value, trip.threshold);
    for row in &recent {
        let x = require(&tx, &row.experiment_id)?;
        let reason = format!("guard: {cause}");
        if matches!(x.state.as_str(), "live" | "measuring") {
            reverse(&tx, &x, &reason, "guard", true, ctx.now)?;
        } else {
            switches::write(
                &tx,
                &row.key,
                false,
                Some(true),
                "guard",
                Some(&reason),
                ctx.now,
            )?;
        }
    }
    tx.commit()?;
    ctx.switches.reload(&conn)?;
    let keys: Vec<&str> = recent.iter().map(|r| r.key.as_str()).collect();
    Ok(vec![Effect::WakeAutopm {
        text: format!("[sm autopm] guard reverted {}: {cause}", keys.join(", ")),
    }])
}

/// Compares dotted version numbers; `None` when either is not numeric.
pub fn version_at_least(have: &str, need: &str) -> Option<bool> {
    let parse = |v: &str| {
        v.trim()
            .trim_start_matches('v')
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()
    };
    let (mut have, mut need) = (parse(have)?, parse(need)?);
    let len = have.len().max(need.len());
    have.resize(len, 0);
    need.resize(len, 0);
    Some(have >= need)
}

/// `building → dark` (B.3): every ticket's PRs merged, the running build
/// contains their merge commits, and an Android change is installed.
fn check_dark(
    ctx: &Ctx<'_>,
    conn: &Connection,
    x: &Experiment,
    app_versions: &[String],
) -> Out<bool> {
    let Some(build) = ctx.build_sha else {
        return Ok(false);
    };
    let tickets = x.tickets();
    if tickets.is_empty() {
        return Ok(false);
    }
    for ticket in &tickets {
        let mut merged = 0;
        for pr in linked_prs(conn, *ticket)? {
            let pull = ctx.git.pull(pr).map_err(|e| anyhow::anyhow!(e))?;
            match (pull.state.as_str(), &pull.merge_commit) {
                ("open", _) => return Ok(false),
                ("merged", Some(merge)) => {
                    if !ctx
                        .git
                        .is_ancestor(merge, build)
                        .map_err(|e| anyhow::anyhow!(e))?
                    {
                        return Ok(false);
                    }
                    merged += 1;
                }
                _ => {}
            }
        }
        if merged == 0 {
            return Ok(false);
        }
    }
    let needed: Vec<String> = {
        let placeholders = tickets.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT version_name FROM autopm_app_deploys WHERE repo = ? AND number IN ({placeholders})"
        ))?;
        let mut values: Vec<rusqlite::types::Value> = vec![super::REPO.to_owned().into()];
        values.extend(tickets.iter().map(|t| rusqlite::types::Value::Integer(*t)));
        let rows = stmt.query_map(rusqlite::params_from_iter(values), |row| {
            row.get::<_, Option<String>>(0)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|v| v.unwrap_or_default())
            .collect()
    };
    for need in needed {
        if !app_versions
            .iter()
            .any(|have| version_at_least(have, &need) == Some(true))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `rolling_back → reverted` once rollback is confirmed: the revert PR's
/// merge commit is in the running build, and a rows experiment recorded
/// its restore.
fn check_rolled_back(ctx: &Ctx<'_>, conn: &Connection, x: &Experiment) -> Out<Option<Vec<Effect>>> {
    let mut revert_commit = x.revert_commit.clone();
    if revert_commit.is_none() {
        let tickets: Vec<i64> = {
            let mut stmt = conn.prepare(
                "SELECT number FROM autopm_fixes WHERE experiment_id = ?1 AND kind = 'revert'",
            )?;
            let rows = stmt.query_map([&x.id], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        'tickets: for ticket in tickets {
            for pr in linked_prs(conn, ticket)? {
                let pull = ctx.git.pull(pr).map_err(|e| anyhow::anyhow!(e))?;
                if let ("merged", Some(merge)) = (pull.state.as_str(), pull.merge_commit) {
                    conn.execute(
                        "UPDATE autopm_experiments SET revert_commit = ?2 WHERE id = ?1",
                        params![x.id, merge],
                    )?;
                    revert_commit = Some(merge);
                    break 'tickets;
                }
            }
        }
    }
    let (Some(commit), Some(build)) = (revert_commit, ctx.build_sha) else {
        return Ok(None);
    };
    if !ctx
        .git
        .is_ancestor(&commit, build)
        .map_err(|e| anyhow::anyhow!(e))?
    {
        return Ok(None);
    }
    if x.change_class.as_deref() == Some("rows") && x.restore_ok_at_ms.is_none() {
        return Ok(None);
    }
    set_state(conn, &x.id, "reverted", None, ctx.now)?;
    Ok(Some(vec![Effect::WakeAutopm {
        text: format!(
            "[sm autopm] {} reverted: rollback confirmed in the running build",
            x.id
        ),
    }]))
}

/// One measurement attempt (F.2). Errors retry hourly; the third reverts.
fn measure(ctx: &Ctx<'_>, conn: &Connection, x: &Experiment) -> Out<Vec<Effect>> {
    let attempts: Vec<(i64, i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT attempt, at_ms, status FROM autopm_measurements
             WHERE experiment_id = ?1 AND superseded_at_ms IS NULL ORDER BY attempt",
        )?;
        let rows = stmt.query_map([&x.id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if let Some((_, at, status)) = attempts.last() {
        if status == "error" && ctx.now < at + HOUR_MS {
            return Ok(Vec::new());
        }
    }
    let next: i64 = conn.query_row(
        "SELECT COALESCE(MAX(attempt), 0) + 1 FROM autopm_measurements WHERE experiment_id = ?1",
        [&x.id],
        |row| row.get(0),
    )?;
    if x.state == "live" {
        set_state(conn, &x.id, "measuring", None, ctx.now)?;
    }
    let result = (|| -> Out<metrics::Outcome> {
        let (metric, guards) = specs(x)?;
        let commit = x
            .plan_commit
            .clone()
            .ok_or_else(|| refuse("plan_merged", "no plan commit"))?;
        let baseline: Baseline =
            serde_json::from_value(x.baseline_json.clone().unwrap_or_default())
                .map_err(|e| refuse("baseline", format!("baseline_json is unreadable: {e}")))?;
        let live_at = x.live_at_ms.unwrap_or(ctx.now);
        let [h1, h2] = metrics::halves(live_at, x.window());
        let run = |path: &str| -> Out<Vec<(i64, f64)>> {
            let sql = query_text(ctx, &commit, path)?;
            metrics::run_query(&ctx.store.paths, &sql, h1.0, h2.1)
                .map_err(|e| refuse("metric_query", format!("{path}: {e}")))
        };
        let halves_of = |rows: &[(i64, f64)], stat| {
            [
                metrics::sample(stat, rows, h1.0, h1.1),
                metrics::sample(stat, rows, h2.0, h2.1),
            ]
        };
        let rows = run(&metric.query)?;
        let metric_halves = halves_of(&rows, metric.stat);
        let mut guard_samples = Vec::new();
        for (index, guard) in guards.iter().enumerate() {
            let rows = run(&guard.query)?;
            guard_samples.push((
                guard,
                baseline
                    .guards
                    .get(index)
                    .copied()
                    .unwrap_or(Sample { stat: None, n: 0 }),
                halves_of(&rows, guard.stat),
            ));
        }
        let guard_results: Vec<metrics::GuardResult<'_>> = guard_samples
            .iter()
            .map(|(spec, baseline, halves)| metrics::GuardResult {
                spec,
                baseline: *baseline,
                halves: *halves,
            })
            .collect();
        Ok(metrics::decide(
            &metric,
            baseline.metric,
            metric_halves,
            &guard_results,
            x.votes_down,
            x.extended,
        ))
    })();
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(Fail::Refused(refusal)) => {
            conn.execute(
                "INSERT INTO autopm_measurements(experiment_id, attempt, at_ms, status, error)
                 VALUES (?1, ?2, ?3, 'error', ?4)",
                params![x.id, next, ctx.now, refusal.message()],
            )?;
            let errors = attempts.iter().filter(|(_, _, s)| s == "error").count() + 1;
            if errors >= 3 {
                let x = require(conn, &x.id)?;
                return reverse(conn, &x, "metric failed", "autopm", false, ctx.now);
            }
            return Ok(vec![Effect::WakeAutopm {
                text: format!(
                    "[sm autopm] measuring {} failed ({errors} of 3): {}",
                    x.id,
                    refusal.message()
                ),
            }]);
        }
        Err(other) => return Err(other),
    };
    conn.execute(
        "INSERT INTO autopm_measurements(experiment_id, attempt, at_ms, status, result_json, decision)
         VALUES (?1, ?2, ?3, 'ok', ?4, ?5)",
        params![x.id, next, ctx.now, outcome.result.to_string(), outcome.decision.as_str()],
    )?;
    let x = require(conn, &x.id)?;
    let text = format!(
        "[sm autopm] measured {}: {} ({})",
        x.id,
        outcome.decision.as_str(),
        outcome.note
    );
    match outcome.decision {
        metrics::Decision::Keep => {
            conn.execute(
                "UPDATE autopm_experiments SET state = 'kept', kept_at_ms = ?2,
                   decision_note = ?3, updated_at_ms = ?2 WHERE id = ?1",
                params![x.id, ctx.now, outcome.note],
            )?;
        }
        metrics::Decision::Extend => {
            conn.execute(
                "UPDATE autopm_experiments SET state = 'live', extended = 1,
                   measure_at_ms = measure_at_ms + ?2, updated_at_ms = ?3 WHERE id = ?1",
                params![x.id, 14 * DAY_MS, ctx.now],
            )?;
        }
        metrics::Decision::Revert => {
            reverse(
                conn,
                &x,
                &format!("measured: {}", outcome.note),
                "autopm",
                false,
                ctx.now,
            )?;
        }
    }
    Ok(vec![Effect::WakeAutopm { text }])
}

/// sm's own moves, run on a timer: `building → dark`, measurement at
/// `measure_at_ms` and its hourly retries, and rollback confirmation.
/// `app_versions` are the owner devices' reported app versions.
pub fn tick(ctx: &Ctx<'_>, app_versions: &[String]) -> Out<Vec<Effect>> {
    let conn = ctx.store.open()?;
    let mut effects = Vec::new();
    for x in list(&conn)? {
        let step = (|| -> Out<Vec<Effect>> {
            match x.state.as_str() {
                "building" if x.path != "proposal" => {
                    if check_dark(ctx, &conn, &x, app_versions)? {
                        set_state(&conn, &x.id, "dark", None, ctx.now)?;
                        return Ok(vec![Effect::WakeAutopm {
                            text: format!(
                                "[sm autopm] {} is dark: built, deployed, switched off",
                                x.id
                            ),
                        }]);
                    }
                    Ok(Vec::new())
                }
                "live" | "measuring" if x.measure_at_ms.is_some_and(|at| ctx.now >= at) => {
                    measure(ctx, &conn, &x)
                }
                "rolling_back" => Ok(check_rolled_back(ctx, &conn, &x)?.unwrap_or_default()),
                _ => Ok(Vec::new()),
            }
        })();
        match step {
            Ok(step) => effects.extend(step),
            Err(Fail::Refused(refusal)) => {
                eprintln!("autopm {}: {}", x.id, refusal.message());
            }
            Err(Fail::Internal(error)) => eprintln!("autopm {}: {error:#}", x.id),
        }
    }
    ctx.switches.reload(&conn)?;
    Ok(effects)
}

/// Whether a kept experiment is still in its 30 days with the switch.
pub fn in_kept_period(x: &Experiment, now: i64) -> bool {
    x.kept_at_ms
        .is_some_and(|kept| now < kept + KEPT_SWITCH_DAYS * DAY_MS)
}

/// States in which an experiment counts toward `max_live`.
pub fn counts_live(state: &str) -> bool {
    LIVE_STATES.contains(&state)
}
