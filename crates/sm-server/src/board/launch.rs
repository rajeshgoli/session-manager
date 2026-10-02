//! Web-only saved launch choices and atomic, previewed selection edits (#1949).
use super::*;
use crate::owner_settings::NewAgentSettings;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub provider: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub brief: Option<String>,
}
impl Config {
    pub fn validate(&self) -> Option<&'static str> {
        let efforts: &[&str] = match self.provider.as_str() {
            "claude" => &["low", "medium", "high", "xhigh", "max"],
            "codex-fork" => &["medium", "high", "xhigh"],
            _ => return Some("Provider must be Claude or Codex"),
        };
        if self.model.as_ref().is_some_and(|s| s.trim().is_empty()) {
            return Some("Model must be nonempty or null");
        }
        if self
            .reasoning_effort
            .as_deref()
            .is_some_and(|s| !efforts.contains(&s))
        {
            return Some("Effort is invalid for this provider");
        }
        if self
            .brief
            .as_ref()
            .is_some_and(|s| s.trim().is_empty() || s.len() > 32_000)
        {
            return Some("Custom first message must be nonblank and at most 32000 bytes");
        }
        None
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConfigEdit {
    Keep,
    Set {
        provider: String,
        model: Option<String>,
        reasoning_effort: Option<String>,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum MessageEdit {
    Keep,
    Default,
    Custom { text: String },
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Behavior {
    Keep,
    Manual,
    WhenReady,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub config: ConfigEdit,
    pub message: MessageEdit,
    pub behavior: Behavior,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub repo: String,
    pub number: i64,
}
impl Selection {
    fn key(&self) -> Key {
        (canonical_repo(&self.repo), self.number)
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Exception {
    pub repo: String,
    pub number: i64,
    pub config: Option<ConfigEdit>,
    pub message: Option<MessageEdit>,
    pub behavior: Option<Behavior>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Preview {
    pub selection: Vec<Selection>,
    pub common: Edit,
    #[serde(default)]
    pub exceptions: Vec<Exception>,
    pub last_agent_type: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct PlanItem {
    key: Key,
    config: Config,
    behavior: Behavior,
    changed: bool,
    eligible: bool,
}
pub type Reply = (u16, Value);
fn error(status: u16, code: &str, detail: &str) -> Reply {
    (status, json!({"code":code,"detail":detail}))
}

pub(super) fn schema(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS board_launch_preferences (
      repo TEXT NOT NULL, number INTEGER NOT NULL, config TEXT NOT NULL,
      revision INTEGER NOT NULL DEFAULT 1, source TEXT NOT NULL, source_lane_id INTEGER,
      PRIMARY KEY(repo,number));
    CREATE TABLE IF NOT EXISTS board_launch_defaults (lane_id INTEGER PRIMARY KEY, config TEXT, revision INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS board_launch_seen (repo TEXT NOT NULL, number INTEGER NOT NULL, PRIMARY KEY(repo,number));
    CREATE TABLE IF NOT EXISTS board_launch_previews (token TEXT PRIMARY KEY, expires INTEGER NOT NULL, body TEXT NOT NULL, snapshot TEXT NOT NULL, plan TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS board_launch_requests (id TEXT PRIMARY KEY, token TEXT NOT NULL, response TEXT NOT NULL, created INTEGER NOT NULL);")?;
    if conn
        .prepare("SELECT revision FROM auto_starts LIMIT 0")
        .is_err()
    {
        conn.execute(
            "ALTER TABLE auto_starts ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
            [],
        )?;
    }
    conn.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS board_auto_revision AFTER UPDATE ON auto_starts
      WHEN NEW.revision = OLD.revision BEGIN
      UPDATE auto_starts SET revision=OLD.revision+1 WHERE repo=NEW.repo AND number=NEW.number; END;
    INSERT OR IGNORE INTO board_launch_seen SELECT repo,number FROM board_items
      WHERE NOT EXISTS (SELECT 1 FROM board_settings WHERE key='launch_initialized');
    INSERT OR IGNORE INTO board_settings(key,value) VALUES('launch_initialized','1');",
    )?;
    Ok(())
}
fn preference(conn: &Connection, key: &Key) -> Result<Value> {
    let row: Option<(String,i64,String,Option<i64>)> = conn.query_row(
        "SELECT config,revision,source,source_lane_id FROM board_launch_preferences WHERE repo=?1 AND number=?2",
        params![key.0,key.1], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    Ok(match row {
        Some((config, revision, source, lane)) => {
            json!({"config":serde_json::from_str::<Value>(&config)?,"revision":revision,"source":source,"source_lane_id":lane})
        }
        None => Value::Null,
    })
}
fn lane_default(conn: &Connection, lane: i64) -> Result<Value> {
    let row: Option<(Option<String>, i64)> = conn
        .query_row(
            "SELECT config,revision FROM board_launch_defaults WHERE lane_id=?1",
            [lane],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (config, revision) = row.unwrap_or((None, 0));
    Ok(
        json!({"lane_id":lane,"config":config.map(|s|serde_json::from_str::<Value>(&s)).transpose()?,"revision":revision}),
    )
}
fn auto(conn: &Connection, key: &Key) -> Result<Value> {
    Ok(conn.query_row("SELECT provider,model,effort,brief,state,attempts,last_error,revision FROM auto_starts WHERE repo=?1 AND number=?2",params![key.0,key.1],|r| Ok(json!({
        "config":{"provider":r.get::<_,String>(0)?,"model":r.get::<_,Option<String>>(1)?,"reasoning_effort":r.get::<_,Option<String>>(2)?,"brief":r.get::<_,Option<String>>(3)?},
        "state":r.get::<_,String>(4)?,"attempts":r.get::<_,i64>(5)?,"last_error":r.get::<_,Option<String>>(6)?,"revision":r.get::<_,i64>(7)?}))).optional()?.unwrap_or(Value::Null))
}
fn resolve(
    conn: &Connection,
    key: &Key,
    settings: &NewAgentSettings,
    last: Option<&str>,
) -> Result<(Config, String)> {
    let a = auto(conn, key)?;
    if matches!(a["state"].as_str(), Some("waiting" | "failed")) {
        return Ok((
            serde_json::from_value(a["config"].clone())?,
            "authorization".into(),
        ));
    }
    let pref = preference(conn, key)?;
    if !pref.is_null() && pref["source"] != "lane" {
        return Ok((
            serde_json::from_value(pref["config"].clone())?,
            pref["source"].as_str().unwrap_or("ticket").into(),
        ));
    }
    let tier: Option<String> = conn
        .query_row(
            "SELECT tier FROM board_items WHERE repo=?1 AND number=?2",
            params![key.0, key.1],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    for (name, source) in [(tier.as_deref(), "ticket Tier"), (last, "last used")] {
        if source == "last used" && !pref.is_null() {
            return Ok((
                serde_json::from_value(pref["config"].clone())?,
                "lane".into(),
            ));
        }
        if let Some(t) = name.and_then(|n| {
            settings
                .agent_types
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case(n))
        }) {
            return Ok((
                Config {
                    provider: t.provider.clone(),
                    model: Some(t.model.clone()),
                    reasoning_effort: Some(t.effort.clone()),
                    brief: None,
                },
                source.into(),
            ));
        }
    }
    let defaults = settings.provider_defaults();
    Ok((
        Config {
            provider: settings.provider.clone(),
            model: defaults.model.clone(),
            reasoning_effort: defaults.effort.clone(),
            brief: None,
        },
        "global defaults".into(),
    ))
}
fn save_pref(
    conn: &Connection,
    key: &Key,
    config: &Config,
    source: &str,
    lane: Option<i64>,
) -> Result<()> {
    conn.execute("INSERT INTO board_launch_preferences(repo,number,config,source,source_lane_id) VALUES(?1,?2,?3,?4,?5)
      ON CONFLICT(repo,number) DO UPDATE SET config=excluded.config,source=excluded.source,source_lane_id=excluded.source_lane_id,revision=board_launch_preferences.revision+1",
      params![key.0,key.1,serde_json::to_string(config)?,source,lane])?;
    Ok(())
}
// Include PRs attributed by explicit links or by the ticket holder's claim interval.
// GitHub's board reference takes precedence over an older claim-cache state.
fn attributed_prs(conn: &Connection, key: &Key) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT p.number, UPPER(COALESCE(
          (SELECT b.pr_state FROM board_prs b WHERE b.pr_repo=p.repo AND b.pr_number=p.number LIMIT 1),
          p.state, 'OPEN'))
         FROM work_items p WHERE p.repo=?1 AND p.kind='pr' AND (
          EXISTS (SELECT 1 FROM work_links l WHERE l.repo=p.repo AND l.pr_number=p.number AND l.ticket_number=?2)
          OR EXISTS (SELECT 1 FROM work_claims t JOIN work_claims r
            ON r.session_id=t.session_id AND r.repo=t.repo AND r.kind='pr' AND r.number=p.number
            WHERE t.repo=?1 AND t.number=?2 AND t.kind='ticket'
              AND t.reserved_at IS NULL AND r.reserved_at IS NULL
              AND r.claimed_at>=t.claimed_at AND (t.ended_at IS NULL OR r.claimed_at<=t.ended_at)))
         ORDER BY p.number",
    )?;
    let rows = stmt
        .query_map(params![key.0, key.1], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
fn snapshot(conn: &Connection, board: &Board, req: &Preview, settings: &Value) -> Result<Value> {
    let mut rows = Vec::new();
    for t in &req.selection {
        let key = t.key();
        let claims=conn.prepare("SELECT id,session_id,reserved_at FROM work_claims WHERE repo=?1 AND number=?2 AND kind='ticket' AND ended_at IS NULL ORDER BY id")?
            .query_map(params![key.0,key.1],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let tier: Option<String> = conn
            .query_row(
                "SELECT tier FROM board_items WHERE repo=?1 AND number=?2",
                params![key.0, key.1],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let facts=board.facts.get(&key).map(|f|json!({"state":f.state.as_str(),"open":f.item.is_open(),"warnings":f.warnings,"waits_on":f.waits_on,"prs":f.prs,"holder":f.holder.as_ref().map(|h|(&h.session_id,h.state.as_str()))}));
        let goals: Vec<_> = board
            .lanes
            .iter()
            .filter(|l| l.lane.goal == key)
            .map(|l| l.lane.id)
            .collect();
        rows.push(json!({"key":key,"facts":facts,"tier":tier,"claims":claims,"attributed_prs":attributed_prs(conn,&key)?,"goals":goals,"preference":preference(conn,&key)?,"auto":auto(conn,&key)?}));
    }
    Ok(json!({"rows":rows,"settings":settings}))
}
fn build_plan(
    conn: &Connection,
    board: &Board,
    req: &Preview,
    settings: &NewAgentSettings,
) -> Result<(Value, Vec<PlanItem>)> {
    let mut items = Vec::new();
    let mut plan = Vec::new();
    let (mut ready, mut blocked, mut renew, mut cancel) = (0, 0, 0, 0);
    for t in &req.selection {
        let key = t.key();
        let mut reasons = Vec::new();
        let facts = board.facts.get(&key);
        match facts {
            None => reasons.push("not_on_board"),
            Some(f) => {
                if !f.item.is_open() {
                    reasons.push("closed");
                }
                if !matches!(f.state, TicketState::Ready | TicketState::Blocked) {
                    reasons.push("state");
                }
                if f.prs.iter().any(|p| p.state.eq_ignore_ascii_case("open")) {
                    reasons.push("open_pr");
                }
                for warning in ["stale", "cycle", "merged_not_closed"] {
                    if f.warnings.contains(&warning) {
                        reasons.push(warning);
                    }
                }
            }
        }
        if conn.query_row("SELECT EXISTS(SELECT 1 FROM work_claims WHERE repo=?1 AND number=?2 AND kind='ticket' AND ended_at IS NULL)",params![key.0,key.1],|r|r.get::<_,bool>(0))? {reasons.push("held");}
        let linked = attributed_prs(conn, &key)?;
        if linked.iter().any(|(_, state)| state == "OPEN") && !reasons.contains(&"open_pr") {
            reasons.push("open_pr");
        }
        if linked.iter().any(|(_, state)| state == "MERGED")
            && !reasons.contains(&"merged_not_closed")
        {
            reasons.push("merged_not_closed");
        }
        if board.lanes.iter().any(|l| l.lane.goal == key) {
            reasons.push("lane_goal");
        }
        let (mut config, source) = resolve(conn, &key, settings, req.last_agent_type.as_deref())?;
        let ex = req
            .exceptions
            .iter()
            .find(|e| (canonical_repo(&e.repo), e.number) == key);
        let ce = ex
            .and_then(|e| e.config.as_ref())
            .unwrap_or(&req.common.config);
        let me = ex
            .and_then(|e| e.message.as_ref())
            .unwrap_or(&req.common.message);
        let behavior = ex
            .and_then(|e| e.behavior.as_ref())
            .unwrap_or(&req.common.behavior)
            .clone();
        if let ConfigEdit::Set {
            provider,
            model,
            reasoning_effort,
        } = ce
        {
            config.provider = provider.clone();
            config.model = model.clone();
            config.reasoning_effort = reasoning_effort.clone();
        }
        match me {
            MessageEdit::Keep => {}
            MessageEdit::Default => config.brief = None,
            MessageEdit::Custom { text } => config.brief = Some(text.clone()),
        }
        if let Some(message) = config.validate() {
            anyhow::bail!("{message}");
        }
        let eligible = reasons.is_empty();
        let a = auto(conn, &key)?;
        let changed = !matches!(ce, ConfigEdit::Keep)
            || !matches!(me, MessageEdit::Keep)
            || behavior != Behavior::Keep;
        if eligible {
            if facts.is_some_and(|f| f.state == TicketState::Ready) {
                ready += 1;
            } else {
                blocked += 1;
            }
            if behavior == Behavior::WhenReady && a["state"] == "failed" {
                renew += 1;
            }
            if behavior == Behavior::Manual
                && matches!(a["state"].as_str(), Some("waiting" | "failed"))
            {
                cancel += 1;
            }
        }
        items.push(json!({"repo":key.0,"number":key.1,"eligible":eligible,"reasons":reasons,"state":facts.map(|f|f.state.as_str()),"effective_config":config,"behavior":behavior,"authorization_state":a["state"],"source":source,"has_exception":ex.is_some()}));
        plan.push(PlanItem {
            key,
            config,
            behavior,
            changed,
            eligible,
        });
    }
    Ok((
        json!({"selected_count":items.len(),"eligible_count":ready+blocked,"excluded_count":items.len()-ready-blocked,"items":items,"ready_count":ready,"blocked_count":blocked,"renew_failed_count":renew,"cancel_count":cancel,"auto_start_paused":settings.auto_start_paused}),
        plan,
    ))
}
impl BoardStore {
    /// Capture defaults once, at first discovery anywhere on the board.
    pub fn sync_launch_preferences(
        &self,
        board: &Board,
        settings: &NewAgentSettings,
    ) -> Result<()> {
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for key in board.facts.keys() {
            if tx.execute(
                "INSERT OR IGNORE INTO board_launch_seen(repo,number) VALUES(?1,?2)",
                params![key.0, key.1],
            )? == 0
            {
                continue;
            }
            let tier: Option<String> = tx
                .query_row(
                    "SELECT tier FROM board_items WHERE repo=?1 AND number=?2",
                    params![key.0, key.1],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if tier.is_some_and(|n| {
                settings
                    .agent_types
                    .iter()
                    .any(|t| t.name.eq_ignore_ascii_case(&n))
            }) {
                continue;
            }
            let mut lanes: Vec<_> = board.lanes.iter().filter(|l| l.contains(key)).collect();
            lanes.sort_by_key(|l| (l.lane.rank, l.lane.id));
            for lane in lanes {
                let d = lane_default(&tx, lane.lane.id)?;
                if !d["config"].is_null() {
                    save_pref(
                        &tx,
                        key,
                        &serde_json::from_value(d["config"].clone())?,
                        "lane",
                        Some(lane.lane.id),
                    )?;
                    break;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }
    pub fn launch_preference(&self, key: &Key) -> Result<Value> {
        match self.open_read()? {
            Some(c) => preference(&c, key),
            None => Ok(Value::Null),
        }
    }
    pub fn launch_default(&self, id: i64) -> Result<Value> {
        lane_default(&self.open_write()?, id)
    }
    pub fn set_launch_default(
        &self,
        id: i64,
        revision: i64,
        config: Option<Config>,
    ) -> Result<Reply> {
        if let Some(e) = config.as_ref().and_then(Config::validate) {
            return Ok(error(400, "invalid_config", e));
        }
        let mut c = self.open_write()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM board_lanes WHERE id=?1 AND ended_at IS NULL)",
            [id],
            |r| r.get::<_, bool>(0),
        )? {
            return Ok(error(404, "not_found", "Lane is not active"));
        }
        if lane_default(&tx, id)?["revision"] != revision {
            return Ok(error(
                409,
                "selection_changed",
                "Lane default changed; reopen it",
            ));
        }
        tx.execute("INSERT INTO board_launch_defaults(lane_id,config,revision) VALUES(?1,?2,1) ON CONFLICT(lane_id) DO UPDATE SET config=excluded.config,revision=board_launch_defaults.revision+1",params![id,config.map(|c|serde_json::to_string(&c)).transpose()?])?;
        let value = lane_default(&tx, id)?;
        tx.commit()?;
        Ok((200, value))
    }
    pub fn preview_launch(
        &self,
        outside: &Outside,
        mut req: Preview,
        settings: Value,
        token: &str,
        now: OffsetDateTime,
    ) -> Result<Reply> {
        let mut keys = BTreeSet::new();
        for item in &mut req.selection {
            item.repo = canonical_repo(&item.repo);
            if item.number <= 0 || !item.repo.contains('/') || !keys.insert(item.key()) {
                return Ok(error(
                    400,
                    "invalid_selection",
                    "Select 1–100 distinct repository/ticket pairs",
                ));
            }
        }
        if keys.is_empty() || keys.len() > 100 {
            return Ok(error(
                400,
                "invalid_selection",
                "Select 1–100 distinct tickets",
            ));
        }
        let mut exceptions = BTreeSet::new();
        for e in &req.exceptions {
            let k = (canonical_repo(&e.repo), e.number);
            if !keys.contains(&k) || !exceptions.insert(k) {
                return Ok(error(
                    400,
                    "invalid_exception",
                    "Exceptions must name distinct selected tickets",
                ));
            }
        }
        req.selection.sort_by_key(Selection::key);
        let mut c = self.open_write()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let board = model::compute(&load_input(&tx, outside)?, now);
        let typed: NewAgentSettings = serde_json::from_value(settings.clone())?;
        let (mut response, plan) = match build_plan(&tx, &board, &req, &typed) {
            Ok(v) => v,
            Err(e) => return Ok(error(400, "invalid_config", &e.to_string())),
        };
        let snap = snapshot(&tx, &board, &req, &settings)?;
        let expires = now.unix_timestamp() + 300;
        response["token"] = json!(token);
        response["expires_at"] = json!(format_ts(now + time::Duration::minutes(5)));
        tx.execute(
            "DELETE FROM board_launch_previews WHERE expires<?1",
            [now.unix_timestamp()],
        )?;
        tx.execute(
            "DELETE FROM board_launch_requests WHERE created<?1",
            [now.unix_timestamp() - 7 * 86400],
        )?;
        tx.execute(
            "INSERT INTO board_launch_previews VALUES(?1,?2,?3,?4,?5)",
            params![
                token,
                expires,
                serde_json::to_string(&req)?,
                snap.to_string(),
                serde_json::to_string(&(response.clone(), plan))?
            ],
        )?;
        tx.commit()?;
        Ok((200, response))
    }
    pub fn commit_launch(
        &self,
        outside: &Outside,
        settings: Value,
        request_id: &str,
        token: &str,
        now: OffsetDateTime,
    ) -> Result<Reply> {
        if request_id.len() != 36
            || request_id
                .chars()
                .any(|c| !c.is_ascii_hexdigit() && c != '-')
        {
            return Ok(error(
                400,
                "invalid_request_id",
                "A UUID request_id is required",
            ));
        }
        let mut c = self.open_write()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior: Option<(String, String)> = tx
            .query_row(
                "SELECT token,response FROM board_launch_requests WHERE id=?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((saved, response)) = prior {
            return Ok(if saved == token {
                (200, serde_json::from_str(&response)?)
            } else {
                error(
                    409,
                    "request_id_reused",
                    "Request ID belongs to a different preview",
                )
            });
        }
        let row: Option<(i64, String, String, String)> = tx
            .query_row(
                "SELECT expires,body,snapshot,plan FROM board_launch_previews WHERE token=?1",
                [token],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((expires, body, snap, plan)) = row else {
            return Ok(error(
                409,
                "preview_expired",
                "Preview is unavailable; review this selection again",
            ));
        };
        if expires < now.unix_timestamp() {
            return Ok(error(
                409,
                "preview_expired",
                "Preview expired; review this selection again",
            ));
        }
        let req: Preview = serde_json::from_str(&body)?;
        let board = model::compute(&load_input(&tx, outside)?, now);
        let current = snapshot(&tx, &board, &req, &settings)?;
        let prior: Value = serde_json::from_str(&snap)?;
        if prior != current {
            let changed: Vec<_> = req
                .selection
                .iter()
                .enumerate()
                .filter_map(|(i, t)| {
                    let reasons: Vec<_> = [
                        "facts",
                        "tier",
                        "claims",
                        "goals",
                        "preference",
                        "auto",
                        "attributed_prs",
                    ]
                    .into_iter()
                    .filter(|field| prior["rows"][i][*field] != current["rows"][i][*field])
                    .chain((prior["settings"] != current["settings"]).then_some("settings"))
                    .collect();
                    (!reasons.is_empty())
                        .then(|| json!({"repo":t.repo,"number":t.number,"reasons":reasons}))
                })
                .collect();
            return Ok((
                409,
                json!({"code":"selection_changed","detail":"Selected work or launch settings changed. Nothing was saved; review a fresh preview.","changed":changed}),
            ));
        }
        let (preview, plan): (Value, Vec<PlanItem>) = serde_json::from_str(&plan)?;
        if !plan.iter().any(|p| p.eligible) {
            return Ok(error(
                409,
                "no_eligible_tickets",
                "No selected ticket is eligible",
            ));
        }
        let (mut authorized, mut cancelled, mut prefs) = (0, 0, 0);
        let mut applied = Vec::new();
        let ts = format_ts(now);
        for p in plan.iter().filter(|p| p.eligible && p.changed) {
            let cfg = &p.config;
            let k = &p.key;
            save_pref(&tx, k, cfg, "ticket", None)?;
            prefs += 1;
            match p.behavior {
                Behavior::WhenReady => {
                    tx.execute("INSERT INTO auto_starts(repo,number,provider,model,effort,brief,state,attempts,authorized_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,'waiting',0,?7,?7)
                      ON CONFLICT(repo,number) DO UPDATE SET provider=excluded.provider,model=excluded.model,effort=excluded.effort,brief=excluded.brief,agent_type=NULL,state='waiting',attempts=0,last_error=NULL,session_id=NULL,authorized_at=excluded.authorized_at,updated_at=excluded.updated_at",
                      params![k.0,k.1,cfg.provider,cfg.model,cfg.reasoning_effort,cfg.brief,ts])?;
                    authorized += 1;
                }
                Behavior::Manual => {
                    cancelled+=tx.execute("UPDATE auto_starts SET state='cancelled',last_error='cancelled by owner',updated_at=?3 WHERE repo=?1 AND number=?2 AND state IN ('waiting','failed')",params![k.0,k.1,ts])?;
                }
                Behavior::Keep => {
                    tx.execute("UPDATE auto_starts SET provider=?3,model=?4,effort=?5,brief=?6,agent_type=NULL,updated_at=?7 WHERE repo=?1 AND number=?2 AND state IN ('waiting','failed')",params![k.0,k.1,cfg.provider,cfg.model,cfg.reasoning_effort,cfg.brief,ts])?;
                }
            }
            applied.push(json!({"repo":k.0,"number":k.1}));
        }
        let excluded: Vec<_> = preview["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["eligible"] == false)
            .cloned()
            .collect();
        let response = json!({"request_id":request_id,"applied":applied,"excluded":excluded,"authorized_count":authorized,"cancelled_count":cancelled,"preferences_count":prefs});
        tx.execute(
            "INSERT INTO board_launch_requests VALUES(?1,?2,?3,?4)",
            params![
                request_id,
                token,
                response.to_string(),
                now.unix_timestamp()
            ],
        )?;
        tx.commit()?;
        Ok((200, response))
    }
}
