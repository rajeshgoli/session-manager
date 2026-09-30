//! Local review policy, provider output and GitHub review rendering (#1777).
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};
use std::path::{Component, Path};

pub const RUBRIC: &str = include_str!("rubric.md");
pub const SCHEMA: &str = include_str!("schema.json");

pub fn validate_reviewer(v: &Value) -> Result<(), String> {
    let kind = v["kind"].as_str().unwrap_or("");
    if kind == "github_codex" && v == &json!({"kind":"github_codex"}) {
        return Ok(());
    }
    let models: &[&str] = match kind {
        "codex" => &[
            "gpt-6-astra",
            "gpt-6-sol",
            "gpt-6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
        ],
        "claude" => &["fable", "opus", "opus[1m]", "sonnet", "haiku"],
        _ => return Err("reviews.reviewer.kind must be github_codex, codex or claude".into()),
    };
    if !v["model"].as_str().is_some_and(|m| models.contains(&m)) {
        return Err("reviews.reviewer.model is not supported for this provider".into());
    }
    let efforts: &[&str] = if kind == "codex" {
        &["medium", "high", "xhigh"]
    } else {
        &["low", "medium", "high", "xhigh", "max"]
    };
    if !v["effort"].as_str().is_some_and(|e| efforts.contains(&e)) {
        return Err("reviews.reviewer.effort is not supported for this provider".into());
    }
    if v.as_object().is_none_or(|o| o.len() != 3) {
        return Err("reviews.reviewer has unknown fields".into());
    }
    Ok(())
}

pub fn chain(chosen: &Value) -> Vec<Value> {
    let run = |kind, model, effort| json!({"kind":kind,"model":model,"effort":effort});
    let mut result = vec![chosen.clone()];
    match chosen["kind"].as_str() {
        Some("github_codex") => {
            result.push(run("codex", "gpt-6-sol", "medium"));
            result.push(run("claude", "opus", "high"));
        }
        Some("codex") => result.push(match chosen["model"].as_str() {
            Some("gpt-6-astra") => run("claude", "fable", "xhigh"),
            Some("gpt-6-luna" | "gpt-5.6-luna") => run("claude", "sonnet", "high"),
            _ => run("claude", "opus", "high"),
        }),
        Some("claude") => result.push(match chosen["model"].as_str() {
            Some("fable") => run("codex", "gpt-6-astra", "high"),
            Some("sonnet" | "haiku") => run("codex", "gpt-6-luna", "high"),
            _ => run("codex", "gpt-6-sol", "medium"),
        }),
        _ => {}
    }
    result
}

pub fn label(step: &Value) -> String {
    match step["kind"].as_str() {
        Some("github_codex") => "GitHub Codex".into(),
        kind => format!(
            "{} run ({}, {})",
            if kind == Some("codex") {
                "Codex"
            } else {
                "Claude"
            },
            step["model"].as_str().unwrap_or("?"),
            step["effort"].as_str().unwrap_or("?")
        ),
    }
}

pub fn meter(db: &Path, provider: &str) -> Result<Option<f64>> {
    if !db.exists() {
        return Ok(None);
    }
    let c = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // Choose the account from its latest observation across all windows,
    // then that account's weekly sample. A different account's older weekly
    // sample must not make us skip the account in use now.
    let window = if provider == "codex" {
        "codex_10080"
    } else {
        "weekly_all"
    };
    let value=c.query_row("SELECT percent FROM burn_samples WHERE window_kind=?1 AND account_key=(SELECT account_key FROM accounts WHERE provider=?2 ORDER BY last_seen DESC,account_key LIMIT 1) ORDER BY observed_at DESC,id DESC LIMIT 1",rusqlite::params![window,provider],|r| r.get(0)).optional();
    match value {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::SqliteFailure(_, Some(e))) if e.contains("no such table") => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn should_skip(percent: Option<f64>, limit: i64) -> bool {
    limit < 100 && percent.is_some_and(|p| p >= limit as f64)
}

pub fn valid_output(v: &Value) -> bool {
    v["findings"].is_array()
        && v["overall_correctness"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty())
}

/// The actual review event is in the rollout, not the exec assistant message.
pub fn rollout_output(text: &str) -> Option<Value> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|v| {
            let payload = &v["payload"];
            if payload["type"] == "exited_review_mode" {
                Some(payload["review_output"].clone())
            } else if payload["type"] == "item_completed"
                && payload["item"]["type"] == "ExitedReviewMode"
            {
                Some(payload["item"]["review_output"].clone())
            } else {
                None
            }
        })
        .next_back()
        .filter(valid_output)
}

pub fn codex_output(events: &str, sessions: &Path) -> Result<Value> {
    let thread = events
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "thread.started")
        .and_then(|v| v["thread_id"].as_str().map(str::to_owned))
        .context("no thread.started event")?;
    anyhow::ensure!(
        !thread.is_empty()
            && thread
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "invalid review thread id"
    );
    fn find(dir: &Path, suffix: &str) -> Result<Option<String>> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Some(v) = find(&entry.path(), suffix)? {
                    return Ok(Some(v));
                }
            } else if entry.file_name().to_string_lossy().ends_with(suffix) {
                return Ok(Some(std::fs::read_to_string(entry.path())?));
            }
        }
        Ok(None)
    }
    let text = find(sessions, &format!("{thread}.jsonl"))?.context("review rollout missing")?;
    rollout_output(&text).context("no valid exited_review_mode output")
}

pub fn claude_output(text: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(text)?;
    anyhow::ensure!(v["is_error"] != true, "Claude reported an error");
    let output = v.get("structured_output").context("no structured output")?;
    anyhow::ensure!(valid_output(output), "invalid review output");
    Ok(output.clone())
}

pub fn counts(output: &Value) -> Value {
    let mut c =
        json!({"p0":0,"p1":0,"p2":0,"p3":0,"unrated":0,"verdict":output["overall_correctness"]});
    for f in output["findings"].as_array().into_iter().flatten() {
        let key = match f["priority"].as_i64() {
            Some(0) => "p0",
            Some(1) => "p1",
            Some(2) => "p2",
            Some(3) => "p3",
            _ => "unrated",
        };
        c[key] = json!(c[key].as_i64().unwrap_or(0) + 1);
    }
    c
}

fn relative_path<'a>(checkout: &Path, path: &'a str) -> Option<&'a Path> {
    let p = Path::new(path);
    if !p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    let rel = p.strip_prefix(checkout).ok()?;
    // Check symlinks as well as lexical containment, when the file exists.
    if let Ok(real) = p.canonicalize() {
        if !real.starts_with(checkout.canonicalize().ok()?) {
            return None;
        }
    }
    Some(rel)
}

/// New-side hunk ranges include context, as GitHub line comments do.
pub fn diff_ranges(diff: &str) -> Vec<(i64, i64)> {
    diff.lines()
        .filter(|l| l.starts_with("@@ "))
        .filter_map(|line| {
            let range = line.split_whitespace().nth(2)?.strip_prefix('+')?;
            let mut parts = range.split(',');
            let start = parts.next()?.parse::<i64>().ok()?;
            let count = parts.next().unwrap_or("1").parse::<i64>().ok()?;
            (count > 0).then_some((start, start + count - 1))
        })
        .collect()
}

pub fn payload(
    request: &crate::queue::CodexReviewRequestRegistration,
    checkout: &Path,
    output: &Value,
    diffs: &std::collections::BTreeMap<String, String>,
    inline: bool,
) -> Value {
    let c = counts(output);
    let summary = ["p0", "p1", "p2", "p3", "unrated"]
        .iter()
        .filter_map(|key| {
            let n = c[*key].as_i64().unwrap_or(0);
            (n > 0).then(|| {
                format!(
                    "{n} {}",
                    if *key == "unrated" {
                        "unrated".to_owned()
                    } else {
                        key.to_uppercase()
                    }
                )
            })
        })
        .collect::<Vec<_>>()
        .join(" · ");
    let sha = request.requested_head_sha.as_deref().unwrap_or("");
    let steps: Vec<Value> =
        serde_json::from_str(request.chain_json.as_deref().unwrap_or("[]")).unwrap_or_default();
    let fallback = if request.step_index > 0 {
        steps
            .first()
            .map(|s| format!(", fallback from {}", label(s)))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let mut body=format!("<!-- sm-review-request:{} -->\n**sm review** · {} · round {} · {}{}\n\n**Reviewed commit:** `{}` · **Findings:** {} · **Verdict:** {}\n\n{}",request.id,request.reviewer_label.as_deref().unwrap_or("Review run"),request.round,request.policy_source.as_deref().unwrap_or("default"),fallback,&sha[..sha.len().min(10)],if summary.is_empty() {"0 findings"} else {&summary},output["overall_correctness"].as_str().unwrap_or(""),output["overall_explanation"].as_str().unwrap_or(""));
    let mut comments = Vec::new();
    let mut unplaced = Vec::new();
    for f in output["findings"].as_array().into_iter().flatten() {
        let priority = f["priority"].as_i64().filter(|p| (0..=3).contains(p));
        let mut title = f["title"].as_str().unwrap_or("Finding");
        if title.starts_with("[P") {
            if let Some((_, rest)) = title.split_once(']') {
                title = rest.trim_start();
            }
        }
        let tag = priority.map(|p| format!("[P{p}] ")).unwrap_or_default();
        let content = format!("**{tag}{title}**\n\n{}", f["body"].as_str().unwrap_or(""));
        let path = f["code_location"]["absolute_file_path"]
            .as_str()
            .unwrap_or("");
        let start = f["code_location"]["line_range"]["start"]
            .as_i64()
            .unwrap_or(0);
        let end = f["code_location"]["line_range"]["end"]
            .as_i64()
            .unwrap_or(0);
        let relative = relative_path(checkout, path).and_then(Path::to_str);
        let valid_priority = f["priority"].is_null() || priority.is_some();
        let on_hunk = relative
            .and_then(|p| diffs.get(p))
            .is_some_and(|d| diff_ranges(d).iter().any(|(a, b)| start >= *a && end <= *b));
        if inline && valid_priority && start >= 1 && start <= end && on_hunk {
            let mut comment = json!({"path":relative,"side":"RIGHT","line":end,"body":content});
            if start < end {
                comment["start_line"] = json!(start);
                comment["start_side"] = json!("RIGHT");
            }
            comments.push(comment);
        } else {
            unplaced.push(format!(
                "#### {tag}{title}\n`{path}:{start}-{end}`\n\n{}",
                f["body"].as_str().unwrap_or("")
            ));
        }
    }
    if !unplaced.is_empty() {
        body.push_str("\n\n### Findings not on a changed line\n\n");
        body.push_str(&unplaced.join("\n\n"));
    }
    json!({"commit_id":sha,"event":"COMMENT","body":body,"comments":comments})
}

/// GitHub rejects some valid diff anchors; preserve every finding in one body.
/// Other failures are retried by the durable watcher after checking its marker.
pub fn post_with_body_fallback(
    inline: &Value,
    body: &Value,
    mut post: impl FnMut(&Value) -> Result<Value>,
) -> Result<Value> {
    match post(inline) {
        Err(e) if e.to_string().contains("HTTP 422") => post(body),
        result => result,
    }
}

/// Queue jobs start from an empty environment. Carry only tool lookup,
/// identity, locale and provider config roots; never the author's session,
/// nesting guard, GitHub token or unrelated service credentials.
pub fn environment(
    vars: impl IntoIterator<Item = (String, String)>,
) -> std::collections::BTreeMap<String, String> {
    const KEYS: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TMPDIR",
        "TERM",
        "LANG",
        "CODEX_HOME",
        "CLAUDE_CONFIG_DIR",
    ];
    vars.into_iter()
        .filter(|(k, v)| !v.is_empty() && (KEYS.contains(&k.as_str()) || k.starts_with("LC_")))
        .collect()
}

pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}
pub fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{CreateCodexReviewRequest, RetainedQueueStore};
    fn request() -> (
        scratch::Scratch,
        crate::queue::CodexReviewRequestRegistration,
    ) {
        let dir = scratch::Scratch::new();
        let r = RetainedQueueStore::create_codex_review_request_in_path(
            &dir.0.join("messages.db"),
            CreateCodexReviewRequest {
                repo: "example/repo".into(),
                pr_number: 7,
                requester_session_id: Some("author".into()),
                notify_session_id: "author".into(),
                steer: None,
                requested_head_sha: "0123456789012345678901234567890123456789".into(),
                latest_request_comment_id: None,
                latest_request_comment_url: None,
                latest_request_posted_at: "2026-09-30T12:00:00Z".into(),
                poll_interval_seconds: 30,
                retry_interval_seconds: 120,
            },
        )
        .unwrap();
        (dir, r)
    }
    mod scratch {
        pub struct Scratch(pub std::path::PathBuf);
        impl Scratch {
            pub fn new() -> Self {
                let p = std::env::temp_dir().join(format!(
                    "sm-review-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
                std::fs::create_dir_all(&p).unwrap();
                Self(p)
            }
        }
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
    #[test]
    fn provider_environment_keeps_auth_roots_without_agent_or_github_credentials() {
        let env = environment(
            [
                ("PATH", "/tools/bin"),
                ("HOME", "/home/owner"),
                ("CODEX_HOME", "/home/owner/.codex"),
                ("GH_TOKEN", "secret"),
                ("CLAUDECODE", "nested"),
                ("CLAUDE_SESSION_MANAGER_ID", "author"),
            ]
            .map(|(k, v)| (k.into(), v.into())),
        );
        assert_eq!(env.len(), 3);
        assert_eq!(env["PATH"], "/tools/bin");
        assert_eq!(env["HOME"], "/home/owner");
    }

    #[test]
    fn fallback_preserves_tier_and_never_loops() {
        for (kind, model, want) in [
            ("codex", "gpt-6-astra", "fable"),
            ("codex", "gpt-6-luna", "sonnet"),
            ("codex", "new-model", "opus"),
            ("claude", "fable", "gpt-6-astra"),
            ("claude", "haiku", "gpt-6-luna"),
            ("claude", "opus[1m]", "gpt-6-sol"),
        ] {
            let c = chain(&json!({"kind":kind,"model":model,"effort":"high"}));
            assert_eq!(c.len(), 2);
            assert_eq!(c[1]["model"], want);
        }
        assert_eq!(chain(&json!({"kind":"github_codex"})).len(), 3);
        assert!(!should_skip(Some(100.0), 100));
        assert!(!should_skip(None, 95));
        assert!(should_skip(Some(95.0), 95));
    }
    #[test]
    fn newest_account_does_not_inherit_previous_accounts_meter() {
        let dir = scratch::Scratch::new();
        let db = dir.0.join("usage.db");
        let c = Connection::open(&db).unwrap();
        c.execute_batch("CREATE TABLE accounts(account_key TEXT,provider TEXT,last_seen TEXT); INSERT INTO accounts VALUES('codex:old','codex','2026-09-29'),('codex:new','codex','2026-09-30'); CREATE TABLE burn_samples(id INTEGER PRIMARY KEY,account_key TEXT,window_kind TEXT,percent REAL,observed_at TEXT);INSERT INTO burn_samples VALUES(1,'codex:old','codex_10080',99,'2026-09-29'),(2,'codex:new','codex_300',20,'2026-09-30');").unwrap();
        assert_eq!(meter(&db, "codex").unwrap(), None);
        c.execute(
            "INSERT INTO burn_samples VALUES(3,'codex:new','codex_10080',42,'2026-09-30')",
            [],
        )
        .unwrap();
        assert_eq!(meter(&db, "codex").unwrap(), Some(42.0));
    }
    #[test]
    fn fallback_transaction_only_wakes_after_exhaustion() {
        let (dir, r) = request();
        let db = dir.0.join("messages.db");
        RetainedQueueStore::initialize_review_chain(
            &db,
            &r.id,
            &chain(&json!({"kind":"github_codex"})),
        )
        .unwrap();
        for index in 0..3 {
            let finished = RetainedQueueStore::finish_github_review_step_in_path(
                &db,
                &r.id,
                "failed: no review output",
                "2026-09-30T12:01:00Z",
                "no reviewer",
            )
            .unwrap();
            assert_eq!(finished.is_some(), index == 2);
            let row = RetainedQueueStore::get_codex_review_request_from_path(&db, &r.id)
                .unwrap()
                .unwrap();
            let log: Vec<Value> =
                serde_json::from_str(row.steps_log_json.as_deref().unwrap()).unwrap();
            assert_eq!(log.len(), index + 1);
            assert_eq!(row.is_active, index != 2);
        }
        let c = Connection::open(db).unwrap();
        assert_eq!(
            c.query_row(
                "SELECT count(*) FROM message_queue WHERE text='no reviewer'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }
    #[test]
    fn findings_outside_checkout_or_hunks_stay_in_body() {
        let (_dir, r) = request();
        let finding = |path, priority, start, end| json!({"title":"[P0] Fix the bound","body":"Missing bound can panic.","priority":priority,"code_location":{"absolute_file_path":path,"line_range":{"start":start,"end":end}}});
        let output = json!({"findings":[finding("/checkout/src.rs",json!(1),12,13),finding("/checkout/../secret",json!(2),12,12),finding("/elsewhere/src.rs",json!(0),12,12),finding("/checkout/src.rs",json!(8),12,12),finding("/checkout/src.rs",Value::Null,0,4),finding("/checkout/src.rs",json!(3),30,31)],"overall_correctness":"patch is incorrect","overall_explanation":"Bounds missing"});
        let diffs = std::collections::BTreeMap::from([(
            "src.rs".into(),
            "@@ -10,5 +10,5 @@\n context\n-old\n+new\n".into(),
        )]);
        let p = payload(&r, Path::new("/checkout"), &output, &diffs, true);
        assert_eq!(p["event"], "COMMENT");
        assert_eq!(p["comments"].as_array().unwrap().len(), 1);
        assert_eq!(p["comments"][0]["line"], 13);
        assert_eq!(p["comments"][0]["start_line"], 12);
        assert!(p["comments"][0]["body"]
            .as_str()
            .unwrap()
            .starts_with("**[P1]"));
        assert!(p["body"].as_str().unwrap().contains("/elsewhere/src.rs"));
        let all_body = payload(&r, Path::new("/checkout"), &output, &diffs, false);
        assert!(all_body["comments"].as_array().unwrap().is_empty());
        assert!(all_body["body"]
            .as_str()
            .unwrap()
            .contains("/checkout/src.rs:12-13"));
    }
    #[test]
    fn github_422_retries_with_every_finding_in_the_body() {
        let inline = json!({"comments":[{"body":"finding"}],"body":"summary"});
        let body = json!({"comments":[],"body":"summary and finding"});
        let mut seen = Vec::new();
        let posted = post_with_body_fallback(&inline, &body, |value| {
            seen.push(value.clone());
            if seen.len() == 1 {
                anyhow::bail!("gh: Validation Failed (HTTP 422)")
            }
            Ok(json!({"id":99}))
        })
        .unwrap();
        assert_eq!(posted["id"], 99);
        assert_eq!(seen, vec![inline.clone(), body.clone()]);
        let mut attempts = 0;
        assert!(post_with_body_fallback(&inline, &body, |_| {
            attempts += 1;
            anyhow::bail!("HTTP 503")
        })
        .is_err());
        assert_eq!(attempts, 1);
    }

    #[test]
    fn recorded_native_provider_outputs_produce_inline_reviews() {
        let codex = rollout_output(include_str!("fixtures/codex-rollout.jsonl")).unwrap();
        let claude = claude_output(include_str!("fixtures/claude-result.json")).unwrap();
        let (_dir, r) = request();
        let diffs = std::collections::BTreeMap::from([(
            "review_smoke_1777.py".into(),
            "@@ -0,0 +1,5 @@\n".into(),
        )]);
        for output in [codex, claude] {
            let p = payload(&r, Path::new("/fixture/checkout"), &output, &diffs, true);
            assert_eq!(p["comments"].as_array().unwrap().len(), 1);
            assert_eq!(p["comments"][0]["path"], "review_smoke_1777.py");
            assert_eq!(p["comments"][0]["line"], 5);
            assert!(p["body"].as_str().unwrap().contains("patch is incorrect"));
        }
    }

    #[test]
    fn output_requires_completed_review_not_final_assistant_prose() {
        let v = json!({"findings":[],"overall_correctness":"patch is correct"});
        let events = format!(
            "{}\n{}\n{}\n",
            json!({"type":"event_msg","payload":{"type":"exited_review_mode","review_output":v}}),
            json!({"type":"event_msg","payload":{"type":"agent_message","message":"all done"}}),
            "partial-json"
        );
        assert_eq!(rollout_output(&events), Some(v.clone()));
        assert_eq!(
            claude_output(&json!({"structured_output":v,"is_error":false}).to_string()).unwrap(),
            v
        );
        assert!(
            claude_output(&json!({"structured_output":v,"is_error":true}).to_string()).is_err()
        );
        assert!(!valid_output(
            &json!({"findings":[],"overall_correctness":" "})
        ));
    }
}
