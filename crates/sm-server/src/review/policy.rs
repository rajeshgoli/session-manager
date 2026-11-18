//! Stored review policies and request-time resolution.

use anyhow::Result;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};
use std::path::Path;

use super::{chain, validate_reviewer};

pub fn ensure_schema(db: &Path) -> Result<()> {
    let conn = Connection::open(db)?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    crate::work_claims::init_work_claims_schema(&conn)?;
    crate::board::init_board_schema(&conn)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS review_policies (\
         scope TEXT NOT NULL, repo TEXT NOT NULL, number INTEGER NOT NULL,\
         reviewer_json TEXT NOT NULL, set_by_session_id TEXT,\
         set_by_name TEXT NOT NULL, set_at TEXT NOT NULL,\
         PRIMARY KEY(scope,repo,number))",
    )?;
    conn.execute_batch(AGENT_SCHEMA)?;
    Ok(())
}

/// One owner-chosen reviewer per agent (#2672). It follows the agent through
/// a handoff and wins over ticket, lane and repo policies, so it applies
/// before the agent has filed a ticket or opened a PR.
pub const AGENT_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS agent_review_policies (\
    session_id TEXT PRIMARY KEY, reviewer_json TEXT NOT NULL, set_by_session_id TEXT,\
    set_by_name TEXT NOT NULL, set_at TEXT NOT NULL)";

fn agent_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let reviewer: String = row.get(1)?;
    let reviewer: Value = serde_json::from_str(&reviewer).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(json!({"scope":"agent","session_id":row.get::<_,String>(0)?,
        "fallback":chain(&reviewer).into_iter().skip(1).collect::<Vec<_>>(),"reviewer":reviewer,
        "set_by_session_id":row.get::<_,Option<String>>(2)?,"set_by_name":row.get::<_,String>(3)?,
        "set_at":row.get::<_,String>(4)?}))
}

fn get_agent(conn: &Connection, session_id: &str) -> Result<Option<Value>> {
    Ok(conn
        .query_row(
            "SELECT session_id,reviewer_json,set_by_session_id,set_by_name,set_at \
             FROM agent_review_policies WHERE session_id=?1",
            [session_id],
            agent_row,
        )
        .optional()?)
}

/// Every agent policy, keyed by the agent's session id.
pub fn list_agents(db: &Path) -> Result<std::collections::BTreeMap<String, Value>> {
    if !db.exists() {
        return Ok(Default::default());
    }
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let exists = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='agent_review_policies'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .is_some();
    if !exists {
        return Ok(Default::default());
    }
    let mut stmt = conn.prepare(
        "SELECT session_id,reviewer_json,set_by_session_id,set_by_name,set_at \
         FROM agent_review_policies ORDER BY session_id",
    )?;
    let rows = stmt
        .query_map([], agent_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|p| (p["session_id"].as_str().unwrap_or_default().to_owned(), p))
        .collect())
}

pub struct AgentPolicyChange<'a> {
    pub agent_session_id: &'a str,
    pub reviewer: Option<&'a Value>,
    pub session_id: Option<&'a str>,
    pub name: &'a str,
    pub now: &'a str,
}

pub fn set_agent(db: &Path, change: AgentPolicyChange<'_>) -> Result<Option<Value>> {
    ensure_schema(db)?;
    let conn = Connection::open(db)?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    if let Some(reviewer) = change.reviewer {
        conn.execute(
            "INSERT INTO agent_review_policies (session_id,reviewer_json,set_by_session_id,set_by_name,set_at) \
             VALUES (?1,?2,?3,?4,?5) ON CONFLICT(session_id) DO UPDATE SET \
             reviewer_json=excluded.reviewer_json,set_by_session_id=excluded.set_by_session_id,\
             set_by_name=excluded.set_by_name,set_at=excluded.set_at",
            params![change.agent_session_id, reviewer.to_string(), change.session_id, change.name, change.now],
        )?;
    } else {
        conn.execute(
            "DELETE FROM agent_review_policies WHERE session_id=?1",
            [change.agent_session_id],
        )?;
    }
    get_agent(&conn, change.agent_session_id)
}

/// The agent whose review is being requested: the requester's session id
/// and the name that review sources show.
pub struct AgentRef<'a> {
    pub session_id: &'a str,
    pub name: &'a str,
}

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let reviewer: String = row.get(3)?;
    let reviewer: Value = serde_json::from_str(&reviewer).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(
        json!({"scope":row.get::<_,String>(0)?,"repo":row.get::<_,String>(1)?,
        "number":row.get::<_,i64>(2)?,"fallback":chain(&reviewer).into_iter().skip(1).collect::<Vec<_>>(),
        "reviewer":reviewer,"set_by_session_id":row.get::<_,Option<String>>(4)?,
        "set_by_name":row.get::<_,String>(5)?,"set_at":row.get::<_,String>(6)?}),
    )
}

fn get(conn: &Connection, scope: &str, repo: &str, number: i64) -> Result<Option<Value>> {
    Ok(conn
        .query_row(
            "SELECT scope,repo,number,reviewer_json,set_by_session_id,set_by_name,set_at \
         FROM review_policies WHERE scope=?1 AND repo=?2 AND number=?3",
            params![scope, repo, number],
            row,
        )
        .optional()?)
}

pub fn list(db: &Path) -> Result<Vec<Value>> {
    if !db.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let exists = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='review_policies'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .is_some();
    if !exists {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT scope,repo,number,reviewer_json,set_by_session_id,set_by_name,set_at \
        FROM review_policies ORDER BY scope,repo,number",
    )?;
    let rows = stmt
        .query_map([], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub struct PolicyChange<'a> {
    pub scope: &'a str,
    pub repo: &'a str,
    pub number: i64,
    pub reviewer: Option<&'a Value>,
    pub session_id: Option<&'a str>,
    pub name: &'a str,
    pub now: &'a str,
}

pub fn set(db: &Path, change: PolicyChange<'_>) -> Result<Option<Value>> {
    let PolicyChange {
        scope,
        repo,
        number,
        reviewer,
        session_id,
        name,
        now,
    } = change;
    ensure_schema(db)?;
    let mut conn = Connection::open(db)?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    let tx = conn.transaction()?;
    if let Some(reviewer) = reviewer {
        tx.execute("INSERT INTO review_policies (scope,repo,number,reviewer_json,set_by_session_id,set_by_name,set_at) \
            VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(scope,repo,number) DO UPDATE SET \
            reviewer_json=excluded.reviewer_json,set_by_session_id=excluded.set_by_session_id,\
            set_by_name=excluded.set_by_name,set_at=excluded.set_at",
            params![scope,repo,number,reviewer.to_string(),session_id,name,now])?;
    } else {
        tx.execute(
            "DELETE FROM review_policies WHERE scope=?1 AND repo=?2 AND number=?3",
            params![scope, repo, number],
        )?;
    }
    let lane_id: Option<i64> = match scope {
        "lane" => tx
            .query_row(
                "SELECT id FROM board_lanes WHERE goal_repo=?1 AND goal_number=?2 \
            AND ended_at IS NULL",
                params![repo, number],
                |r| r.get(0),
            )
            .optional()?,
        "ticket" => tx
            .query_row(
                "SELECT l.id FROM board_members m JOIN board_lanes l ON l.id=m.lane_id \
            WHERE m.repo=?1 AND m.number=?2 AND l.ended_at IS NULL ORDER BY l.rank LIMIT 1",
                params![repo, number],
                |r| r.get(0),
            )
            .optional()?,
        _ => None,
    };
    tx.execute(
        "INSERT INTO board_events(ts,kind,lane_id,repo,number,actor,actor_name,detail) \
        VALUES(?1,'review_policy',?2,?3,?4,?5,?6,?7)",
        params![
            now,
            lane_id,
            repo,
            number,
            session_id
                .map(|s| format!("sm:{s}"))
                .unwrap_or_else(|| "sm:owner".into()),
            name,
            if reviewer.is_some() { "set" } else { "cleared" }
        ],
    )?;
    tx.commit()?;
    get(&conn, scope, repo, number)
}

pub fn linked_tickets(conn: &Connection, repo: &str, pr: i64) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT repo,ticket_number FROM work_links WHERE repo=?1 AND pr_number=?2 \
        UNION SELECT repo,issue_number FROM board_prs WHERE pr_repo=?1 AND pr_number=?2",
    )?;
    let rows = stmt
        .query_map(params![repo, pr], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn source(policy: &Value) -> String {
    match policy["scope"].as_str().unwrap_or("") {
        "ticket" => format!("ticket #{}", policy["number"]),
        "lane" => format!(
            "lane {}#{}",
            policy["repo"].as_str().unwrap_or(""),
            policy["number"]
        ),
        _ => format!("repo {}", policy["repo"].as_str().unwrap_or("")),
    }
}

pub fn resolve(
    db: &Path,
    repo: &str,
    pr: Option<i64>,
    ticket: Option<i64>,
    lane: Option<i64>,
    default: &Value,
) -> Result<Value> {
    resolve_for(db, repo, pr, ticket, lane, None, default)
}

/// `resolve`, with the requesting agent's own policy first (#2672).
pub fn resolve_for(
    db: &Path,
    repo: &str,
    pr: Option<i64>,
    ticket: Option<i64>,
    lane: Option<i64>,
    agent: Option<&AgentRef<'_>>,
    default: &Value,
) -> Result<Value> {
    ensure_schema(db)?;
    let conn = Connection::open(db)?;
    if let Some(agent) = agent {
        if let Some(p) = get_agent(&conn, agent.session_id)? {
            // A paired reviewer for an agent is keyed on the agent, not a ticket.
            return Ok(json!({"reviewer":p["reviewer"],"fallback":p["fallback"],
                "source":format!("agent {}", agent.name),
                "agent":{"session_id":agent.session_id,"name":agent.name}}));
        }
    }
    let mut tickets = if let Some(pr) = pr {
        linked_tickets(&conn, repo, pr)?
    } else {
        ticket
            .map(|n| vec![(repo.to_owned(), n)])
            .unwrap_or_default()
    };
    let mut ranked = tickets
        .into_iter()
        .map(|(ticket_repo, number)| {
            let rank=conn.query_row(
            "SELECT l.rank,r.rank FROM board_members m JOIN board_lanes l ON l.id=m.lane_id \
            LEFT JOIN board_ticket_ranks r ON r.repo=m.repo AND r.number=m.number \
            WHERE m.repo=?1 AND m.number=?2 AND l.ended_at IS NULL ORDER BY l.rank LIMIT 1",
            params![ticket_repo,number],
            |r|Ok((r.get::<_,i64>(0)?,r.get::<_,Option<i64>>(1)?)),
        ).optional()?.map(|(lane,ticket)|(lane,ticket.unwrap_or(i64::MAX),number))
            .unwrap_or((i64::MAX,i64::MAX,number));
            Ok((rank, (ticket_repo, number)))
        })
        .collect::<Result<Vec<_>>>()?;
    ranked.sort_by_key(|(rank, _)| *rank);
    tickets = ranked.into_iter().map(|(_, ticket)| ticket).collect();
    for (r, n) in &tickets {
        if let Some(p) = get(&conn, "ticket", r, *n)? {
            // A paired reviewer needs to know whose ticket it serves.
            return Ok(json!({"reviewer":p["reviewer"],"fallback":p["fallback"],
                "source":source(&p),"ticket":{"repo":r,"number":n}}));
        }
    }
    let mut lanes: Vec<(i64, String, i64)> = Vec::new();
    if let Some(goal) = lane {
        lanes.push((0, repo.to_owned(), goal));
    } else {
        for (r, n) in &tickets {
            let mut stmt=conn.prepare("SELECT l.rank,l.goal_repo,l.goal_number FROM board_members m \
                JOIN board_lanes l ON l.id=m.lane_id WHERE m.repo=?1 AND m.number=?2 AND l.ended_at IS NULL")?;
            lanes.extend(
                stmt.query_map(params![r, n], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
    }
    lanes.sort();
    for (_, r, n) in lanes {
        if let Some(p) = get(&conn, "lane", &r, n)? {
            return Ok(
                json!({"reviewer":p["reviewer"],"fallback":p["fallback"],"source":source(&p)}),
            );
        }
    }
    if let Some(p) = get(&conn, "repo", repo, 0)? {
        return Ok(json!({"reviewer":p["reviewer"],"fallback":p["fallback"],"source":source(&p)}));
    }
    Ok(
        json!({"reviewer":default,"fallback":chain(default).into_iter().skip(1).collect::<Vec<_>>(),"source":"default"}),
    )
}

pub fn validate(scope: &str, reviewer: &Value) -> Result<(), String> {
    if reviewer["kind"] == "paired" {
        if scope != "ticket" && scope != "agent" {
            return Err("A paired reviewer can only be set on a ticket or an agent.".into());
        }
        let mut run = reviewer.clone();
        let provider = reviewer["provider"].as_str().unwrap_or("");
        run["kind"] = json!(provider);
        run.as_object_mut().unwrap().remove("provider");
        validate_reviewer(&run)
    } else {
        validate_reviewer(reviewer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Keep the scenario fixture calls compact while production uses PolicyChange.
    #[allow(clippy::too_many_arguments)]
    fn set(
        db: &Path,
        scope: &str,
        repo: &str,
        number: i64,
        reviewer: Option<&Value>,
        session_id: Option<&str>,
        name: &str,
        now: &str,
    ) -> Result<Option<Value>> {
        super::set(
            db,
            PolicyChange {
                scope,
                repo,
                number,
                reviewer,
                session_id,
                name,
                now,
            },
        )
    }

    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn setup() -> (Scratch, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "sm-policy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        let dir = Scratch(path);
        let db = dir.0.join("policies.db");
        ensure_schema(&db).unwrap();
        let c = Connection::open(&db).unwrap();
        c.execute_batch("INSERT INTO board_lanes(id,goal_repo,goal_number,rank,added_at,added_by,added_by_name) VALUES
            (1,'far/repo',1843,1,'now','owner','Rajesh'),
            (2,'far/repo',1884,3,'now','owner','Rajesh'),
            (3,'rajeshgoli/session-manager',1768,2,'now','owner','Rajesh');
            INSERT INTO board_members(lane_id,repo,number,joined_at) VALUES
            (1,'far/repo',1848,'now'),(1,'far/repo',1873,'now'),(1,'far/repo',1886,'now'),
            (2,'far/repo',1885,'now'),(3,'rajeshgoli/session-manager',1768,'now');
            INSERT INTO board_ticket_ranks(repo,number,rank,lane_id) VALUES
            ('far/repo',1848,1,1),('far/repo',1873,2,1),('far/repo',1886,3,1),
            ('far/repo',1885,1,2),('rajeshgoli/session-manager',1768,1,3);
            INSERT INTO work_links(repo,pr_number,ticket_number,source,created_at) VALUES
            ('far/repo',1851,1848,'test','now'),('far/repo',1875,1873,'test','now'),
            ('far/repo',1890,1885,'test','now'),('far/repo',1890,1886,'test','now'),
            ('rajeshgoli/session-manager',1780,1768,'test','now'),
            ('rajeshgoli/session-manager',1780,1770,'test','now');").unwrap();
        (dir, db)
    }

    #[test]
    fn five_spec_resolution_examples_and_fixed_fallbacks() {
        let (_dir, db) = setup();
        let default = json!({"kind":"github_codex"});
        let top = json!({"kind":"codex","model":"gpt-6-astra","effort":"high"});
        let ticket = json!({"kind":"claude","model":"fable","effort":"max"});
        let repo = json!({"kind":"codex","model":"gpt-6-sol","effort":"medium"});
        let no_policy = resolve(
            &db,
            "rajeshgoli/session-manager",
            Some(1790),
            None,
            None,
            &default,
        )
        .unwrap();
        assert_eq!(no_policy["source"], "default");
        assert_eq!(no_policy["fallback"].as_array().unwrap().len(), 2);
        set(
            &db,
            "lane",
            "far/repo",
            1843,
            Some(&top),
            None,
            "Rajesh",
            "now",
        )
        .unwrap();
        set(
            &db,
            "lane",
            "far/repo",
            1884,
            Some(&ticket),
            None,
            "Rajesh",
            "now",
        )
        .unwrap();
        set(
            &db,
            "ticket",
            "far/repo",
            1848,
            Some(&ticket),
            Some("planner"),
            "Planner",
            "now",
        )
        .unwrap();
        set(
            &db,
            "repo",
            "rajeshgoli/session-manager",
            0,
            Some(&repo),
            None,
            "Rajesh",
            "now",
        )
        .unwrap();
        for (r, pr, source, kind) in [
            ("far/repo", 1851, "ticket #1848", "claude"),
            ("far/repo", 1875, "lane far/repo#1843", "codex"),
            (
                "rajeshgoli/session-manager",
                1780,
                "repo rajeshgoli/session-manager",
                "codex",
            ),
            ("far/repo", 1890, "lane far/repo#1843", "codex"),
        ] {
            let result = resolve(&db, r, Some(pr), None, None, &default).unwrap();
            assert_eq!(result["source"], source);
            assert_eq!(result["reviewer"]["kind"], kind);
            assert_eq!(result["fallback"].as_array().unwrap().len(), 1);
        }
    }

    #[test]
    fn board_event_and_clear_follow_stored_policy() {
        let (_dir, db) = setup();
        let reviewer = json!({"kind":"claude","model":"fable","effort":"max"});
        let saved = set(
            &db,
            "ticket",
            "far/repo",
            1848,
            Some(&reviewer),
            Some("planner"),
            "Planner",
            "now",
        )
        .unwrap()
        .unwrap();
        assert_eq!(saved["set_by_name"], "Planner");
        assert_eq!(saved["fallback"][0]["model"], "gpt-6-astra");
        assert!(
            set(&db, "ticket", "far/repo", 1848, None, None, "Rajesh", "later")
                .unwrap()
                .is_none()
        );
        let c = Connection::open(&db).unwrap();
        let events: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM board_events WHERE kind='review_policy' AND lane_id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 2);
    }

    #[test]
    fn agent_policy_wins_and_clears() {
        let (_dir, db) = setup();
        let default = json!({"kind":"github_codex"});
        let ticket = json!({"kind":"claude","model":"fable","effort":"max"});
        let paired = json!({"kind":"paired","provider":"claude","model":"opus","effort":"high"});
        set(
            &db,
            "ticket",
            "far/repo",
            1848,
            Some(&ticket),
            None,
            "Rajesh",
            "now",
        )
        .unwrap();
        let agent = AgentRef {
            session_id: "a1",
            name: "far-artifacts",
        };
        // No agent policy yet: the PR's ticket decides.
        let before = resolve_for(
            &db,
            "far/repo",
            Some(1851),
            None,
            None,
            Some(&agent),
            &default,
        )
        .unwrap();
        assert_eq!(before["source"], "ticket #1848");
        let saved = set_agent(
            &db,
            AgentPolicyChange {
                agent_session_id: "a1",
                reviewer: Some(&paired),
                session_id: None,
                name: "Rajesh",
                now: "now",
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(saved["scope"], "agent");
        assert_eq!(saved["fallback"][0]["kind"], "claude");
        // The agent's policy wins over the ticket's, and applies to a PR with no ticket.
        for pr in [1851, 4242] {
            let result = resolve_for(
                &db,
                "far/repo",
                Some(pr),
                None,
                None,
                Some(&agent),
                &default,
            )
            .unwrap();
            assert_eq!(result["source"], "agent far-artifacts");
            assert_eq!(result["reviewer"]["kind"], "paired");
            assert_eq!(result["agent"]["session_id"], "a1");
        }
        // Another agent's request is untouched.
        let other = AgentRef {
            session_id: "a2",
            name: "other",
        };
        let result = resolve_for(
            &db,
            "far/repo",
            Some(4242),
            None,
            None,
            Some(&other),
            &default,
        )
        .unwrap();
        assert_eq!(result["source"], "default");
        assert_eq!(list_agents(&db).unwrap().len(), 1);
        set_agent(
            &db,
            AgentPolicyChange {
                agent_session_id: "a1",
                reviewer: None,
                session_id: None,
                name: "Rajesh",
                now: "later",
            },
        )
        .unwrap();
        assert!(list_agents(&db).unwrap().is_empty());
    }

    #[test]
    fn paired_is_ticket_or_agent_only_and_fields_are_validated() {
        let reviewer =
            json!({"kind":"paired","provider":"codex","model":"gpt-6-astra","effort":"high"});
        assert_eq!(
            validate("repo", &reviewer).unwrap_err(),
            "A paired reviewer can only be set on a ticket or an agent."
        );
        assert!(validate("ticket", &reviewer).is_ok());
        assert!(validate("agent", &reviewer).is_ok());
        // A well-formed Codex model passes here; the write route checks the
        // live catalog (#2674). A malformed one fails the shape check.
        let bad = json!({"kind":"codex","model":"not a model","effort":"high"});
        assert!(validate("ticket", &bad).unwrap_err().contains("model"));
    }
}
