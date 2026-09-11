//! Native operator dashboard. Network IO runs off the terminal thread.

use super::{encode_path_segment as enc, WatchArgs};
use anyhow::{anyhow, bail, Context, Result};
use nix::{
    libc,
    sys::termios::{self, SetArg, Termios},
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, IsTerminal, Write},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

#[cfg(test)]
mod tests;

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
fn name(v: &Value) -> &str {
    ["friendly_name", "name", "id"]
        .into_iter()
        .map(|k| s(v, k))
        .find(|v| !v.is_empty())
        .unwrap_or("?")
}
fn array(v: &Value, key: &str) -> Vec<Value> {
    v[key].as_array().cloned().unwrap_or_default()
}
fn stamp(v: &str) -> Option<i64> {
    OffsetDateTime::parse(v, &Rfc3339)
        .ok()
        .map(|t| t.unix_timestamp())
}
fn age(v: &str, now: i64) -> String {
    stamp(v)
        .map(|t| duration(now - t))
        .unwrap_or_else(|| "?".into())
}
fn duration(seconds: i64) -> String {
    let n = seconds.max(0);
    if n < 60 {
        format!("{n}s")
    } else if n < 3600 {
        format!("{}m", n / 60)
    } else if n < 86400 {
        format!("{}h {}m", n / 3600, n % 3600 / 60)
    } else {
        format!("{}d {}h", n / 86400, n % 86400 / 3600)
    }
}
fn job_age(job: &Value, now: i64) -> String {
    let state = s(job, "state");
    let start = if state == "pending" {
        s(job, "queued_at")
    } else {
        s(job, "started_at")
    };
    let end = if matches!(state, "pending" | "running") {
        Some(now)
    } else {
        stamp(s(job, "finished_at"))
    };
    match (stamp(start), end) {
        (Some(a), Some(b)) => duration(b - a),
        _ => "?".into(),
    }
}
fn owns(job: &Value, id: &str) -> bool {
    let requester = s(job, "requester_session_id");
    !id.is_empty()
        && if requester.is_empty() {
            s(job, "notify_session_id") == id
        } else {
            requester == id
        }
}
fn owner(job: &Value) -> &str {
    [
        "requester_name",
        "requester_session_id",
        "notify_name",
        "notify_session_id",
    ]
    .into_iter()
    .map(|k| s(job, k))
    .find(|v| !v.is_empty())
    .unwrap_or("unknown")
}
/// Show the scheduler's reason on the job it holds, not on the whole agent.
fn pending_reason(job: &Value) -> String {
    if s(job, "state") != "pending" {
        return String::new();
    }
    if let Some(summary) = job["holding"]["summary"].as_str().filter(|s| !s.is_empty()) {
        return format!(" · {summary}");
    }
    let reason = match s(job, "holding_reason") {
        "awaiting_tests" => "waiting for test jobs".into(),
        "perf_running" => "waiting for the performance run to finish".into(),
        "perf_cooldown" => "waiting for performance cooldown".into(),
        "concurrency_cap" => "waiting for an available job slot".into(),
        "" => "reason not yet reported".into(),
        other => format!("waiting for {}", other.replace('_', " ")),
    };
    format!(" · {reason}")
}

fn running_job_pid(job: &Value) -> Option<i64> {
    if s(job, "state") == "running" {
        job["pid"].as_i64().filter(|pid| *pid > 0)
    } else {
        None
    }
}
fn job_row_context(job: &Value) -> String {
    match running_job_pid(job) {
        Some(pid) => format!(" · PID {pid}"),
        None => pending_reason(job),
    }
}

/// Summarize outstanding work without repeating the selectable job rows.
fn obligation_context(
    obligation: &Value,
    jobs: &[Value],
    id: &str,
    expanded: bool,
    now: i64,
) -> Vec<String> {
    let items = array(obligation, "waiting_on");
    let mut lines = Vec::new();
    if items.len() > 1 {
        let jobs = items
            .iter()
            .filter(|item| s(item, "kind") == "queue_job")
            .count();
        let reviews = items
            .iter()
            .filter(|item| s(item, "kind") == "review")
            .count();
        let other = items.len() - jobs - reviews;
        let counts: Vec<_> = [(jobs, "job"), (reviews, "review"), (other, "result")]
            .into_iter()
            .filter(|(n, _)| *n > 0)
            .map(|(n, kind)| format!("{n} {kind}{}", if n == 1 { "" } else { "s" }))
            .collect();
        lines.push(format!("Waiting for {}", counts.join(" and ")));
    }
    if items.len() == 1 || expanded {
        for item in items {
            let already_visible = s(&item, "kind") == "queue_job"
                && jobs
                    .iter()
                    .any(|job| s(job, "id") == s(&item, "id") && owns(job, id));
            if !already_visible {
                lines.push(format!(
                    "{} · waiting {}",
                    s(&item, "label"),
                    age(s(&item, "since"), now)
                ));
            }
        }
    }
    lines
}

/// Compact metadata shared by inline cards and the full-screen log.
fn job_metadata(job: &Value, now: i64) -> Vec<String> {
    let mut lines = vec![format!(
        "{} · {} · {}",
        s(job, "label"),
        s(job, "state"),
        job_age(job, now)
    )];
    let reason = pending_reason(job);
    if !reason.is_empty() {
        lines.push(format!(
            "Pending reason  {}",
            job["holding"]["detail"]
                .as_str()
                .unwrap_or_else(|| reason.trim_start_matches(" · "))
        ));
    }
    let mut context = vec![format!("Owner  {}", owner(job))];
    if let Some(pid) = running_job_pid(job) {
        context.push(format!("PID  {pid}"));
    }
    if let Some(code) = job["exit_code"].as_i64() {
        context.push(format!("Exit  {code}"));
    }
    if !s(job, "cwd").is_empty() {
        context.push(format!("Directory  {}", s(job, "cwd")));
    }
    lines.push(context.join(" · "));
    let command = array(job, "argv")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    let command = if command.is_empty() {
        s(job, "script_path").to_owned()
    } else {
        command
    };
    if !command.is_empty() {
        lines.push(format!("Command  {command}"));
    }
    lines
}
fn job_card(job: &Value, snap: &Snapshot, prefix: &str, now: i64) -> Vec<Row> {
    let mut rows = Vec::new();
    for text in job_metadata(job, now) {
        rows.push(Row {
            text: format!("{prefix}│ {text}"),
            target: None,
            style: "\x1b[2m",
        });
    }
    rows.push(Row {
        text: format!("{prefix}│ LIVE OUTPUT · Tab to open"),
        target: None,
        style: "\x1b[36m",
    });
    let text = if snap.log_id == s(job, "id") {
        &snap.log
    } else {
        "Loading output…"
    };
    let lines: Vec<_> = text.lines().collect();
    for line in lines.iter().skip(lines.len().saturating_sub(6)) {
        rows.push(Row::plain(format!("{prefix}│ {line}")));
    }
    rows.push(Row::plain(format!("{prefix}╰─")));
    rows
}

/// Strip terminal control sequences, including OSC clipboard/title sequences.
fn clean(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | '_' | '^') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if c == '\n' || !c.is_control() {
            out.push(c);
        } else if c == '\t' {
            out.push_str("    ");
        }
    }
    out
}
fn clipped(text: &str, width: usize) -> String {
    clean(text).replace('\n', " ").chars().take(width).collect()
}

#[derive(Clone)]
struct Client {
    url: String,
    agent: ureq::Agent,
}
impl Client {
    fn new(url: &str) -> Self {
        Self {
            url: url.trim_end_matches('/').into(),
            agent: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(Duration::from_secs(10)))
                .build()
                .into(),
        }
    }
    fn request(&self, method: &str, path: &str, body: Value) -> Result<Value> {
        let url = format!("{}{path}", self.url);
        let bytes = serde_json::to_vec(&body)?;
        let mutation_agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .build()
            .into();
        let agent = if method == "GET" {
            &self.agent
        } else {
            &mutation_agent
        };
        let mut response = match method {
            "GET" => agent.get(&url).call()?,
            "POST" => agent
                .post(&url)
                .header("Content-Type", "application/json")
                .send(bytes.as_slice())?,
            "PATCH" => agent
                .patch(&url)
                .header("Content-Type", "application/json")
                .send(bytes.as_slice())?,
            _ => bail!("unsupported method"),
        };
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string()?;
        let value: Value = serde_json::from_str(&text).context("invalid API response")?;
        if status >= 400 {
            bail!("HTTP {status}: {}", value.get("detail").unwrap_or(&value));
        }
        if let Some(error) = value.get("error").filter(|v| !v.is_null()) {
            bail!("{error}");
        }
        Ok(value)
    }
    fn get(&self, path: &str) -> Result<Value> {
        self.request("GET", path, Value::Null)
    }
}

#[derive(Clone, Default)]
struct Snapshot {
    sessions: Vec<Value>,
    jobs: Vec<Value>,
    requests: Vec<Value>,
    obligations: Vec<Value>,
    details: BTreeMap<String, Vec<String>>,
    errors: BTreeMap<String, String>,
    log_id: String,
    log: String,
}
#[derive(Clone, PartialEq)]
struct Interest {
    details: BTreeSet<String>,
    log: String,
    log_lines: usize,
}
impl Default for Interest {
    fn default() -> Self {
        Self {
            details: BTreeSet::new(),
            log: String::new(),
            log_lines: 200,
        }
    }
}
enum Work {
    Refresh(Interest),
    Action {
        method: &'static str,
        path: String,
        body: Value,
        attach: bool,
    },
}
struct Reply {
    result: Result<Value, String>,
    attach: bool,
}
struct Worker {
    tx: mpsc::Sender<Work>,
    state: Arc<Mutex<Snapshot>>,
    replies: mpsc::Receiver<Reply>,
}
impl Worker {
    fn start(client: Client, args: &WatchArgs) -> Self {
        let (tx, rx) = mpsc::channel();
        let (reply_tx, replies) = mpsc::channel();
        let state = Arc::new(Mutex::new(Snapshot::default()));
        let shared = state.clone();
        let restore = args.restore;
        let node = args.node.clone().unwrap_or_else(|| "primary".into());
        let all_nodes = args.all_nodes;
        let interval = Duration::from_secs_f64(args.interval.max(0.2));
        thread::spawn(move || {
            let mut interest = Interest::default();
            loop {
                refresh(&client, &shared, &interest, restore, &node, all_nodes);
                match rx.recv_timeout(interval) {
                    Ok(Work::Refresh(next)) => interest = next,
                    Ok(Work::Action {
                        method,
                        path,
                        body,
                        attach,
                    }) => {
                        let result = client.request(method, &path, body).map(|mut value| {
                            if attach && method != "GET" && !s(&value, "id").is_empty() {
                                let descriptor = client.get(&format!("/sessions/{}/attach-descriptor", enc(s(&value,"id"))))
                                    .unwrap_or_else(|e|json!({"attach_supported":false,"message":format!("Session created/restored; attach unavailable: {e}")}));
                                value["attach"] = descriptor.get("attach").unwrap_or(&descriptor).clone();
                            }
                            value
                        }).map_err(|e| format!("{e:#}"));
                        let _ = reply_tx.send(Reply { result, attach });
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Self { tx, state, replies }
    }
    fn snapshot(&self) -> Snapshot {
        self.state.lock().unwrap().clone()
    }
}
fn update_list(client: &Client, shared: &Arc<Mutex<Snapshot>>, path: &str, key: &str, field: &str) {
    let result = client.get(path);
    let mut state = shared.lock().unwrap();
    match result {
        Ok(value) => {
            let values = array(&value, key);
            match field {
                "sessions" => state.sessions = values,
                "queue" => state.jobs = values,
                "obligations" => state.obligations = values,
                _ => state.requests = values,
            }
            state.errors.remove(field);
        }
        Err(e) => {
            state.errors.insert(
                field.into(),
                format!("{field} unavailable (last snapshot): {e:#}"),
            );
        }
    }
}
fn refresh(
    client: &Client,
    shared: &Arc<Mutex<Snapshot>>,
    interest: &Interest,
    restore: bool,
    node: &str,
    all_nodes: bool,
) {
    let path = if restore && !all_nodes && node != "primary" {
        super::node_restore_candidates_path(node)
    } else if restore {
        "/sessions?include_stopped=true".into()
    } else {
        "/sessions".into()
    };
    update_list(client, shared, &path, "sessions", "sessions");
    if restore && !all_nodes {
        shared.lock().unwrap().sessions.retain(|v| {
            let n = s(v, "node");
            n == node || (node == "primary" && n.is_empty())
        });
    }
    if !restore {
        update_list(client, shared, "/queue-jobs", "jobs", "queue");
        update_list(client, shared, "/reparent-requests", "requests", "reparent");
        update_list(
            client,
            shared,
            "/session-obligations",
            "sessions",
            "obligations",
        );
    }
    for id in interest.details.iter().filter(|_| !restore) {
        let mut lines = vec!["── Recent activity ──".into()];
        let provider = {
            let state = shared.lock().unwrap();
            state
                .sessions
                .iter()
                .find(|v| s(v, "id") == id)
                .map(|v| s(v, "provider").to_owned())
                .unwrap_or_default()
        };
        let endpoint = if provider == "codex-app" {
            "activity-actions"
        } else {
            "tool-calls"
        };
        match client.get(&format!("/sessions/{}/{endpoint}?limit=10", enc(id))) {
            Ok(v) => {
                for action in array(
                    &v,
                    if provider == "codex-app" {
                        "actions"
                    } else {
                        "tool_calls"
                    },
                ) {
                    lines.push(format!(
                        "  {} {}",
                        if provider == "codex-app" {
                            s(&action, "summary_text")
                        } else {
                            s(&action, "tool_name")
                        },
                        s(&action, "status")
                    ));
                }
            }
            Err(e) => lines.push(format!("Actions unavailable: {e}")),
        }
        lines.push("── Recent output ──".into());
        match client.get(&format!("/sessions/{}/output?lines=10", enc(id))) {
            Ok(v) => lines.extend(clean(s(&v, "output")).lines().map(str::to_owned)),
            Err(e) => lines.push(format!("Output unavailable: {e}")),
        }
        shared.lock().unwrap().details.insert(id.clone(), lines);
    }
    shared
        .lock()
        .unwrap()
        .details
        .retain(|id, _| interest.details.contains(id));
    if !interest.log.is_empty() {
        let id = &interest.log;
        let missing = !shared.lock().unwrap().jobs.iter().any(|j| s(j, "id") == id);
        if missing {
            if let Ok(job) = client.get(&format!("/queue-jobs/{}", enc(id))) {
                shared.lock().unwrap().jobs.push(job);
            }
        }
        let pending = shared
            .lock()
            .unwrap()
            .jobs
            .iter()
            .any(|j| s(j, "id") == id && s(j, "state") == "pending");
        let text = if pending {
            "Waiting to start — no log yet. Following automatically when the job starts.".into()
        } else {
            match client.get(&format!(
                "/queue-jobs/{}/log?lines={}",
                enc(id),
                interest.log_lines
            )) {
                Ok(v) => clean(s(&v, "text")),
                Err(e) => format!("Log unavailable: {e}"),
            }
        };
        let mut state = shared.lock().unwrap();
        state.log_id = id.clone();
        state.log = text;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    Session(String),
    Repo(String),
    Request(String),
    Job(String),
    SessionJob(String, String),
}
#[derive(Clone)]
struct Row {
    text: String,
    target: Option<Target>,
    style: &'static str,
}
impl Row {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            target: None,
            style: "",
        }
    }
    fn selectable(text: String, target: Target) -> Self {
        Self {
            text,
            target: Some(target),
            style: "",
        }
    }
}
fn repo(v: &Value) -> &str {
    let path = s(v, "working_dir");
    if path.is_empty() {
        "unknown"
    } else {
        path
    }
}
fn retired_at(v: &Value) -> &str {
    ["stopped_at", "completed_at", "retired_at", "last_activity"]
        .into_iter()
        .map(|k| s(v, k))
        .find(|t| !t.is_empty())
        .unwrap_or("")
}
fn filtered(sessions: &[Value], args: &WatchArgs, query: &str) -> Vec<Value> {
    let by_id: BTreeMap<_, _> = sessions.iter().map(|v| (s(v, "id"), v)).collect();
    let mut ids: BTreeSet<String> = sessions
        .iter()
        .filter(|v| {
            args.repo.as_ref().is_none_or(|r| {
                repo(v) == r || repo(v).starts_with(&format!("{}/", r.trim_end_matches('/')))
            }) && args
                .role
                .as_ref()
                .is_none_or(|r| s(v, "role").eq_ignore_ascii_case(r))
                && (query.is_empty()
                    || format!(
                        "{} {}",
                        v,
                        by_id
                            .get(s(v, "parent_session_id"))
                            .map(|p| name(p))
                            .unwrap_or("")
                    )
                    .to_lowercase()
                    .contains(&query.to_lowercase()))
        })
        .map(|v| s(v, "id").to_owned())
        .collect();
    if args.repo.is_some() && args.role.is_none() && query.is_empty() {
        let matched = ids.clone();
        for id in &matched {
            let mut parent = by_id
                .get(id.as_str())
                .map(|v| s(v, "parent_session_id"))
                .unwrap_or("");
            let mut seen = BTreeSet::new();
            while let Some(v) = by_id.get(parent) {
                if !seen.insert(parent) {
                    break;
                }
                ids.insert(parent.into());
                parent = s(v, "parent_session_id");
            }
        }
        let mut descendants = matched;
        loop {
            let before = descendants.len();
            for v in sessions {
                if descendants.contains(s(v, "parent_session_id")) {
                    descendants.insert(s(v, "id").into());
                }
            }
            if before == descendants.len() {
                break;
            }
        }
        ids.extend(descendants);
    }
    sessions
        .iter()
        .filter(|v| ids.contains(s(v, "id")) && (!args.restore || s(v, "status") == "stopped"))
        .cloned()
        .collect()
}

struct View {
    selected: Option<Target>,
    expanded: BTreeSet<String>,
    inline_job: Option<String>,
    collapsed: BTreeSet<String>,
    hidden: BTreeSet<String>,
    top_level: bool,
    sort: String,
    query: String,
    offset: usize,
    jobs_for: Option<String>,
    global: bool,
    tail: bool,
    tail_lines: usize,
    log_scroll: usize,
    job_selected: Option<Target>,
    retained: BTreeMap<String, Value>,
    flash: String,
    retire: Option<(String, Instant)>,
    busy: bool,
    free_scroll: bool,
}
impl View {
    fn new(args: &WatchArgs) -> Self {
        Self {
            selected: None,
            expanded: BTreeSet::new(),
            inline_job: None,
            collapsed: BTreeSet::new(),
            hidden: BTreeSet::new(),
            top_level: args.top_level,
            sort: args.sort.clone(),
            query: String::new(),
            offset: 0,
            jobs_for: None,
            global: false,
            tail: false,
            tail_lines: 200,
            log_scroll: 0,
            job_selected: None,
            retained: BTreeMap::new(),
            flash: String::new(),
            retire: None,
            busy: false,
            free_scroll: false,
        }
    }
    fn interest(&self) -> Interest {
        Interest {
            details: if self.jobs_for.is_some() {
                BTreeSet::new()
            } else {
                self.expanded.clone()
            },
            log_lines: if self.tail { self.tail_lines } else { 6 },
            log: if self.tail {
                match &self.job_selected {
                    Some(Target::Job(id)) => id.clone(),
                    _ => String::new(),
                }
            } else {
                self.inline_job.clone().unwrap_or_default()
            },
        }
    }
    fn active_selection(&mut self) -> &mut Option<Target> {
        if self.jobs_for.is_some() {
            &mut self.job_selected
        } else {
            &mut self.selected
        }
    }
    fn rows(&mut self, snap: &Snapshot, args: &WatchArgs, now: i64) -> Vec<Row> {
        if let Some(id) = &self.jobs_for {
            if !snap.errors.contains_key("queue") {
                for (jid, job) in &mut self.retained {
                    if matches!(s(job, "state"), "pending" | "running")
                        && !snap.jobs.iter().any(|j| s(j, "id") == jid)
                    {
                        job["state"] = json!("left active queue");
                    }
                }
            }
            for job in &snap.jobs {
                self.retained.insert(s(job, "id").into(), job.clone());
            }
            let mut jobs: Vec<_> = self
                .retained
                .values()
                .filter(|j| self.global || owns(j, id))
                .collect();
            jobs.sort_by_key(|j| (stamp(s(j, "queued_at")).unwrap_or(0), s(j, "id")));
            return jobs
                .into_iter()
                .flat_map(|j| {
                    let mut row = Row::selectable(
                        format!(
                            "{}  {:10} {:8} {:5}{}  {}  {}",
                            s(j, "label"),
                            s(j, "type"),
                            s(j, "state"),
                            job_age(j, now),
                            job_row_context(j),
                            owner(j),
                            s(j, "id")
                        ),
                        Target::Job(s(j, "id").into()),
                    );
                    if s(j, "state") == "running" {
                        row.style = "\x1b[32m";
                    }
                    let mut rows = vec![row];
                    if !self.tail && self.inline_job.as_deref() == Some(s(j, "id")) {
                        rows.extend(job_card(j, snap, "    ", now));
                    }
                    rows
                })
                .collect();
        }
        let mut sessions = filtered(&snap.sessions, args, &self.query);
        sessions.sort_by(|a, b| match self.sort.as_str() {
            "retired" if args.restore => stamp(retired_at(b))
                .cmp(&stamp(retired_at(a)))
                .then(name(a).cmp(name(b))),
            "last-active" if args.restore => stamp(s(b, "last_activity"))
                .cmp(&stamp(s(a, "last_activity")))
                .then(name(a).cmp(name(b))),
            _ => name(a).to_lowercase().cmp(&name(b).to_lowercase()),
        });
        let mut rows = Vec::new();
        let mut visited = BTreeSet::new();
        let mut recency: BTreeMap<&str, i64> = BTreeMap::new();
        for v in &sessions {
            let mut root = v;
            let mut seen = BTreeSet::new();
            while seen.insert(s(root, "id")) {
                let Some(parent) = sessions
                    .iter()
                    .find(|p| s(p, "id") == s(root, "parent_session_id"))
                else {
                    break;
                };
                root = parent;
            }
            let time = stamp(if self.sort == "last-active" {
                s(v, "last_activity")
            } else {
                retired_at(v)
            })
            .unwrap_or(i64::MIN);
            recency
                .entry(repo(root))
                .and_modify(|n| *n = (*n).max(time))
                .or_insert(time);
        }
        let mut repos: Vec<_> = sessions
            .iter()
            .map(repo)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if args.restore && self.sort != "name" {
            repos.sort_by(|a, b| recency.get(b).cmp(&recency.get(a)).then(a.cmp(b)));
        }
        for root in repos {
            let roots: Vec<_> = sessions
                .iter()
                .filter(|v| {
                    repo(v) == root
                        && !sessions
                            .iter()
                            .any(|p| s(p, "id") == s(v, "parent_session_id"))
                })
                .collect();
            if roots.is_empty() {
                continue;
            }
            rows.push(Row {
                text: root.into(),
                target: if args.restore {
                    Some(Target::Repo(root.into()))
                } else {
                    None
                },
                style: "\x1b[1;36m",
            });
            if !self.hidden.contains(root) {
                for v in roots {
                    self.session_rows(v, &sessions, snap, args, now, 0, &mut visited, &mut rows);
                }
            }
        }
        // Broken parent cycles must not make agents disappear.
        for v in &sessions {
            if !visited.contains(s(v, "id")) && !self.hidden.contains(repo(v)) {
                self.session_rows(v, &sessions, snap, args, now, 0, &mut visited, &mut rows);
            }
        }
        rows
    }
    #[allow(clippy::too_many_arguments)]
    fn session_rows(
        &self,
        v: &Value,
        sessions: &[Value],
        snap: &Snapshot,
        args: &WatchArgs,
        now: i64,
        depth: usize,
        visited: &mut BTreeSet<String>,
        rows: &mut Vec<Row>,
    ) {
        let id = s(v, "id");
        if !visited.insert(id.into()) {
            return;
        }
        let prefix = "  ".repeat(depth.min(20));
        let obligation = snap.obligations.iter().find(|o| s(o, "session_id") == id);
        let waiting = obligation.is_some_and(|o| !array(o, "waiting_on").is_empty());
        let state = if !args.restore && s(v, "activity_state") == "idle" && waiting {
            "waiting"
        } else if args.restore {
            s(v, "status")
        } else {
            s(v, "activity_state")
        };
        rows.push(Row {
            text: format!(
                "{prefix}{} {:20} {:8} {:10} {:5} {:10} {:8} {:8} {}",
                if state == "waiting" { "◷ " } else { "+-" },
                clipped(name(v), 20),
                id,
                state,
                age(s(v, "last_activity"), now),
                s(v, "provider"),
                s(v, "role"),
                s(v, "node"),
                s(v, "status")
            ),
            target: Some(Target::Session(id.into())),
            style: match state {
                "working" | "thinking" => "\x1b[32m",
                "blocked" => "\x1b[33m",
                "waiting" => "\x1b[36m",
                _ => "",
            },
        });
        if args.restore {
            rows.push(Row::plain(format!(
                "{prefix}   retired: {}  restore: {}  parent: {}",
                age(retired_at(v), now),
                if s(v, "provider") == "codex-app" {
                    "headless"
                } else if s(v, "tmux_session").is_empty() {
                    "no-tmux"
                } else {
                    "ready"
                },
                s(v, "parent_session_id")
            )));
        } else {
            let last = if s(v, "last_action_summary").is_empty() {
                s(v, "last_tool_name")
            } else {
                s(v, "last_action_summary")
            };
            if !last.is_empty() {
                rows.push(Row::plain(format!("{prefix}   last: {last}")));
            }
            if !s(v, "agent_status_text").is_empty() {
                rows.push(Row::plain(format!(
                    "{prefix}   status: {} ({})",
                    s(v, "agent_status_text"),
                    age(s(v, "agent_status_at"), now)
                )));
            }
            if !s(v, "agent_task_completed_at").is_empty() {
                rows.push(Row::plain(format!(
                    "{prefix}   task completed ({})",
                    age(s(v, "agent_task_completed_at"), now)
                )));
            }
            for r in &snap.requests {
                if s(r, "subject_session_id") == id
                    && (s(r, "status") == "failed"
                        || (s(r, "status") == "pending" && r["required_human_approval"] == true))
                {
                    rows.push(Row::selectable(
                        format!(
                            "{prefix}   reparent {} -> {}: {} stage={} [A/X decide, R/B repair]",
                            s(r, "id"),
                            s(r, "target_parent_session_id"),
                            s(r, "status"),
                            s(r, "apply_stage")
                        ),
                        Target::Request(s(r, "id").into()),
                    ));
                }
            }
            if let Some(obligation) = obligation {
                for text in
                    obligation_context(obligation, &snap.jobs, id, self.expanded.contains(id), now)
                {
                    rows.push(Row {
                        text: format!("{prefix}   {text}"),
                        target: None,
                        style: "\x1b[36m",
                    });
                }
                if self.expanded.contains(id) {
                    for review in array(obligation, "review_history") {
                        rows.push(Row::plain(format!("{prefix}   Reviews · {} #{} · {} landed via sm ({} from this agent) · {} requests by this agent", s(&review, "repo"), review["pr_number"], review["landed_count"], review["landed_requested_by_agent"].as_u64().unwrap_or(0), review["requested_by_agent"])));
                    }
                }
            }
            if self.expanded.contains(id) {
                rows.push(Row::plain(format!(
                    "{prefix}   Context · {} tokens   Parent · {}   Repository · {}",
                    v["tokens_used"]
                        .as_i64()
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "—".into()),
                    if s(v, "parent_session_id").is_empty() {
                        "Top level"
                    } else {
                        s(v, "parent_session_id")
                    },
                    repo(v)
                )));
                let since = ["last_action_started_at", "last_tool_call", "last_activity"]
                    .into_iter()
                    .map(|k| s(v, k))
                    .find(|v| !v.is_empty())
                    .unwrap_or("");
                rows.push(Row::plain(format!(
                    "{prefix}   thinking duration: {}",
                    if matches!(state, "working" | "thinking") {
                        age(since, now)
                    } else {
                        "-".into()
                    }
                )));
                match snap.details.get(id) {
                    Some(lines) => {
                        for line in lines {
                            rows.push(Row {
                                text: format!("{prefix}   │ {line}"),
                                target: None,
                                style: if line.starts_with("──") {
                                    "\x1b[1;36m"
                                } else {
                                    ""
                                },
                            });
                        }
                    }
                    None => rows.push(Row::plain("   Loading actions/output...")),
                }
            }
            // Jobs are independent navigation targets, even when the agent is
            // collapsed. Keep agent output above these rows so it cannot look
            // like output belonging to the job.
            for j in snap.jobs.iter().filter(|j| owns(j, id)) {
                let mut row = Row::selectable(
                    format!(
                        "{prefix}   +- {} · {} · {}{}  ({})",
                        s(j, "label"),
                        s(j, "state"),
                        job_age(j, now),
                        job_row_context(j),
                        s(j, "id")
                    ),
                    Target::SessionJob(id.into(), s(j, "id").into()),
                );
                if s(j, "state") == "running" {
                    row.style = "\x1b[32m";
                }
                rows.push(row);
                if self.inline_job.as_deref() == Some(s(j, "id")) {
                    rows.extend(job_card(j, snap, &format!("{prefix}      "), now));
                }
            }
        }
        if args.restore
            && ((self.top_level && !self.expanded.contains(id)) || self.collapsed.contains(id))
        {
            let mut stack = vec![id.to_owned()];
            while let Some(parent) = stack.pop() {
                for child in sessions
                    .iter()
                    .filter(|v| s(v, "parent_session_id") == parent)
                {
                    let child = s(child, "id").to_owned();
                    if visited.insert(child.clone()) {
                        stack.push(child);
                    }
                }
            }
            return;
        }
        for child in sessions.iter().filter(|v| s(v, "parent_session_id") == id) {
            if repo(child) != repo(v) {
                rows.push(Row::plain(format!("{prefix}   {}", repo(child))));
            }
            self.session_rows(child, sessions, snap, args, now, depth + 1, visited, rows);
        }
    }
}

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}
struct Terminal {
    saved: Termios,
    signals: Vec<(libc::c_int, libc::sighandler_t)>,
}
impl Terminal {
    fn enter() -> Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            bail!("sm watch requires an interactive terminal");
        }
        let saved = termios::tcgetattr(io::stdin())?;
        let mut t = Self {
            saved,
            signals: Vec::new(),
        };
        STOP.store(false, Ordering::Relaxed);
        for sig in [libc::SIGHUP, libc::SIGTERM, libc::SIGINT] {
            // The handler only stores an atomic flag; cleanup happens on the UI thread.
            let old = unsafe { libc::signal(sig, stop_signal as *const () as libc::sighandler_t) };
            t.signals.push((sig, old));
        }
        t.resume()?;
        Ok(t)
    }
    fn resume(&self) -> Result<()> {
        let mut raw = self.saved.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(io::stdin(), SetArg::TCSANOW, &raw)?;
        print!("\x1b[?1049h\x1b[?25l\x1b[2J");
        io::stdout().flush()?;
        Ok(())
    }
    fn suspend(&self) {
        let _ = termios::tcsetattr(io::stdin(), SetArg::TCSANOW, &self.saved);
        print!("\x1b[0m\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?25h\x1b[?1049l");
        let _ = io::stdout().flush();
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        self.suspend();
        for (sig, old) in &self.signals {
            unsafe {
                libc::signal(*sig, *old);
            }
        }
    }
}
fn size() -> (usize, usize) {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size);
    }
    (
        usize::from(size.ws_row).max(4),
        usize::from(size.ws_col).max(10),
    )
}
#[derive(Debug, PartialEq)]
enum Key {
    Char(char),
    Up,
    Down,
    PageUp,
    PageDown,
    End,
    Enter,
    Tab,
    Esc,
    Backspace,
    None,
}
fn readable(ms: i32) -> bool {
    let mut fd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut fd, 1, ms) > 0 }
}
fn key() -> Result<Key> {
    if !readable(100) {
        return Ok(Key::None);
    }
    let Some(mut byte) = read_byte()? else {
        return Ok(Key::Esc);
    };
    Ok(match byte {
        3 => {
            STOP.store(true, Ordering::Relaxed);
            Key::Esc
        }
        9 => Key::Tab,
        10 | 13 => Key::Enter,
        127 | 8 => Key::Backspace,
        27 => {
            let mut seq = Vec::new();
            while seq.len() < 8 && readable(20) {
                let Some(next) = read_byte()? else {
                    break;
                };
                byte = next;
                seq.push(byte);
                if byte.is_ascii_alphabetic() || byte == b'~' {
                    break;
                }
            }
            match seq.as_slice() {
                b"[A" => Key::Up,
                b"[B" => Key::Down,
                b"[5~" => Key::PageUp,
                b"[6~" => Key::PageDown,
                b"[F" | b"OF" | b"[4~" => Key::End,
                _ => Key::Esc,
            }
        }
        b if b < 128 => Key::Char(b as char),
        b => {
            let n = if b < 224 {
                2
            } else if b < 240 {
                3
            } else {
                4
            };
            let mut bytes = vec![b];
            for _ in 1..n {
                if !readable(20) {
                    break;
                }
                let Some(next) = read_byte()? else {
                    break;
                };
                bytes.push(next);
            }
            Key::Char(
                std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|v| v.chars().next())
                    .unwrap_or('\u{fffd}'),
            )
        }
    })
}
/// Do not use buffered Stdin with poll: it can swallow the rest of a key burst.
fn read_byte() -> Result<Option<u8>> {
    let mut byte = 0u8;
    let n = unsafe { libc::read(libc::STDIN_FILENO, (&mut byte as *mut u8).cast(), 1) };
    if n < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(None);
        }
        return Err(error.into());
    }
    Ok((n == 1).then_some(byte))
}
fn prompt(label: &str) -> Result<Option<String>> {
    let mut text = String::new();
    while !STOP.load(Ordering::Relaxed) {
        let (h, w) = size();

        print!(
            "\x1b[{h};1H\x1b[2K{}\x1b[?25h",
            clipped(&format!("{label}{text}"), w - 1)
        );
        io::stdout().flush()?;
        match key()? {
            Key::Enter => {
                print!("\x1b[?25l");
                return Ok(Some(text));
            }
            Key::Esc => {
                print!("\x1b[?25l");
                return Ok(None);
            }
            Key::Backspace => {
                text.pop();
            }
            Key::Char(c) if !c.is_control() => text.push(c),
            _ => {}
        }
    }
    Ok(None)
}
fn show_help() -> Result<()> {
    let help = [
        "sm watch — keyboard controls",
        "",
        "j/k or arrows: select agent or reparent request",
        "Tab: expand inline; Tab again on a job: full-screen live output",
        "J: jobs; j/k: select job; Tab: full-screen live output; g: global queue",
        "Jobs are selectable below collapsed agents; t/Enter: 200-line tail",
        "In logs: PgUp/PgDn scroll; End follows newest output; q goes back",
        "Enter: attach (restore in --restore mode)",
        "s: send message; n: rename; +: create",
        "K then K within 5s: retire selected session",
        "A/X: approve/reject a human-gated reparent request",
        "R/B: resume/rollback a failed reparent request",
        "Restore mode: E/C expand/collapse all; o sort; R/U hide/show repos",
        "/: filter; r: refresh; q/Esc: quit",
        "",
        "Press any key to return",
    ];
    let (h, w) = size();
    print!("\x1b[H\x1b[2J");
    for line in help.iter().take(h.saturating_sub(1)) {
        print!("{}\r\n", clipped(line, w - 1));
    }
    io::stdout().flush()?;
    while !STOP.load(Ordering::Relaxed) && key()? == Key::None {}
    Ok(())
}
fn navigation(rows: &[Row], selection: &mut Option<Target>, delta: isize) {
    let targets: Vec<_> = rows.iter().filter_map(|r| r.target.clone()).collect();
    if targets.is_empty() {
        *selection = None;
        return;
    }
    let index = selection
        .as_ref()
        .and_then(|t| targets.iter().position(|v| v == t))
        .unwrap_or(0);
    *selection = Some(targets[index.saturating_add_signed(delta).min(targets.len() - 1)].clone());
}
fn refresh_interest(worker: &Worker, view: &View, interest: &mut Interest) -> Result<()> {
    let next = view.interest();
    if next != *interest {
        worker.tx.send(Work::Refresh(next.clone()))?;
        *interest = next;
    }
    Ok(())
}
/// Wrap status/details so queue owner names remain readable on narrow terminals.
fn wrap_rows(rows: Vec<Row>, width: usize) -> Vec<Row> {
    let mut result = Vec::new();
    for row in rows {
        if row.target.is_some() {
            result.push(row);
            continue;
        }
        let text = clean(&row.text);
        let chars: Vec<_> = text.chars().collect();
        if chars.is_empty() {
            result.push(row);
            continue;
        }
        for part in chars.chunks(width.max(1)) {
            result.push(Row {
                text: part.iter().collect(),
                target: None,
                style: row.style,
            });
        }
    }
    result
}
fn frame(
    view: &mut View,
    snap: &Snapshot,
    args: &WatchArgs,
    rows: &[Row],
    h: usize,
    w: usize,
) -> String {
    let width = w - 1;
    let jobs = view.jobs_for.is_some();
    let mut lines = vec![String::new(); h];
    lines[0] = if jobs {
        format!(
            "Queue jobs — {} (enqueue order)",
            if view.global {
                "all agents"
            } else {
                view.jobs_for.as_deref().unwrap_or("")
            }
        )
    } else {
        format!(
            "sm watch{}  {} agents  filter: {}",
            if args.restore { " --restore" } else { "" },
            filtered(&snap.sessions, args, &view.query).len(),
            view.query
        )
    };
    lines[1] = if jobs {
        "Job                         Type       State    Age   Agent / ID".into()
    } else {
        "Session                  ID       Activity   Age   Provider   Role     Node     Status"
            .into()
    };
    let available = h.saturating_sub(4);
    let list_height = if jobs {
        if view.tail {
            1
        } else {
            available
        }
    } else {
        available
    }
    .max(1);
    let selected = if jobs {
        &view.job_selected
    } else {
        &view.selected
    };
    if let Some(i) = rows
        .iter()
        .position(|r| r.target.is_some() && &r.target == selected)
        .filter(|_| !view.free_scroll)
    {
        if i < view.offset {
            view.offset = i;
        } else if i >= view.offset + list_height {
            view.offset = i + 1 - list_height;
        }
    }
    view.offset = view.offset.min(rows.len().saturating_sub(list_height));
    for (index, row) in rows.iter().skip(view.offset).take(list_height).enumerate() {
        let selected = row.target.is_some() && &row.target == selected;
        let text = clipped(
            &format!("{} {}", if selected { ">" } else { " " }, row.text),
            width,
        );
        if index + 2 < h - 2 {
            lines[index + 2] = format!(
                "{}{}{text}\x1b[0m",
                row.style,
                if selected { "\x1b[7m" } else { "" }
            );
        }
    }
    if rows.is_empty() {
        lines[2] = "No matching agents/jobs".into();
    }
    if jobs && view.tail {
        if let Some(Target::Job(id)) = &view.job_selected {
            if let Some(job) = view.retained.get(id) {
                let y = 2 + list_height;
                let metadata = job_metadata(job, OffsetDateTime::now_utc().unix_timestamp());
                for (i, text) in metadata.iter().enumerate() {
                    if y + i < h - 2 {
                        lines[y + i] = clipped(text, width);
                    }
                }
                if view.tail && y + metadata.len() < h - 2 {
                    lines[y + metadata.len()] = format!(
                        "\x1b[1;36mLive output · {} · {}\x1b[0m",
                        s(job, "label"),
                        if view.log_scroll == 0 {
                            "following"
                        } else {
                            "scrolled · End to follow"
                        }
                    );
                    let text = if snap.log_id == *id {
                        snap.log.as_str()
                    } else {
                        "Loading log..."
                    };
                    let mut log: Vec<_> = text.lines().collect();
                    if log.len() > view.tail_lines {
                        log.drain(..log.len() - view.tail_lines);
                    }
                    let count = (h - 2).saturating_sub(y + metadata.len() + 1);
                    view.log_scroll = view.log_scroll.min(log.len().saturating_sub(count));
                    let end = log.len().saturating_sub(view.log_scroll);
                    for (i, line) in log[end.saturating_sub(count)..end].iter().enumerate() {
                        lines[y + metadata.len() + 1 + i] = clipped(line, width);
                    }
                }
            }
        }
    }
    lines[h - 2] = clipped(
        &if view.flash.is_empty() {
            snap.errors
                .values()
                .cloned()
                .collect::<Vec<_>>()
                .join(" | ")
        } else {
            view.flash.clone()
        },
        width,
    );
    lines[h - 1] = clipped(
        if jobs {
            "Tab: live output  t: toggle output  j/k: job  g: all/agent  PgUp/Dn  q: back"
        } else if args.restore {
            "j/k: move Enter: restore Tab: expand E/C: all o: sort R/U: hide/show /: filter q: quit"
        } else {
            "q: quit  j/k: move  J: jobs  Tab: details  Enter: attach  /: filter  ?: help"
        },
        width,
    );
    let mut output = String::from("\x1b[H");
    for (i, line) in lines.iter().enumerate() {
        output.push_str("\x1b[2K");
        if i < 2 {
            output.push_str(&clipped(line, width));
        } else {
            output.push_str(line);
        }
        if i + 1 < h {
            output.push_str("\r\n");
        }
    }
    output
}

fn attach_command(value: &Value) -> Result<Vec<String>> {
    let descriptor = value.get("attach").unwrap_or(value);
    if descriptor["attach_supported"] == false {
        bail!("{}", s(descriptor, "message"));
    }
    let explicit = array(descriptor, "attach_command");
    if !explicit.is_empty() {
        return explicit
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow!("Invalid attach command"))
            })
            .collect();
    }
    let mut command = vec!["tmux".to_owned()];
    if !s(descriptor, "tmux_socket_name").is_empty() {
        command.extend(["-L".into(), s(descriptor, "tmux_socket_name").into()]);
    }
    let target = s(descriptor, "tmux_session");
    if target.is_empty() {
        bail!("No terminal available for this session");
    }
    command.extend(["attach-session".into(), "-t".into(), target.into()]);
    Ok(command)
}
fn attach(terminal: &Terminal, value: &Value) -> Result<()> {
    let argv = attach_command(value)?;
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    terminal.suspend();
    let result = command.status();
    terminal.resume()?;
    if !result.context("tmux attach failed")?.success() {
        bail!("tmux attach failed");
    }
    Ok(())
}
fn send_action(
    worker: &Worker,
    view: &mut View,
    method: &'static str,
    path: String,
    body: Value,
    attach: bool,
) -> Result<()> {
    if view.busy {
        bail!("An operation is already in progress");
    }
    worker.tx.send(Work::Action {
        method,
        path,
        body,
        attach,
    })?;
    view.busy = true;
    view.flash = "Working...".into();
    Ok(())
}

fn configured_node(config: &serde_yaml::Value) -> Option<String> {
    ["local_node", "default_node"].into_iter().find_map(|key| {
        config[key]
            .as_str()
            .or_else(|| config["client"][key].as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    })
}
fn expand_home(path: &str) -> std::path::PathBuf {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    match (path, home) {
        ("~", Some(home)) => home,
        (path, Some(home)) if path.starts_with("~/") => home.join(&path[2..]),
        _ => path.into(),
    }
}
pub(super) fn run(url: &str, mut args: WatchArgs) -> Result<()> {
    if args.restore && !args.all_nodes && args.node.is_none() {
        if let Ok(text) = std::fs::read_to_string(super::client_config_path()) {
            let config: serde_yaml::Value = serde_yaml::from_str(&text)?;
            args.node = configured_node(&config);
        }
    }
    if let Some(repo) = &mut args.repo {
        let path = expand_home(repo);
        *repo = std::fs::canonicalize(&path)
            .unwrap_or_else(|_| {
                if path.is_absolute() {
                    path.clone()
                } else {
                    std::env::current_dir().unwrap_or_default().join(path)
                }
            })
            .to_string_lossy()
            .into();
    }
    let terminal = Terminal::enter()?;
    let worker = Worker::start(Client::new(url), &args);
    let mut view = View::new(&args);
    let mut interest = Interest::default();
    let mut last_frame = String::new();
    while !STOP.load(Ordering::Relaxed) {
        while let Ok(reply) = worker.replies.try_recv() {
            view.busy = false;
            match reply.result {
                Err(error) => view.flash = error,
                Ok(value) => {
                    if reply.attach {
                        view.flash = match attach(&terminal, &value) {
                            Ok(()) => "Detached".into(),
                            Err(e) => e.to_string(),
                        };
                    } else {
                        view.flash = "Done".into();
                        let created = value.get("fork_session").unwrap_or(&value);
                        if !s(created, "id").is_empty() && value.get("requests").is_none() {
                            view.selected = Some(Target::Session(s(created, "id").into()));
                        }
                    }
                }
            }
        }
        let snap = worker.snapshot();
        let rows = view.rows(&snap, &args, OffsetDateTime::now_utc().unix_timestamp());
        let (h, w) = size();
        view.tail_lines = h.saturating_mul(4).clamp(200, 10_000);
        let rows = wrap_rows(rows, w.saturating_sub(3));
        let selection = view.active_selection();
        if !rows
            .iter()
            .any(|r| r.target.is_some() && &r.target == selection)
        {
            navigation(&rows, selection, 0);
        }
        refresh_interest(&worker, &view, &mut interest)?;
        let output = frame(&mut view, &snap, &args, &rows, h, w);
        if output != last_frame {
            print!("{output}");
            io::stdout().flush()?;
            last_frame = output;
        }
        let key = key()?;
        if key == Key::None {
            continue;
        }
        last_frame.clear();
        if key != Key::Char('K') {
            view.retire = None;
        }
        if matches!(key, Key::Char('q') | Key::Esc) {
            if view.jobs_for.take().is_some() {
                view.tail = false;
                view.offset = 0;
                view.retained.clear();
            } else if view.inline_job.take().is_some() {
                // Close the inline card before leaving the dashboard.
            } else {
                break;
            }
        } else if matches!(key, Key::Down | Key::Char('j')) {
            view.free_scroll = false;
            navigation(&rows, view.active_selection(), 1);
            view.log_scroll = 0;
        } else if matches!(key, Key::Up | Key::Char('k')) {
            view.free_scroll = false;
            navigation(&rows, view.active_selection(), -1);
            view.log_scroll = 0;
        } else if view.jobs_for.is_some() {
            match key {
                Key::Tab => {
                    if let Some(Target::Job(id)) = &view.job_selected {
                        if view.inline_job.as_deref() == Some(id) {
                            view.tail = true;
                            view.log_scroll = 0;
                        } else {
                            view.inline_job = Some(id.clone());
                        }
                    }
                }
                Key::Enter | Key::Char('t') => {
                    view.tail = !view.tail;
                    view.log_scroll = 0;
                }
                Key::Char('g') => view.global = !view.global,
                Key::PageUp => view.log_scroll += 10,
                Key::PageDown => view.log_scroll = view.log_scroll.saturating_sub(10),
                Key::End => view.log_scroll = 0,
                _ => {}
            }
        } else {
            let outcome = handle_key(key, &mut view, &worker, &snap, &args);
            if let Err(e) = outcome {
                view.flash = e.to_string();
            }
        }
        refresh_interest(&worker, &view, &mut interest)?;
    }
    Ok(())
}
fn handle_key(
    key: Key,
    view: &mut View,
    worker: &Worker,
    snap: &Snapshot,
    args: &WatchArgs,
) -> Result<()> {
    let target = view.selected.clone();
    let id = match &target {
        Some(Target::Session(id)) => id.as_str(),
        _ => "",
    };
    let session = snap.sessions.iter().find(|v| s(v, "id") == id);
    match key {
        Key::PageDown => {
            view.offset += 10;
            view.free_scroll = true;
        }
        Key::PageUp => {
            view.offset = view.offset.saturating_sub(10);
            view.free_scroll = true;
        }
        Key::Char('/') => {
            if let Some(query) = prompt("filter (blank clears)> ")? {
                view.query = query;
                view.offset = 0;
            }
        }
        Key::Char('r') => {
            view.flash.clear();
            worker.tx.send(Work::Refresh(view.interest()))?;
        }
        Key::Char('?') => {
            show_help()?;
        }
        Key::Char('J') if !id.is_empty() && !args.restore => {
            view.jobs_for = Some(id.into());
            view.offset = 0;
            view.global = false;
            view.job_selected = None;
            view.flash.clear();
        }
        Key::Tab => match target {
            Some(Target::SessionJob(session_id, job_id)) => {
                if view.inline_job.as_deref() != Some(&job_id) {
                    view.inline_job = Some(job_id);
                    return Ok(());
                }
                view.jobs_for = Some(session_id);
                view.job_selected = Some(Target::Job(job_id));
                view.tail = true;
                view.tail_lines = 200;
                view.log_scroll = 0;
                view.offset = 0;
                view.free_scroll = false;
                view.global = false;
                view.flash.clear();
            }
            Some(Target::Repo(repo)) => {
                view.hidden.remove(&repo);
            }
            Some(Target::Session(id)) => {
                if args.restore && !view.top_level {
                    if !view.collapsed.remove(&id) {
                        view.collapsed.insert(id);
                    }
                } else if !view.expanded.remove(&id) {
                    view.expanded.insert(id);
                }
            }
            _ => {}
        },
        Key::Char('C') if args.restore => {
            view.top_level = true;
            view.expanded.clear();
            view.collapsed.clear();
        }
        Key::Char('E') if args.restore => {
            view.top_level = false;
            view.expanded.clear();
            view.collapsed.clear();
        }
        Key::Char('o') if args.restore => {
            view.sort = match view.sort.as_str() {
                "retired" => "last-active",
                "last-active" => "name",
                _ => "retired",
            }
            .into();
        }
        Key::Char('R') if args.restore => {
            let root = match &target {
                Some(Target::Repo(r)) => r.clone(),
                _ => session.map(repo).unwrap_or("").into(),
            };
            view.hidden.insert(root);
        }
        Key::Char('U') if args.restore => view.hidden.clear(),
        Key::Enter => {
            if let Some(Target::Repo(root)) = target {
                view.hidden.remove(&root);
                return Ok(());
            }
            if id.is_empty() {
                return Ok(());
            }
            if args.restore {
                let node = if args.all_nodes {
                    session.map(|v| s(v, "node"))
                } else {
                    args.node.as_deref()
                };
                send_action(
                    worker,
                    view,
                    "POST",
                    super::restore_session_path(id, node),
                    json!({}),
                    true,
                )?;
            } else {
                send_action(
                    worker,
                    view,
                    "GET",
                    format!("/sessions/{}/attach-descriptor", enc(id)),
                    Value::Null,
                    true,
                )?;
            }
        }
        Key::Char('s' | '+' | 'K' | 'n' | 'A' | 'X' | 'B') if args.restore => {
            view.flash = "Not available in restore mode".into()
        }
        Key::Char('s') if !id.is_empty() => {
            if let Some(text) = prompt("send> ")?.filter(|v| !v.is_empty()) {
                send_action(
                    worker,
                    view,
                    "POST",
                    format!("/sessions/{}/input", enc(id)),
                    json!({"text":text,"delivery_mode":"sequential","from_sm_send":true}),
                    false,
                )?;
            }
        }
        Key::Char('n') if !id.is_empty() => {
            if let Some(text) = prompt("name> ")?.filter(|v| !v.is_empty()) {
                send_action(
                    worker,
                    view,
                    "PATCH",
                    format!("/sessions/{}", enc(id)),
                    json!({"friendly_name":text}),
                    false,
                )?;
            }
        }
        Key::Char('K') if !id.is_empty() => {
            if view
                .retire
                .as_ref()
                .is_some_and(|(armed, at)| armed == id && at.elapsed() < Duration::from_secs(5))
            {
                view.retire = None;
                send_action(
                    worker,
                    view,
                    "POST",
                    format!("/sessions/{}/retire", enc(id)),
                    json!({}),
                    false,
                )?;
            } else {
                view.retire = Some((id.into(), Instant::now()));
                view.flash = format!("Press K again within 5s to retire {id}");
            }
        }
        Key::Char('+') => {
            let Some(provider) = prompt("provider [codex/claude] (blank=codex, Esc=cancel)> ")?
            else {
                return Ok(());
            };
            let provider = match provider.trim() {
                "" | "codex" => "codex-fork",
                "claude" => "claude",
                _ => bail!("Use codex or claude"),
            };
            let default = session.map(repo).or(args.repo.as_deref()).unwrap_or(".");
            let Some(path) = prompt(&format!("working dir (blank={default})> "))? else {
                return Ok(());
            };
            let path = if path.is_empty() {
                expand_home(default)
            } else {
                expand_home(&path)
            };
            let path = std::fs::canonicalize(path).context("Working directory does not exist")?;
            if !path.is_dir() {
                bail!("Working directory is not a directory");
            }
            send_action(
                worker,
                view,
                "POST",
                "/sessions".into(),
                super::create_launch_session_payload(provider, &path.to_string_lossy(), None, None),
                true,
            )?;
        }
        Key::Char(action @ ('A' | 'X' | 'R' | 'B')) => {
            let Some(Target::Request(rid)) = target else {
                bail!("Select a reparent request");
            };
            let r = snap
                .requests
                .iter()
                .find(|r| s(r, "id") == rid)
                .ok_or_else(|| anyhow!("Request no longer available"))?;
            let base = format!("/reparent-requests/{}", enc(&rid));
            if matches!(action, 'A' | 'X') {
                if s(r, "status") != "pending" || r["required_human_approval"] != true {
                    bail!("Request does not need a human decision");
                }
                send_action(
                    worker,
                    view,
                    "POST",
                    format!(
                        "{base}/human-{}",
                        if action == 'A' { "approve" } else { "reject" }
                    ),
                    json!({}),
                    false,
                )?;
            } else {
                if s(r, "status") != "failed" {
                    bail!("Select a failed request");
                }
                if action == 'B'
                    && !matches!(
                        s(r, "apply_stage"),
                        "json_routing_quiesced" | "routing_quiesced"
                    )
                {
                    bail!("Rollback is unsafe at this stage; resume only");
                }
                send_action(
                    worker,
                    view,
                    "POST",
                    format!("{base}/repair"),
                    json!({"action":if action=='R'{"resume"}else{"rollback_precommit"}}),
                    false,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}
