//! The weekly budget (C). Each provider's allowance is a dedicated amount
//! plus surplus from day four; spend is the auto PM's seats, the agents on
//! its tickets, and the reviews of its PRs, in fitted points. Build tickets
//! reserve their estimate when they start, and two standing reservations
//! cover the auto PM seat (`pm`) and every autopm review (`reviews`).
//!
//! The budget is an allowance enforced at admission, not an exact ledger.
//! Every rule here stands down while `autopm.enabled` is false, and
//! `kind=revert` tickets are exempt from every one of them.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::experiments::{self, Ctx};
use super::{
    autopm_seats, goal_claims, ticket_open, tickets_under_goal, AutopmStore, Effect, Fail, Out,
    Refusal, Settings, DAY_MS, HOUR_MS, REPO,
};
use crate::quota_rates::{self, Tokens};

/// Surplus is released from this far into the window (C.2).
const SURPLUS_FROM_MS: i64 = 96 * HOUR_MS;
/// A projection from an older sample gives no surplus.
const PROJECTION_MAX_AGE_MS: i64 = 6 * HOUR_MS;
const MIN_PROJECTION_ELAPSED_MS: i64 = HOUR_MS;
/// Samples whose window starts lie this close are one window: Codex reports
/// its start with a second or two of jitter.
const WINDOW_JOIN_MS: i64 = HOUR_MS;
const OVER_ESTIMATE: f64 = 1.5;
/// The standing reservations' ticket numbers under repo `autopm`.
pub const STANDING_REPO: &str = "autopm";
pub const PM: i64 = 0;
pub const REVIEWS: i64 = 1;

pub fn window_kind(provider: &str) -> &'static str {
    if provider == "codex" {
        "codex_10080"
    } else {
        "weekly_all"
    }
}

/// `codex-fork` and `codex` agents spend Codex quota.
pub fn provider_of_agent(provider: &str) -> Option<&'static str> {
    match provider {
        "claude" => Some("claude"),
        "codex" | "codex-fork" | "codex-app" => Some("codex"),
        _ => None,
    }
}

/// A provider's weekly window, from its active account's newest sample.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Window {
    pub provider: String,
    /// The canonical start text, the key reservations carry.
    pub window_start: String,
    pub start_ms: i64,
    pub resets_ms: i64,
    pub percent: f64,
    pub observed_ms: i64,
}

/// The newest weekly sample of the account the provider's agents use: the
/// account seen most recently, as the review meter chooses it.
pub fn latest_sample(usage_db: &Path, provider: &str) -> Result<Option<Window>> {
    let Some(conn) = super::open_read_only(usage_db)? else {
        return Ok(None);
    };
    let row = conn
        .query_row(
            "SELECT window_start, resets_at, percent, observed_at FROM burn_samples
             WHERE window_kind = ?1 AND window_scope IS NULL AND account_key = (
               SELECT account_key FROM accounts WHERE provider = ?2
               ORDER BY last_seen DESC, account_key LIMIT 1)
             ORDER BY observed_at DESC, id DESC LIMIT 1",
            params![window_kind(provider), provider],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional();
    let row = match row {
        Ok(row) => row,
        Err(rusqlite::Error::SqliteFailure(_, Some(e))) if e.contains("no such table") => None,
        Err(e) => return Err(e.into()),
    };
    Ok(row.and_then(|(start, resets, percent, observed)| {
        Some(Window {
            provider: provider.to_owned(),
            start_ms: super::parse_ms(&start)?,
            resets_ms: super::parse_ms(&resets)?,
            observed_ms: super::parse_ms(&observed)?,
            window_start: start,
            percent,
        })
    }))
}

/// The window sm has opened for the provider (its rollover done), with the
/// newest sample's reading.
pub fn current(conn: &Connection, usage_db: &Path, provider: &str) -> Result<Option<Window>> {
    let Some(sample) = latest_sample(usage_db, provider)? else {
        return Ok(None);
    };
    let stored: Option<(String, i64, i64)> = conn
        .query_row(
            "SELECT window_start, start_ms, resets_ms FROM autopm_windows WHERE provider = ?1",
            [provider],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    Ok(stored.and_then(|(start, start_ms, resets_ms)| {
        ((sample.start_ms - start_ms).abs() < WINDOW_JOIN_MS).then_some(Window {
            window_start: start,
            start_ms,
            resets_ms,
            ..sample
        })
    }))
}

/// The instant `skip_window_starting` names: an RFC 3339 time, or a bare
/// date taken as that day's local midnight.
pub fn skip_instant(text: &str) -> Option<i64> {
    if let Some(ms) = super::parse_ms(text) {
        return Some(ms);
    }
    let format = time::macros::format_description!("[year]-[month]-[day]");
    let date = time::Date::parse(text.trim(), &format).ok()?;
    let midnight = date.midnight().assume_utc();
    let offset = time::UtcOffset::local_offset_at(midnight).unwrap_or(time::UtcOffset::UTC);
    let local = date.midnight().assume_offset(offset);
    Some((local.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// A window is skipped when the skip instant falls inside it (C.2).
pub fn skipped(settings: &Settings, window: &Window) -> bool {
    settings
        .skip_window_starting
        .as_deref()
        .and_then(skip_instant)
        .is_some_and(|at| (window.start_ms..window.resets_ms).contains(&at))
}

/// Points the linear projection says will be left at reset; `None` when
/// the sample is over six hours old or the window under an hour in.
pub fn projected_headroom(window: &Window, now: i64) -> Option<f64> {
    if now - window.observed_ms > PROJECTION_MAX_AGE_MS {
        return None;
    }
    let elapsed = window.observed_ms - window.start_ms;
    let horizon = window.resets_ms - window.observed_ms;
    if elapsed < MIN_PROJECTION_ELAPSED_MS || horizon <= 0 {
        return None;
    }
    let projected = window.percent + window.percent.max(0.0) / elapsed as f64 * horizon as f64;
    Some((100.0 - projected).max(0.0))
}

/// C.2: zero when skipped; the dedicated points before 96 h into the
/// window; then the dedicated points plus surplus beyond the margin, up to
/// the cap. No projection, no surplus.
pub fn allowance(settings: &Settings, window: &Window, now: i64) -> f64 {
    if skipped(settings, window) {
        return 0.0;
    }
    let dedicated = settings
        .dedicated
        .get(&window.provider)
        .copied()
        .unwrap_or(0.0);
    if now - window.start_ms < SURPLUS_FROM_MS {
        return dedicated;
    }
    let cap = settings
        .surplus_cap
        .get(&window.provider)
        .copied()
        .unwrap_or(0.0);
    let surplus = projected_headroom(window, now)
        .map(|headroom| cap.min((headroom - settings.surplus_margin).max(0.0)))
        .unwrap_or(0.0);
    dedicated + surplus
}

/// Spend in one provider's window, split by what it is charged to.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Spend {
    pub total: f64,
    pub pm: f64,
    pub reviews: f64,
    pub by_ticket: BTreeMap<i64, f64>,
}

/// Fitted points per seat in `[from, until)` of the provider's weekly
/// ledger rows.
pub fn seat_points(
    usage_db: &Path,
    provider: &str,
    seats: &BTreeSet<String>,
    from: i64,
    until: i64,
) -> Result<BTreeMap<String, f64>> {
    let mut points = BTreeMap::new();
    if seats.is_empty() {
        return Ok(points);
    }
    let Some(conn) = super::open_read_only(usage_db)? else {
        return Ok(points);
    };
    // One lookup per seat: `seat_id` leads the table's primary key.
    let mut stmt = match conn.prepare(
        "SELECT bucket_ts, model, input_tokens, output_tokens, cache_write_5m,
                cache_write_1h, cache_read_tokens
         FROM seat_tokens WHERE seat_id = ?1 AND window_kind = ?2 AND account_key LIKE ?3",
    ) {
        Ok(stmt) => stmt,
        Err(rusqlite::Error::SqliteFailure(_, Some(e))) if e.contains("no such table") => {
            return Ok(points)
        }
        Err(e) => return Err(e.into()),
    };
    for seat in seats {
        let rows = stmt.query_map(
            params![seat, window_kind(provider), format!("{provider}:%")],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    Tokens {
                        input: row.get(2)?,
                        output: row.get(3)?,
                        cache_write_5m: row.get(4)?,
                        cache_write_1h: row.get(5)?,
                        cache_read: row.get(6)?,
                    },
                ))
            },
        )?;
        for row in rows {
            let (bucket, model, tokens) = row?;
            if !super::parse_ms(&bucket).is_some_and(|at| (from..until).contains(&at)) {
                continue;
            }
            *points.entry(seat.clone()).or_insert(0.0) +=
                quota_rates::fitted_percent(provider, &model, &tokens);
        }
    }
    Ok(points)
}

/// C.3: every seat that held the role is charged to `pm`; every other seat
/// that claimed a ticket under #2170 to the first such ticket it claimed;
/// every review run on an autopm PR to `reviews`.
pub fn spend(
    conn: &Connection,
    usage_db: &Path,
    provider: &str,
    from: i64,
    until: i64,
) -> Result<Spend> {
    let pm_seats = autopm_seats(conn)?;
    let under = tickets_under_goal(conn)?;
    let mut seat_ticket = BTreeMap::new();
    for claim in goal_claims(conn, &under)? {
        if !pm_seats.contains(&claim.session_id) {
            seat_ticket.entry(claim.session_id).or_insert(claim.number);
        }
    }
    let seats: BTreeSet<String> = pm_seats
        .iter()
        .cloned()
        .chain(seat_ticket.keys().cloned())
        .collect();
    let mut spend = Spend::default();
    for (seat, points) in seat_points(usage_db, provider, &seats, from, until)? {
        if pm_seats.contains(&seat) {
            spend.pm += points;
        } else if let Some(ticket) = seat_ticket.get(&seat) {
            *spend.by_ticket.entry(*ticket).or_insert(0.0) += points;
        }
    }
    spend.reviews = conn.query_row(
        "SELECT COALESCE(SUM(points), 0) FROM autopm_review_charges
         WHERE provider = ?1 AND at_ms >= ?2 AND at_ms < ?3",
        params![provider, from, until],
        |row| row.get(0),
    )?;
    spend.total = spend.pm + spend.reviews + spend.by_ticket.values().sum::<f64>();
    Ok(spend)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reservation {
    pub repo: String,
    pub number: i64,
    pub provider: String,
    pub points: f64,
    pub window_start: String,
    pub created_at_ms: i64,
    pub released_at_ms: Option<i64>,
    pub original_points: f64,
    pub origin_ms: i64,
    pub suspended_at_ms: Option<i64>,
}

impl Reservation {
    pub fn standing(&self) -> bool {
        self.repo == STANDING_REPO
    }

    /// What this reservation's charges have spent this window.
    pub fn spent(&self, spend: &Spend) -> f64 {
        match (self.standing(), self.number) {
            (true, PM) => spend.pm,
            (true, _) => spend.reviews,
            (false, number) => spend.by_ticket.get(&number).copied().unwrap_or(0.0),
        }
    }
}

fn reservations(
    conn: &Connection,
    filter: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Reservation>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT repo, number, provider, points, window_start, created_at_ms, released_at_ms,
                COALESCE(original_points, points), COALESCE(origin_ms, created_at_ms), suspended_at_ms
         FROM autopm_reservations WHERE {filter} ORDER BY created_at_ms, repo, number"
    ))?;
    let rows = stmt.query_map(args, |row| {
        Ok(Reservation {
            repo: row.get(0)?,
            number: row.get(1)?,
            provider: row.get(2)?,
            points: row.get(3)?,
            window_start: row.get(4)?,
            created_at_ms: row.get(5)?,
            released_at_ms: row.get(6)?,
            original_points: row.get(7)?,
            origin_ms: row.get(8)?,
            suspended_at_ms: row.get(9)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Unreleased, unsuspended reservations of a provider's window.
pub fn holds(conn: &Connection, provider: &str, window_start: &str) -> Result<Vec<Reservation>> {
    reservations(
        conn,
        "provider = ?1 AND window_start = ?2 AND released_at_ms IS NULL AND suspended_at_ms IS NULL",
        &[&provider, &window_start],
    )
}

/// `open_hold` (C.4): what the open reservations may still spend.
pub fn open_hold(holds: &[Reservation], spend: &Spend) -> f64 {
    holds
        .iter()
        .map(|r| (r.points - r.spent(spend)).max(0.0))
        .sum()
}

/// One provider's budget now.
#[derive(Debug, Clone, PartialEq)]
pub struct Balance {
    pub window: Window,
    pub allowance: f64,
    pub spend: Spend,
    pub holds: Vec<Reservation>,
    pub reserved: f64,
}

impl Balance {
    /// What new tickets may still reserve.
    pub fn remaining(&self) -> f64 {
        self.allowance - self.spend.total - self.reserved
    }

    pub fn standing(&self, number: i64) -> Option<&Reservation> {
        self.holds
            .iter()
            .find(|r| r.standing() && r.number == number)
    }

    /// `pm.left` or `reviews.left`: the standing reservation less its spend.
    pub fn left(&self, number: i64) -> f64 {
        self.standing(number)
            .map_or(0.0, |r| r.points - r.spent(&self.spend))
    }
}

pub fn balance(
    conn: &Connection,
    usage_db: &Path,
    settings: &Settings,
    provider: &str,
    now: i64,
) -> Result<Option<Balance>> {
    let Some(window) = current(conn, usage_db, provider)? else {
        return Ok(None);
    };
    let spend = spend(conn, usage_db, provider, window.start_ms, window.resets_ms)?;
    let holds = holds(conn, provider, &window.window_start)?;
    Ok(Some(Balance {
        allowance: allowance(settings, &window, now),
        reserved: open_hold(&holds, &spend),
        window,
        spend,
        holds,
    }))
}

/// `GET /autopm/budget` (C.3).
pub fn status(store: &AutopmStore, settings: &Settings, now: i64) -> Result<Value> {
    let conn = store.open()?;
    let mut providers = serde_json::Map::new();
    let mut top: Option<(String, String)> = None;
    for provider in super::PROVIDERS {
        let Some(b) = balance(&conn, &store.paths.usage_db, settings, provider, now)? else {
            providers.insert(provider.into(), Value::Null);
            continue;
        };
        let standing = |number: i64, spent: f64| {
            let held = b.standing(number).map_or(0.0, |r| r.points);
            json!({"held": round(held), "spent": round(spent), "left": round(held - spent)})
        };
        let resets = super::format_ms(b.window.resets_ms);
        top.get_or_insert((b.window.window_start.clone(), resets.clone()));
        providers.insert(
            provider.into(),
            json!({
                "window_start": b.window.window_start,
                "resets_at": resets,
                "skipped": skipped(settings, &b.window),
                "allowance": round(b.allowance),
                "spent": round(b.spend.total),
                "reserved": round(b.reserved),
                "remaining": round(b.remaining()),
                "pm": standing(PM, b.spend.pm),
                "reviews": standing(REVIEWS, b.spend.reviews),
                "tickets": b.holds.iter().filter(|r| !r.standing()).map(|r| json!({
                    "number": r.number, "points": round(r.points),
                    "spent": round(r.spent(&b.spend)),
                })).collect::<Vec<_>>(),
            }),
        );
    }
    let (window_start, resets_at) = top.unzip();
    Ok(json!({
        "enabled": settings.enabled,
        "window_start": window_start,
        "resets_at": resets_at,
        "providers": providers,
    }))
}

fn round(points: f64) -> f64 {
    (points * 100.0).round() / 100.0
}

fn refuse(gate: &str, detail: impl Into<String>) -> Fail {
    Refusal::gate(gate, detail).into()
}

/// C.4 admission: in one `BEGIN IMMEDIATE` transaction, so concurrent
/// starts serialize, a ticket reserves `points_est` only if that is no more
/// than the provider's unreserved balance.
pub fn admit(
    store: &AutopmStore,
    settings: &Settings,
    provider: &str,
    number: i64,
    points_est: f64,
    now: i64,
) -> Out<Reservation> {
    if !(points_est.is_finite() && points_est > 0.0) {
        return Err(Refusal::bad("points_est", "points_est must be above 0").into());
    }
    if !super::PROVIDERS.contains(&provider) {
        return Err(Refusal::bad("provider", "provider must be claude or codex").into());
    }
    let mut conn = store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some(b) = balance(&tx, &store.paths.usage_db, settings, provider, now)? else {
        return Err(refuse(
            "budget",
            format!("no current {provider} window yet; retry in a minute"),
        ));
    };
    if b.holds.iter().any(|r| !r.standing() && r.number == number) {
        return Err(refuse(
            "already_reserved",
            format!("#{number} already holds a reservation"),
        ));
    }
    let unreserved = b.remaining();
    if points_est > unreserved {
        return Err(refuse(
            "budget",
            format!(
                "#{number} needs {points_est} {provider} points; {} are unreserved",
                round(unreserved.max(0.0))
            ),
        ));
    }
    tx.execute(
        "INSERT OR REPLACE INTO autopm_reservations (repo, number, provider, points, window_start,
           created_at_ms, original_points, origin_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?4, ?6)",
        params![
            REPO,
            number,
            provider,
            points_est,
            b.window.window_start,
            now
        ],
    )?;
    tx.commit()?;
    Ok(reservations(
        &conn,
        "repo = ?1 AND number = ?2 AND provider = ?3 AND window_start = ?4",
        &[&REPO, &number, &provider, &b.window.window_start],
    )?
    .remove(0))
}

/// `PUT /autopm/auto-start` for an experiment's build ticket (B.3, C.4):
/// the ticket is the experiment's, its plan merged, and the reservation is
/// admitted. sm#2175's route calls this.
pub fn admit_experiment_start(
    ctx: &Ctx<'_>,
    id: &str,
    number: i64,
    provider: &str,
    points_est: f64,
) -> Out<Reservation> {
    starts_allowed(ctx.settings, false)?;
    let x = experiments::get(&ctx.store.open()?, id)?
        .ok_or_else(|| Refusal::not_found(format!("no experiment {id}")))?;
    if !x.tickets().contains(&number) {
        return Err(refuse(
            "ticket",
            format!("#{number} is not in {id}'s tickets_json"),
        ));
    }
    experiments::plan_merged(ctx, &x)?;
    admit(
        ctx.store,
        ctx.settings,
        provider,
        number,
        points_est,
        ctx.now,
    )
}

/// `PUT /autopm/auto-start` for a fix, revert or cleanup ticket (B.3): it is
/// recorded in `autopm_fixes` and still open. A `kind=revert` ticket is
/// exempt from admission, pause and the kill switch, and reserves nothing.
pub fn admit_fix_start(
    ctx: &Ctx<'_>,
    number: i64,
    provider: &str,
    points_est: f64,
) -> Out<Option<Reservation>> {
    let conn = ctx.store.open()?;
    let fix = super::fix(&conn, number)?
        .filter(|fix| fix.cancelled_at_ms.is_none())
        .ok_or_else(|| {
            refuse(
                "not_fix",
                format!("#{number} is not an open autopm fix ticket"),
            )
        })?;
    if !ticket_open(&conn, number)? {
        return Err(refuse("not_fix", format!("#{number} is closed")));
    }
    if fix.kind == "revert" {
        return Ok(None);
    }
    starts_allowed(ctx.settings, fix.kind == "cleanup")?;
    admit(
        ctx.store,
        ctx.settings,
        provider,
        number,
        points_est,
        ctx.now,
    )
    .map(Some)
}

/// The kill switch stops every autopm start (B.5); a pause (F.4) leaves
/// only cleanup tickets. Revert tickets never ask.
pub fn starts_allowed(settings: &Settings, cleanup: bool) -> Out<()> {
    if !settings.enabled {
        return Err(refuse("autopm_disabled", "Auto PM is off"));
    }
    if settings.paused_at.is_some() && !cleanup {
        return Err(refuse("paused", "the auto PM is paused"));
    }
    Ok(())
}

fn safe_stop(text: &str, holders: &[(i64, String)]) -> Vec<Effect> {
    let mut effects = vec![Effect::WakeAutopm { text: text.into() }];
    effects.extend(holders.iter().map(|(_, session_id)| Effect::Message {
        session_id: session_id.clone(),
        text: text.into(),
    }));
    effects
}

/// The standing `pm` then `reviews` reservations of a window, each capped
/// by what is still unreserved. Existing ones are kept.
fn ensure_standing(
    conn: &Connection,
    usage_db: &Path,
    settings: &Settings,
    provider: &str,
    now: i64,
) -> Result<Vec<Effect>> {
    let mut effects = Vec::new();
    for (number, wanted) in [
        (PM, settings.pm_points.get(provider).copied().unwrap_or(0.0)),
        (
            REVIEWS,
            settings.review_points.get(provider).copied().unwrap_or(0.0),
        ),
    ] {
        let Some(b) = balance(conn, usage_db, settings, provider, now)? else {
            return Ok(effects);
        };
        if b.standing(number).is_some() {
            continue;
        }
        let points = wanted.min(b.remaining().max(0.0));
        conn.execute(
            "INSERT OR REPLACE INTO autopm_reservations (repo, number, provider, points, window_start,
               created_at_ms, original_points, origin_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?4, ?6)",
            params![STANDING_REPO, number, provider, points, b.window.window_start, now],
        )?;
        if number == PM && points <= 0.0 && provider == "claude" {
            effects.push(Effect::WakeAutopm {
                text: format!("[sm autopm] no {provider} budget for the auto PM this window: reach a safe stop"),
            });
        }
    }
    Ok(effects)
}

/// A provider's new window (C.4): release the old window's standing
/// reservations; roll every open or suspended ticket reservation into it
/// (unless it is skipped, when they wait); then create the standing ones.
fn open_window(
    conn: &Connection,
    usage_db: &Path,
    settings: &Settings,
    sample: &Window,
    now: i64,
) -> Result<Vec<Effect>> {
    let provider = sample.provider.as_str();
    let previous: Option<(i64, bool)> = conn
        .query_row(
            "SELECT start_ms, skipped FROM autopm_windows WHERE provider = ?1",
            [provider],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let skip = skipped(settings, sample);
    conn.execute(
        "INSERT OR REPLACE INTO autopm_windows(provider, window_start, start_ms, resets_ms, skipped)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![provider, sample.window_start, sample.start_ms, sample.resets_ms, skip],
    )?;
    conn.execute(
        "UPDATE autopm_reservations SET released_at_ms = ?3
         WHERE repo = ?1 AND provider = ?2 AND released_at_ms IS NULL",
        params![STANDING_REPO, provider, now],
    )?;
    let open = reservations(
        conn,
        "repo != ?1 AND provider = ?2 AND released_at_ms IS NULL AND window_start != ?3",
        &[&STANDING_REPO, &provider, &sample.window_start],
    )?;
    let mut effects = Vec::new();
    if !settings.enabled {
        return Ok(effects);
    }
    let holders = experiments::stoppable_holders(conn).map_err(fail_to_anyhow)?;
    if skip {
        conn.execute(
            "UPDATE autopm_reservations SET suspended_at_ms = COALESCE(suspended_at_ms, ?3)
             WHERE repo != ?1 AND provider = ?2 AND released_at_ms IS NULL",
            params![STANDING_REPO, provider, now],
        )?;
        effects.extend(safe_stop(
            &format!("[sm autopm] this {provider} window is skipped: reach a safe stop"),
            &holders,
        ));
        return Ok(effects);
    }
    for r in open {
        let cumulative = spend(conn, usage_db, provider, r.origin_ms, now)?
            .by_ticket
            .get(&r.number)
            .copied()
            .unwrap_or(0.0);
        let points = (OVER_ESTIMATE * r.original_points - cumulative).max(1.0);
        conn.execute(
            "INSERT OR REPLACE INTO autopm_reservations (repo, number, provider, points, window_start,
               created_at_ms, original_points, origin_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![r.repo, r.number, provider, points, sample.window_start, now, r.original_points, r.origin_ms],
        )?;
        conn.execute(
            "UPDATE autopm_reservations SET released_at_ms = ?5
             WHERE repo = ?1 AND number = ?2 AND provider = ?3 AND window_start = ?4",
            params![r.repo, r.number, provider, r.window_start, now],
        )?;
        for (_, session_id) in holders.iter().filter(|(n, _)| *n == r.number) {
            effects.push(Effect::Message {
                session_id: session_id.clone(),
                text: format!("[sm autopm] budget restored: resume #{}", r.number),
            });
        }
    }
    let starved = ensure_standing(conn, usage_db, settings, provider, now)?;
    let pm_was_zero = previous.is_some_and(|(_, skipped)| skipped);
    if starved.is_empty() && pm_was_zero && provider == "claude" {
        effects.push(Effect::WakeAutopm {
            text: "[sm autopm] budget restored: resume".into(),
        });
    }
    effects.extend(starved);
    Ok(effects)
}

fn fail_to_anyhow(fail: Fail) -> anyhow::Error {
    match fail {
        Fail::Internal(error) => error,
        Fail::Refused(refusal) => anyhow::anyhow!(refusal.message()),
    }
}

/// Opens each provider's window when its sample shows a new one. Runs
/// before anything else each minute.
pub fn sync_windows(store: &AutopmStore, settings: &Settings, now: i64) -> Result<Vec<Effect>> {
    let mut conn = store.open()?;
    let mut effects = Vec::new();
    for provider in super::PROVIDERS {
        let Some(sample) = latest_sample(&store.paths.usage_db, provider)? else {
            continue;
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored: Option<i64> = tx
            .query_row(
                "SELECT start_ms FROM autopm_windows WHERE provider = ?1",
                [provider],
                |row| row.get(0),
            )
            .optional()?;
        let new = stored.is_none_or(|start| sample.start_ms - start >= WINDOW_JOIN_MS);
        if new && sample.resets_ms > now {
            effects.extend(open_window(
                &tx,
                &store.paths.usage_db,
                settings,
                &sample,
                now,
            )?);
        }
        tx.commit()?;
    }
    Ok(effects)
}

/// Enabling the auto PM creates the current windows' standing reservations.
pub fn on_enable(store: &AutopmStore, settings: &Settings, now: i64) -> Result<Vec<Effect>> {
    let mut effects = sync_windows(store, settings, now)?;
    let conn = store.open()?;
    for provider in super::PROVIDERS {
        if let Some(window) = current(&conn, &store.paths.usage_db, provider)? {
            if !skipped(settings, &window) {
                effects.extend(ensure_standing(
                    &conn,
                    &store.paths.usage_db,
                    settings,
                    provider,
                    now,
                )?);
            }
        }
    }
    Ok(effects)
}

/// Applies a change of `skip_window_starting` to the current windows.
/// Skipping one cancels its standing reservations, suspends ticket
/// reservations and stops every autopm seat; clearing it resumes them.
pub fn apply_skip(store: &AutopmStore, settings: &Settings, now: i64) -> Result<Vec<Effect>> {
    let mut conn = store.open()?;
    let mut effects = Vec::new();
    for provider in super::PROVIDERS {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(window) = current(&tx, &store.paths.usage_db, provider)? else {
            continue;
        };
        let was: bool = tx.query_row(
            "SELECT skipped FROM autopm_windows WHERE provider = ?1",
            [provider],
            |row| row.get(0),
        )?;
        let now_skipped = skipped(settings, &window);
        if was == now_skipped {
            continue;
        }
        tx.execute(
            "UPDATE autopm_windows SET skipped = ?2 WHERE provider = ?1",
            params![provider, now_skipped],
        )?;
        if settings.enabled {
            let holders = experiments::stoppable_holders(&tx).map_err(fail_to_anyhow)?;
            if now_skipped {
                tx.execute(
                    "UPDATE autopm_reservations SET released_at_ms = ?3
                     WHERE repo = ?1 AND provider = ?2 AND released_at_ms IS NULL",
                    params![STANDING_REPO, provider, now],
                )?;
                tx.execute(
                    "UPDATE autopm_reservations SET suspended_at_ms = ?3
                     WHERE repo != ?1 AND provider = ?2 AND released_at_ms IS NULL
                       AND suspended_at_ms IS NULL",
                    params![STANDING_REPO, provider, now],
                )?;
                effects.extend(safe_stop(
                    &format!(
                        "[sm autopm] Rajesh skipped this {provider} window: reach a safe stop"
                    ),
                    &holders,
                ));
            } else {
                let resumed = reservations(
                    &tx,
                    "repo != ?1 AND provider = ?2 AND window_start = ?3 AND released_at_ms IS NULL
                       AND suspended_at_ms IS NOT NULL",
                    &[&STANDING_REPO, &provider, &window.window_start],
                )?;
                tx.execute(
                    "UPDATE autopm_reservations SET suspended_at_ms = NULL
                     WHERE repo != ?1 AND provider = ?2 AND window_start = ?3 AND released_at_ms IS NULL",
                    params![STANDING_REPO, provider, window.window_start],
                )?;
                effects.extend(ensure_standing(
                    &tx,
                    &store.paths.usage_db,
                    settings,
                    provider,
                    now,
                )?);
                for r in resumed {
                    for (_, session_id) in holders.iter().filter(|(n, _)| *n == r.number) {
                        effects.push(Effect::Message {
                            session_id: session_id.clone(),
                            text: format!("[sm autopm] budget restored: resume #{}", r.number),
                        });
                    }
                }
            }
        }
        tx.commit()?;
    }
    Ok(effects)
}

/// The minute's budget upkeep (C.4): release reservations whose ticket
/// closed or lost its last claim, send over-estimate and zero-balance safe
/// stops once each, and record each experiment's points spent.
/// `provider_of` names a session's provider (`claude` or `codex`).
pub fn tick(
    store: &AutopmStore,
    settings: &Settings,
    now: i64,
    provider_of: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<Effect>> {
    let mut effects = sync_windows(store, settings, now)?;
    let conn = store.open()?;
    let claims: Vec<(super::Claim, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT number, session_id, ended_at IS NULL, claimed_at FROM work_claims
             WHERE repo = ?1 AND kind = 'ticket'",
        );
        match &mut stmt {
            Ok(stmt) => stmt
                .query_map([REPO], |row| {
                    Ok((
                        super::Claim {
                            number: row.get(0)?,
                            session_id: row.get(1)?,
                            live: row.get(2)?,
                        },
                        super::parse_ms(&row.get::<_, String>(3)?).unwrap_or(0),
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            Err(_) => Vec::new(),
        }
    };
    let open = reservations(
        &conn,
        "repo != ?1 AND released_at_ms IS NULL",
        &[&STANDING_REPO],
    )?;
    for r in &open {
        let on_ticket: Vec<&(super::Claim, i64)> = claims
            .iter()
            .filter(|(c, _)| c.number == r.number)
            .collect();
        let claimed_since = on_ticket.iter().any(|(_, at)| *at >= r.origin_ms);
        let live = on_ticket.iter().any(|(c, _)| c.live);
        if !ticket_open(&conn, r.number)? || (claimed_since && !live) {
            conn.execute(
                "UPDATE autopm_reservations SET released_at_ms = ?5
                 WHERE repo = ?1 AND number = ?2 AND provider = ?3 AND window_start = ?4",
                params![r.repo, r.number, r.provider, r.window_start, now],
            )?;
        }
    }
    if !settings.enabled {
        return Ok(effects);
    }
    let holders = experiments::stoppable_holders(&conn).map_err(fail_to_anyhow)?;
    for provider in super::PROVIDERS {
        let Some(b) = balance(&conn, &store.paths.usage_db, settings, provider, now)? else {
            continue;
        };
        for r in b.holds.iter().filter(|r| !r.standing()) {
            let cumulative = spend(&conn, &store.paths.usage_db, provider, r.origin_ms, now)?
                .by_ticket
                .get(&r.number)
                .copied()
                .unwrap_or(0.0);
            if cumulative > OVER_ESTIMATE * r.original_points
                && AutopmStore::first_notice(
                    &conn,
                    &format!("over:{}:{provider}:{}", r.number, r.origin_ms),
                    now,
                )?
            {
                for (_, session_id) in holders.iter().filter(|(n, _)| *n == r.number) {
                    effects.push(Effect::Message {
                        session_id: session_id.clone(),
                        text: "[sm autopm] ticket over estimate: reach a safe stop".into(),
                    });
                }
            }
        }
        if let Some(pm) = b.standing(PM) {
            if b.spend.pm > OVER_ESTIMATE * pm.points
                && AutopmStore::first_notice(
                    &conn,
                    &format!("over:pm:{provider}:{}", b.window.window_start),
                    now,
                )?
            {
                effects.push(Effect::WakeAutopm {
                    text: "[sm autopm] auto PM over its pm reservation: reach a safe stop".into(),
                });
            }
        }
        let rolled: f64 = b
            .holds
            .iter()
            .filter(|r| !r.standing() && r.origin_ms < b.window.start_ms)
            .map(|r| r.points)
            .sum();
        if b.allowance.max(rolled) - b.spend.total <= 0.0
            && AutopmStore::first_notice(
                &conn,
                &format!("zero:{provider}:{}", b.window.window_start),
                now,
            )?
        {
            let of_provider: Vec<(i64, String)> = holders
                .iter()
                .filter(|(_, session)| provider_of(session).as_deref() == Some(provider))
                .cloned()
                .collect();
            effects.extend(safe_stop(
                &format!("[sm autopm] {provider} budget spent: reach a safe stop"),
                &of_provider,
            ));
        }
    }
    record_points_spent(&conn, &store.paths.usage_db)?;
    Ok(effects)
}

/// `points_spent_json`: each experiment's tickets' spend, all time.
fn record_points_spent(conn: &Connection, usage_db: &Path) -> Result<()> {
    let experiments = experiments::list(conn)?;
    if experiments.iter().all(|x| x.tickets().is_empty()) {
        return Ok(());
    }
    let mut by_provider = BTreeMap::new();
    for provider in super::PROVIDERS {
        by_provider.insert(
            provider,
            spend(conn, usage_db, provider, 0, i64::MAX)?.by_ticket,
        );
    }
    for x in experiments.iter().filter(|x| !x.tickets().is_empty()) {
        let mut spent = serde_json::Map::new();
        for (provider, tickets) in &by_provider {
            let points: f64 = x.tickets().iter().filter_map(|t| tickets.get(t)).sum();
            spent.insert((*provider).into(), json!(round(points)));
        }
        conn.execute(
            "UPDATE autopm_experiments SET points_spent_json = ?2 WHERE id = ?1",
            params![x.id, Value::Object(spent).to_string()],
        )?;
    }
    Ok(())
}

/// What the review watcher must do with one autopm review step (C.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewGate {
    /// Dispatch now.
    Proceed,
    /// Move to the next reviewer in the chain, with this reason.
    Skip(String),
    /// Hold the step; tell the requester `notify` once, if set.
    Wait(Option<String>),
}

/// A review step to gate.
pub struct ReviewStep<'a> {
    pub request_id: &'a str,
    pub round: i64,
    pub step_index: i64,
    pub pr: i64,
    pub repo: &'a str,
    pub provider: &'a str,
    /// The providers of the steps after this one.
    pub later: &'a [String],
}

/// C.4's review rules: autopm reviews run one at a time per provider, only
/// while the provider has an allowance, and skip a provider whose `reviews`
/// reservation is spent. Revert PRs dispatch at once. Nothing applies while
/// the auto PM is off.
pub fn review_gate(
    store: &AutopmStore,
    settings: &Settings,
    step: &ReviewStep<'_>,
    now: i64,
) -> Result<ReviewGate> {
    if !settings.enabled {
        return Ok(ReviewGate::Proceed);
    }
    let mut conn = store.open()?;
    let class = super::classify_pr(&conn, step.repo, step.pr)?;
    if !class.autopm {
        return Ok(ReviewGate::Proceed);
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let held: Option<(String, i64, i64)> = tx
        .query_row(
            "SELECT provider, round, step_index FROM autopm_review_slots WHERE request_id = ?1",
            [step.request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if held == Some((step.provider.to_owned(), step.round, step.step_index)) {
        return Ok(ReviewGate::Proceed);
    }
    if !class.revert {
        let usage = &store.paths.usage_db;
        let Some(b) = balance(&tx, usage, settings, step.provider, now)? else {
            return Ok(ReviewGate::Wait(None));
        };
        if b.allowance <= 0.0 {
            return Ok(ReviewGate::Wait(None));
        }
        if b.left(REVIEWS) <= 0.0 {
            for later in step.later {
                if let Some(other) = balance(&tx, usage, settings, later, now)? {
                    if other.allowance > 0.0 && other.left(REVIEWS) > 0.0 {
                        return Ok(ReviewGate::Skip(format!(
                            "{} autopm review budget spent",
                            step.provider
                        )));
                    }
                }
            }
            let notice = format!(
                "review-budget:{}:{}",
                step.request_id, b.window.window_start
            );
            let notify = AutopmStore::first_notice(&tx, &notice, now)?.then(|| {
                format!(
                    "[sm autopm] review budget spent until {}",
                    super::format_ms(b.window.resets_ms)
                )
            });
            tx.commit()?;
            return Ok(ReviewGate::Wait(notify));
        }
        let busy: bool = tx
            .query_row(
                "SELECT 1 FROM autopm_review_slots WHERE provider = ?1 AND exempt = 0
                   AND request_id != ?2",
                params![step.provider, step.request_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if busy {
            return Ok(ReviewGate::Wait(None));
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO autopm_review_slots
           (request_id, provider, round, step_index, exempt, started_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            step.request_id,
            step.provider,
            step.round,
            step.step_index,
            class.revert,
            now
        ],
    )?;
    tx.commit()?;
    Ok(ReviewGate::Proceed)
}

/// A dispatched autopm review step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub request_id: String,
    pub provider: String,
    pub round: i64,
    pub step_index: i64,
    pub started_at_ms: i64,
}

pub fn slots(conn: &Connection) -> Result<Vec<Slot>> {
    let mut stmt = conn.prepare(
        "SELECT request_id, provider, round, step_index, started_at_ms FROM autopm_review_slots",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Slot {
            request_id: row.get(0)?,
            provider: row.get(1)?,
            round: row.get(2)?,
            step_index: row.get(3)?,
            started_at_ms: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Ends a slot whose step is over, charging its spend to the provider that
/// ran it (C.4: every review run, fallbacks included, draws on `reviews`).
pub fn finish_slot(store: &AutopmStore, slot: &Slot, points: f64, now: i64) -> Result<()> {
    let conn = store.open()?;
    conn.execute(
        "INSERT OR IGNORE INTO autopm_review_charges
           (request_id, round, step_index, provider, points, at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![slot.request_id, slot.round, slot.step_index, slot.provider, points, now],
    )?;
    conn.execute(
        "DELETE FROM autopm_review_slots WHERE request_id = ?1 AND round = ?2 AND step_index = ?3",
        params![slot.request_id, slot.round, slot.step_index],
    )?;
    Ok(())
}

/// The fitted points of a review run from its token counts.
pub fn run_points(provider: &str, model: &str, tokens: &Tokens) -> f64 {
    quota_rates::fitted_percent(provider, model, tokens)
}

/// Days into the window, for display.
pub fn days_into(window: &Window, now: i64) -> f64 {
    (now - window.start_ms) as f64 / DAY_MS as f64
}
