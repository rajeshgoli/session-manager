//! Weekly asks and strict numbered replies. Durable answers survive effect retries.
use super::{
    experiments::{self, Ctx},
    Fail, Out, Refusal, DAY_MS,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub experiment_id: String,
    pub text: String,
}

pub fn schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS autopm_asks (
        cycle INTEGER PRIMARY KEY, decisions TEXT NOT NULL, sender TEXT NOT NULL,
        created_ms INTEGER NOT NULL, message_id TEXT UNIQUE);
        CREATE TABLE IF NOT EXISTS autopm_answers (
        cycle INTEGER NOT NULL, ordinal INTEGER NOT NULL, yes INTEGER NOT NULL,
        note TEXT NOT NULL, applied INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(cycle,ordinal));
        CREATE TABLE IF NOT EXISTS autopm_ask_replies (id TEXT PRIMARY KEY);",
    )?;
    Ok(())
}
fn bad(gate: &str, text: &str) -> Fail {
    Refusal::bad(gate, text).into()
}

pub fn validate(decisions: &[Decision]) -> Out<()> {
    if decisions.len() > 3 {
        return Err(bad(
            "too_many_decisions",
            "an ask holds at most three decisions",
        ));
    }
    if decisions.is_empty() {
        return Err(bad("decisions", "an ask needs at least one decision"));
    }
    let mut ids = std::collections::BTreeSet::new();
    for d in decisions {
        if d.text.trim().is_empty() || d.text.split_whitespace().count() > 120 {
            return Err(bad("decision_words", "each decision needs 1–120 words"));
        }
        if !ids.insert(&d.experiment_id) {
            return Err(bad("decisions", "duplicate experiment"));
        }
    }
    Ok(())
}

pub trait Effects {
    fn message(&self, key: &str, sender: &str, title: &str, body: &str) -> anyhow::Result<String>;
    fn clarify(&self, key: &str, text: &str) -> anyhow::Result<()>;
    fn proposal(&self, experiment: &experiments::Experiment) -> anyhow::Result<(String, i64)>;
}

pub fn send(
    ctx: &Ctx<'_>,
    cycle: i64,
    sender: &str,
    decisions: &[Decision],
    effects: &dyn Effects,
) -> Out<String> {
    validate(decisions)?;
    let encoded = serde_json::to_string(decisions).map_err(anyhow::Error::from)?;
    let mut conn = ctx.store.open()?;
    schema(&conn)?;
    let existing: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT decisions,message_id FROM autopm_asks WHERE cycle=?1",
            [cycle],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((old, Some(message))) = &existing {
        if *old == encoded {
            return Ok(message.clone());
        }
    }
    if existing.as_ref().is_some_and(|(old, _)| *old != encoded) {
        return Err(
            Refusal::gate("weekly_ask", "this cycle already has its decision message").into(),
        );
    }
    for d in decisions {
        if existing.is_none() {
            let already: bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM autopm_asks a,json_each(a.decisions) d WHERE json_extract(d.value,'$.experiment_id')=?1)", [&d.experiment_id], |r|r.get(0))?;
            if already {
                return Err(Refusal::gate("already_asked","this experiment already has a decision thread; materially new evidence needs a new proposal").into());
            }
        }
        let x = experiments::get(&conn, &d.experiment_id)?
            .ok_or_else(|| Refusal::not_found("experiment not found"))?;
        if x.state != "awaiting_owner" || !matches!(x.path.as_str(), "shadow" | "proposal") {
            return Err(Refusal::gate("state", "each decision must be awaiting the owner").into());
        }
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT OR IGNORE INTO autopm_asks(cycle,decisions,sender,created_ms) VALUES(?1,?2,?3,?4)",
        params![cycle, encoded, sender, ctx.now],
    )?;
    let (stored, original_sender): (String, String) = tx.query_row(
        "SELECT decisions,sender FROM autopm_asks WHERE cycle=?1",
        [cycle],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if stored != encoded {
        return Err(
            Refusal::gate("weekly_ask", "this cycle already has its decision message").into(),
        );
    }
    tx.commit()?;
    let body = decisions
        .iter()
        .enumerate()
        .map(|(i, d)| format!("{}. {}", i + 1, d.text.trim()))
        .collect::<Vec<_>>()
        .join("\n\n")
        + "\n\nReply by number: 1 yes, 2 no. No answer within 14 days means no.";
    let id = effects.message(
        &format!("autopm-ask:{cycle}"),
        &original_sender,
        &format!("Auto PM: {} decisions", decisions.len()),
        &body,
    )?;
    conn.execute(
        "UPDATE autopm_asks SET message_id=?1 WHERE cycle=?2",
        params![id, cycle],
    )?;
    Ok(id)
}

/// Reject the whole reply on unknown text, duplicate numbers, or out-of-range numbers.
/// A partial numbered reply is valid; unanswered decisions keep their deadline.
pub fn parse(text: &str, count: usize) -> Option<BTreeMap<usize, bool>> {
    let text = text.trim().to_ascii_lowercase();
    if count == 1 && matches!(text.as_str(), "yes" | "no") {
        return Some(BTreeMap::from([(1, text == "yes")]));
    }
    let re = regex::Regex::new(r"([1-3])\s*[:.)-]?\s*(yes|no)\b").ok()?;
    let mut end = 0;
    let mut result = BTreeMap::new();
    for cap in re.captures_iter(&text) {
        let all = cap.get(0)?;
        if !text[end..all.start()]
            .chars()
            .all(|c| c.is_whitespace() || matches!(c, ',' | ';'))
        {
            return None;
        }
        let number = cap[1].parse::<usize>().ok()?;
        if number > count || result.insert(number, &cap[2] == "yes").is_some() {
            return None;
        }
        end = all.end();
    }
    if result.is_empty()
        || !text[end..]
            .chars()
            .all(|c| c.is_whitespace() || matches!(c, ',' | ';' | '.'))
    {
        return None;
    }
    Some(result)
}

pub fn reply(
    ctx: &Ctx<'_>,
    message: &str,
    id: &str,
    text: &str,
    at: i64,
    effects: &dyn Effects,
) -> Out<()> {
    let mut conn = ctx.store.open()?;
    schema(&conn)?;
    let ask: Option<(i64, String, i64)> = conn
        .query_row(
            "SELECT cycle,decisions,created_ms FROM autopm_asks WHERE message_id=?1",
            [message],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((cycle, encoded, created)) = ask else {
        return Ok(());
    };
    let decisions: Vec<Decision> = serde_json::from_str(&encoded).map_err(anyhow::Error::from)?;
    let seen: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM autopm_ask_replies WHERE id=?1)",
        [id],
        |r| r.get(0),
    )?;
    if seen {
        return Ok(());
    }
    if at < created || at >= created + 14 * DAY_MS {
        return Ok(());
    }
    let Some(parsed) = parse(text, decisions.len()) else {
        effects.clarify(&format!("autopm-clarify:{id}"),"[sm autopm] Could not parse the owner's decision reply. Ask again in plain words in the existing thread; use numbered yes/no decisions.")?;
        conn.execute("INSERT OR IGNORE INTO autopm_ask_replies VALUES(?1)", [id])?;
        return Ok(());
    };
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (ordinal, yes) in parsed {
        tx.execute(
            "INSERT OR IGNORE INTO autopm_answers(cycle,ordinal,yes,note) VALUES(?1,?2,?3,?4)",
            params![
                cycle,
                ordinal as i64,
                yes,
                if yes {
                    "owner said yes"
                } else {
                    "owner said no"
                }
            ],
        )?;
    }
    tx.execute("INSERT OR IGNORE INTO autopm_ask_replies VALUES(?1)", [id])?;
    tx.commit()?;
    Ok(())
}

/// Process persisted answers before expiring unanswered items. The caller replays owner
/// replies first, so a timely reply still wins after downtime past the deadline.
pub fn apply(ctx: &Ctx<'_>, effects: &dyn Effects) -> Out<()> {
    let conn = ctx.store.open()?;
    schema(&conn)?;
    let asks = {
        let mut s = conn.prepare(
            "SELECT cycle,decisions,created_ms FROM autopm_asks WHERE message_id IS NOT NULL",
        )?;
        let rows = s
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for (cycle, encoded, created) in asks {
        let decisions: Vec<Decision> =
            serde_json::from_str(&encoded).map_err(anyhow::Error::from)?;
        for (i, d) in decisions.iter().enumerate() {
            let ordinal = (i + 1) as i64;
            if ctx.now >= created + 14 * DAY_MS {
                conn.execute("INSERT OR IGNORE INTO autopm_answers(cycle,ordinal,yes,note) VALUES(?1,?2,0,'no answer')",params![cycle,ordinal])?;
            }
            let answer: Option<(bool, String, bool)> = conn
                .query_row(
                    "SELECT yes,note,applied FROM autopm_answers WHERE cycle=?1 AND ordinal=?2",
                    params![cycle, ordinal],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((yes, mut note, false)) = answer else {
                continue;
            };
            let x = experiments::get(&conn, &d.experiment_id)?
                .ok_or_else(|| Refusal::not_found("experiment not found"))?;
            if x.state == "awaiting_owner" {
                if yes && x.path == "proposal" {
                    let (repo, number) = effects.proposal(&x)?;
                    note = format!("handed to {repo}#{number}");
                }
                experiments::answer(ctx, &x.id, yes, &note)?;
            }
            conn.execute(
                "UPDATE autopm_answers SET applied=1 WHERE cycle=?1 AND ordinal=?2",
                params![cycle, ordinal],
            )?;
        }
    }
    Ok(())
}
