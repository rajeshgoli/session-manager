//! Review-run lifecycle. The request watcher owns transitions; queue terminal
//! hooks wake it and persisted jobs make the same path safe after a restart.
use super::*;
use crate::review::{self, cut, quote};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path as FsPath, PathBuf},
    process::Command,
};

pub(super) fn current_step(r: &CodexReviewRequestRegistration) -> Result<Value, String> {
    let chain: Vec<Value> =
        serde_json::from_str(r.chain_json.as_deref().unwrap_or("[]")).map_err(|e| e.to_string())?;
    chain
        .get(r.step_index as usize)
        .cloned()
        .ok_or_else(|| "invalid review step".into())
}

pub(super) fn failure_lines(r: &CodexReviewRequestRegistration) -> String {
    let log: Vec<Value> =
        serde_json::from_str(r.steps_log_json.as_deref().unwrap_or("[]")).unwrap_or_default();
    log.iter()
        .map(|s| {
            format!(
                "{}: {}",
                s["label"].as_str().unwrap_or("Reviewer"),
                s["reason"].as_str().unwrap_or("failed")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// I2: a review job's `review` field — which request it serves and why this
/// reviewer. `why` is `default` for the chosen reviewer, otherwise the step
/// before it and the reason it failed.
pub(super) fn job_fields(state: &AppState, job: &QueueJobRecord) -> Result<Option<Value>> {
    if job.job_type != "review" {
        return Ok(None);
    }
    let queue_db = queue_dir(state).join("queue_runner.db");
    let Some(owner) = RetainedQueueStore::queue_job_owner_from_path(&queue_db, &job.id)? else {
        return Ok(None);
    };
    let Some((id, index)) = owner
        .strip_prefix("review_request:")
        .and_then(|rest| rest.rsplit_once(':'))
        .and_then(|(id, index)| Some((id, index.parse::<usize>().ok()?)))
    else {
        return Ok(None);
    };
    let db = expand_home(&state.config.sm_send.db_path);
    let Some(r) = RetainedQueueStore::get_codex_review_request_from_path(&db, id)? else {
        return Ok(None);
    };
    let chain: Vec<Value> =
        serde_json::from_str(r.chain_json.as_deref().unwrap_or("[]")).unwrap_or_default();
    let log: Vec<Value> =
        serde_json::from_str(r.steps_log_json.as_deref().unwrap_or("[]")).unwrap_or_default();
    let why = match index.checked_sub(1) {
        None => "default".to_owned(),
        Some(previous) => log
            .iter()
            .rev()
            .find(|s| s["index"].as_u64() == Some(previous as u64))
            .map_or_else(
                || "fallback".to_owned(),
                |s| {
                    format!(
                        "{}: {}, so its fallback",
                        s["label"].as_str().unwrap_or("Reviewer"),
                        s["reason"].as_str().unwrap_or("failed")
                    )
                },
            ),
    };
    let author = r
        .requester_session_id
        .clone()
        .unwrap_or_else(|| r.notify_session_id.clone());
    let author_name = state
        .session_store
        .get_session(&author)?
        .map(session_display_name)
        .unwrap_or(author);
    Ok(Some(json!({
        "request_id": r.id, "repo": r.repo, "pr_number": r.pr_number, "round": r.round,
        "reviewer_label": chain.get(index).map(review::label).or(r.reviewer_label),
        "policy_source": r.policy_source, "why": why, "author_name": author_name,
    })))
}

pub(super) async fn start_github(
    state: &AppState,
    db: &FsPath,
    r: &CodexReviewRequestRegistration,
) -> Result<bool, String> {
    let now = now_rfc3339();
    let allowed = RetainedQueueStore::reserve_github_review_check_in_path(db, &r.id, &now)
        .map_err(|e| e.to_string())?;
    if !allowed {
        let channel =
            RetainedQueueStore::github_review_channel_from_path(db).map_err(|e| e.to_string())?;
        finish_github_review_step(
            state,
            db,
            r,
            &format!(
                "paused (out of quota since {})",
                channel
                    .paused_at
                    .as_deref()
                    .map_or_else(|| "unknown".to_owned(), crate::queue::quiet::local_time)
            ),
            &now,
        )?;
        return Ok(true);
    }
    // An interrupted post is not repeated: the watcher falls through to the
    // next provider if a restart finds no recorded request comment.
    RetainedQueueStore::mark_github_review_step_in_path(db, &r.id, "posting", 0, &now)
        .map_err(|e| e.to_string())?;
    match github_post_review_request(
        state.github_review_poster.clone(),
        &r.repo,
        r.pr_number,
        r.steer.as_deref(),
    )
    .await
    {
        Ok(c) => {
            RetainedQueueStore::mark_github_review_posted_in_path(
                db,
                &r.id,
                c.comment_id,
                c.comment_url.as_deref(),
                &c.posted_at,
            )
            .map_err(|e| e.to_string())?;
            // Preserve registration's existing reconciliation for GitHub's
            // eventually consistent view immediately after accepting a post.
            if let Ok(GitHubPullRequestState::Open { head_sha }) =
                github_current_pr_state(state.github_review_poster.clone(), &r.repo, r.pr_number)
                    .await
            {
                RetainedQueueStore::reconcile_initial_review_head_in_path(db, &r.id, &head_sha)
                    .map_err(|e| e.to_string())?;
            }
        }
        Err(e) => {
            finish_github_review_step(state, db, r, &format!("failed to start: {e}"), &now)?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn command(program: &str, args: &[&str], cwd: Option<&FsPath>) -> Result<String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let out = cmd.output().with_context(|| format!("start {program}"))?;
    anyhow::ensure!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8(out.stdout)?)
}
pub(super) fn gh(args: &[&str]) -> Result<Value> {
    Ok(serde_json::from_str(&command("gh", args, None)?)?)
}
fn api_list(route: &str) -> Result<Vec<Value>> {
    let pages = gh(&["api", route, "--paginate", "--slurp"])?;
    Ok(pages
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|v| v.as_array().into_iter().flatten().cloned())
        .collect())
}
pub(super) fn git(cwd: &FsPath, args: &[&str]) -> Result<String> {
    command("git", args, Some(cwd))
}
fn root(state: &AppState) -> PathBuf {
    expand_home(&state.config.sm_send.db_path)
        .parent()
        .expect("queue DB parent")
        .join("reviews")
}
pub(super) fn step_dir(state: &AppState, r: &CodexReviewRequestRegistration) -> PathBuf {
    root(state)
        .join(&r.id)
        .join(format!("r{}-s{}", r.round, r.step_index))
}
fn queue_dir(state: &AppState) -> PathBuf {
    expand_home(&state.config.queue_runner_state_dir().to_string_lossy())
}
fn owner(r: &CodexReviewRequestRegistration) -> String {
    format!("review_request:{}:{}", r.id, r.step_index)
}

#[derive(Serialize, Deserialize)]
struct Run {
    repository: PathBuf,
    checkout: PathBuf,
    merge_base: String,
    provider: String,
    codex_home: PathBuf,
}

fn save_run(dir: &FsPath, run: &Run) -> Result<()> {
    fs::create_dir_all(dir)?;
    let temp = dir.join("run.tmp");
    fs::write(&temp, serde_json::to_vec(run)?)?;
    fs::rename(temp, dir.join("run.json"))?;
    Ok(())
}
fn load_run(dir: &FsPath) -> Result<Run> {
    Ok(serde_json::from_slice(&fs::read(dir.join("run.json"))?)?)
}

fn linked_context(db: &FsPath, repo: &str, pr: i64) -> Result<Vec<(String, i64, bool)>> {
    let c = rusqlite::Connection::open(db)?;
    let mut tickets = BTreeSet::new();
    for sql in [
        "SELECT repo,ticket_number FROM work_links WHERE repo=?1 AND pr_number=?2",
        "SELECT repo,issue_number FROM board_prs WHERE pr_repo=?1 AND pr_number=?2",
    ] {
        match c.prepare(sql) {
            Ok(mut s) => {
                let rows = s.query_map(rusqlite::params![repo, pr], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?;
                for r in rows {
                    tickets.insert(r?);
                }
            }
            Err(rusqlite::Error::SqliteFailure(_, Some(e))) if e.contains("no such table") => {}
            Err(e) => return Err(e.into()),
        }
    }
    let mut out: Vec<_> = tickets
        .iter()
        .map(|(r, n)| (r.clone(), *n, false))
        .collect();
    if let Ok(mut s)=c.prepare("SELECT l.goal_repo,l.goal_number FROM board_members m JOIN board_lanes l ON l.id=m.lane_id WHERE m.repo=?1 AND m.number=?2 AND l.ended_at IS NULL ORDER BY l.rank") {
        let mut goals=BTreeSet::new();
        for (repo,n) in tickets {for r in s.query_map(rusqlite::params![repo,n],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?)))? {goals.insert(r?);}}
        out.extend(goals.into_iter().map(|(r,n)|(r,n,true)));
    }
    Ok(out)
}

pub(super) fn prompt(
    state: &AppState,
    r: &CodexReviewRequestRegistration,
    pr: &Value,
    base: &str,
    merge_base: &str,
) -> Result<String> {
    let title = pr["title"].as_str().unwrap_or("");
    let head = r.requested_head_sha.as_deref().context("no head")?;
    let mut text=format!("Review the code changes against the base branch '{base}'. The merge base commit for this comparison is {merge_base}. Run `git diff {merge_base}` to inspect the changes relative to {base}. The head of PR #{} in {}, \"{title}\", is checked out here at {head}. Provide prioritized, actionable findings.\n\n## What this PR is for\n{}\n",r.pr_number,r.repo,cut(pr["body"].as_str().unwrap_or(""),6000));
    let mut linked = linked_context(
        &expand_home(&state.config.sm_send.db_path),
        &r.repo,
        r.pr_number,
    )?;
    for issue in pr["closingIssuesReferences"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(n) = issue["number"].as_i64() {
            if !linked
                .iter()
                .any(|(repo, num, _)| repo == &r.repo && *num == n)
            {
                linked.push((r.repo.clone(), n, false));
            }
        }
    }
    for (repo, n, goal) in linked {
        let issue = gh(&["api", &format!("repos/{repo}/issues/{n}")])?;
        text.push_str(&format!(
            "\n### {}#{n} {}\n{}\n",
            if goal { "Lane goal " } else { "" },
            issue["title"].as_str().unwrap_or(""),
            cut(issue["body"].as_str().unwrap_or(""), 4000)
        ));
    }
    let route = format!("repos/{}/pulls/{}", r.repo, r.pr_number);
    let mut reviews = api_list(&format!("{route}/reviews"))?;
    let lines = api_list(&format!("{route}/comments"))?;
    let replies = api_list(&format!("repos/{}/issues/{}/comments", r.repo, r.pr_number))?;
    reviews.sort_by(|a, b| b["submitted_at"].as_str().cmp(&a["submitted_at"].as_str()));
    let mut history = String::new();
    for rev in reviews {
        let body = rev["body"].as_str().unwrap_or("");
        if !github_actor_is_codex(&rev) && !body.contains("<!-- sm-review-request:") {
            continue;
        }
        history.push_str(&format!(
            "\n{}\n{body}\n",
            rev["html_url"].as_str().unwrap_or("")
        ));
        for line in &lines {
            if line["pull_request_review_id"] == rev["id"] {
                history.push_str(&format!(
                    "{}:{} {}\n",
                    line["path"].as_str().unwrap_or(""),
                    line["line"],
                    line["body"].as_str().unwrap_or("")
                ));
            }
        }
        for reply in &replies {
            if reply["created_at"].as_str() >= rev["submitted_at"].as_str() {
                history.push_str(&format!(
                    "{}: {}\n",
                    reply["user"]["login"].as_str().unwrap_or(""),
                    reply["body"].as_str().unwrap_or("")
                ));
            }
        }
        if history.chars().count() >= 20000 {
            break;
        }
    }
    text.push_str(&format!("\n## Earlier reviews on this PR, and the author's replies\n{}\nDo not repeat a finding that the author has answered unless you disagree with the answer. If you disagree, say why in the finding.\n",cut(&history,20000)));
    if let Some(steer) = &r.steer {
        text.push_str(&format!("\n## The author asks you to focus on\n{steer}\n"));
    }
    Ok(text)
}

fn prepare(
    state: &AppState,
    r: &CodexReviewRequestRegistration,
    step: &Value,
) -> Result<crate::queue::QueueJobRecord> {
    let settings = state.session_store.owner_settings()?;
    let repo_name = r.repo.split('/').next_back().context("invalid repo")?;
    let repository = expand_home(
        state
            .config
            .board
            .checkouts
            .get(&r.repo)
            .map(String::as_str)
            .unwrap_or(&format!("~/projects/{repo_name}")),
    );
    anyhow::ensure!(repository.is_dir(), "no local checkout of {}", r.repo);
    let pr = gh(&[
        "pr",
        "view",
        &r.pr_number.to_string(),
        "--repo",
        &r.repo,
        "--json",
        "title,body,baseRefName,headRefOid,closingIssuesReferences",
    ])?;
    let head = r.requested_head_sha.as_deref().context("no head")?;
    anyhow::ensure!(pr["headRefOid"] == head, "PR head moved before checkout");
    let base = pr["baseRefName"].as_str().context("no base")?;
    git(
        &repository,
        &[
            "fetch",
            "origin",
            &format!("pull/{}/head", r.pr_number),
            &format!("+refs/heads/{base}:refs/remotes/origin/{base}"),
        ],
    )?;
    let merge_base = git(
        &repository,
        &["merge-base", head, &format!("origin/{base}")],
    )?
    .trim()
    .to_owned();
    let short = settings["new_agent"]["repo_short"][&r.repo]
        .as_str()
        .unwrap_or(repo_name);
    anyhow::ensure!(
        short
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')),
        "invalid repo short name"
    );
    // Include request and step to prevent a late cleanup from removing a new
    // head's checkout (round increments only after a completed review).
    let checkout = expand_home(&format!(
        "~/worktrees/{short}-{}-review-r{}-{}-s{}",
        r.pr_number, r.round, r.id, r.step_index
    ));
    let dir = step_dir(state, r);
    let run = Run {
        repository,
        checkout,
        merge_base,
        provider: step["kind"].as_str().context("no provider")?.into(),
        codex_home: std::env::var("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| expand_home("~/.codex")),
    };
    save_run(&dir, &run)?;
    if !run.checkout.exists() {
        git(
            &run.repository,
            &[
                "worktree",
                "add",
                "--detach",
                run.checkout.to_str().context("checkout path")?,
                head,
            ],
        )?;
    }
    fs::write(
        dir.join("prompt.md"),
        prompt(state, r, &pr, base, &run.merge_base)?,
    )?;
    fs::write(dir.join("rubric.md"), review::RUBRIC)?;
    fs::write(dir.join("schema.json"), review::SCHEMA)?;
    let model = quote(step["model"].as_str().context("no model")?);
    let effort = step["effort"].as_str().context("no effort")?;
    let input = quote(&dir.join("prompt.md").to_string_lossy());
    let script = if run.provider == "codex" {
        format!(
            "exec {} exec -m {model} -c {} -s read-only --json review - < {input} > {}",
            quote(&state.config.codex_fork.command),
            quote(&format!("model_reasoning_effort=\"{effort}\"")),
            quote(&dir.join("events.jsonl").to_string_lossy())
        )
    } else {
        format!("exec claude -p --model {model} --effort {} --output-format json --json-schema {} --append-system-prompt {} --allowedTools 'Read' 'Grep' 'Glob' 'Bash(git diff:*)' 'Bash(git log:*)' 'Bash(git show:*)' --disallowedTools 'Edit' 'Write' 'NotebookEdit' 'WebFetch' 'WebSearch' < {input} > {}",quote(effort),quote(review::SCHEMA),quote(review::RUBRIC),quote(&dir.join("result.json").to_string_lossy()))
    };
    let job = RetainedQueueStore::create_owned_queue_job(
        &queue_dir(state),
        CreateQueueJob {
local_submitter: None,
            job_type: "review".into(),
            label: format!("review {short} #{} r{}", r.pr_number, r.round),
            requester_session_id: r.requester_session_id.clone(),
            notify_session_id: String::new(),
            cwd: run.checkout.to_string_lossy().into_owned(),
            argv: None,
            script: Some(format!("unset CLAUDECODE CLAUDE_SESSION_MANAGER_ID SESSION_MANAGER_ID SM_SESSION_ID\n{script}")),
            env: review::environment(std::env::vars()),
            timeout_seconds: 2700,
            cpu_percent: None,
            gpu_percent: None,
            memory_bytes: None,
            rank_tickets: None,
        },
        &owner(r),
    )?;
    Ok(job)
}

/// D3 step 1: the reason to skip a provider whose weekly meter is too high.
pub(super) fn meter_skip(state: &AppState, provider: &str) -> Result<Option<String>> {
    let percent = review::meter(&expand_home(&state.config.usage.db_path), provider)?;
    let limit = state.session_store.owner_settings()?["reviews"]["skip_meter_percent"]
        .as_i64()
        .unwrap_or(95);
    Ok(review::should_skip(percent, limit).then(|| {
        format!(
            "{} weekly meter at {}%",
            if provider == "codex" {
                "Codex"
            } else {
                "Claude"
            },
            percent.unwrap_or(0.0)
        )
    }))
}

pub(super) async fn poll_run(
    state: &AppState,
    db: &FsPath,
    r: &CodexReviewRequestRegistration,
    step: &Value,
) -> Result<bool, String> {
    let state = state.clone();
    let db = db.to_path_buf();
    let r = r.clone();
    let step = step.clone();
    tokio::task::spawn_blocking(move || poll_run_blocking(&state, &db, &r, &step))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

fn poll_run_blocking(
    state: &AppState,
    db: &FsPath,
    r: &CodexReviewRequestRegistration,
    step: &Value,
) -> Result<bool> {
    let advance = |reason: &str| -> Result<bool> {
        cleanup_step(state, r)?;
        finish_github_review_step(state, db, r, reason, &now_rfc3339())
            .map_err(anyhow::Error::msg)?;
        Ok(true)
    };
    if !state.config.rust_core.runtime_enabled {
        return advance("failed to start: runtime disabled");
    }
    let job = match RetainedQueueStore::owned_queue_job(&queue_dir(state), &owner(r))? {
        Some(job) => job,
        None => {
            if let Some(reason) = meter_skip(state, step["kind"].as_str().context("provider")?)? {
                return advance(&reason);
            }
            match prepare(state, r, step) {
                Ok(job) => job,
                Err(e) => {
                    let error = e.to_string();
                    let first = error.lines().next().unwrap_or("unknown error");
                    return advance(&if first.starts_with("no local checkout of ") {
                        first.to_owned()
                    } else {
                        format!("failed to start: {first}")
                    });
                }
            }
        }
    };
    RetainedQueueStore::attach_review_job(db, &r.id, r.step_index, &job)?;
    let current = RetainedQueueStore::get_codex_review_request_from_path(db, &r.id)?
        .context("request disappeared")?;
    if !current.is_active {
        cleanup_step(state, r)?;
        return Ok(true);
    }
    if job.state == "pending" {
        RetainedQueueStore::admit_queue_jobs_in_state_dir_continuing_after_failed_start_with_policy(&queue_dir(state),db,state.config.queue_runner.cancel_grace_seconds,queue_admission_policy(state))?;
        return Ok(false);
    }
    if job.state == "running" {
        return Ok(false);
    }
    if job.state == "timed_out" {
        return advance("no result in 45 minutes");
    }
    if job.state != "succeeded" || job.exit_code != Some(0) {
        return advance(&format!("failed: exit {}", job.exit_code.unwrap_or(-1)));
    }
    let dir = step_dir(state, r);
    let run = load_run(&dir)?;
    let output = if run.provider == "codex" {
        fs::read_to_string(dir.join("events.jsonl"))
            .map_err(anyhow::Error::from)
            .and_then(|text| review::codex_output(&text, &run.codex_home.join("sessions")))
    } else {
        fs::read_to_string(dir.join("result.json"))
            .map_err(anyhow::Error::from)
            .and_then(|text| review::claude_output(&text))
    };
    let output = match output {
        Ok(v) => v,
        Err(_) => return advance("failed: no review output"),
    };
    post_review(
        state,
        db,
        r,
        PostTarget {
            checkout: &run.checkout,
            merge_base: &run.merge_base,
            source: &format!("{}_run", run.provider),
            cache: true,
        },
        &output,
        || cleanup_step(state, r),
    )?;
    cleanup_step(state, r)?;
    Ok(true)
}

/// Where a review's findings are placed, and how it completes the request.
pub(super) struct PostTarget<'a> {
    pub checkout: &'a FsPath,
    pub merge_base: &'a str,
    /// `review_source` on the completed request.
    pub source: &'a str,
    /// A run's payloads survive a restart; a paired reviewer resubmits.
    pub cache: bool,
}

/// Posts the review (F) and completes the request. `None` when the request
/// is no longer current, so nothing was posted.
pub(super) fn post_review(
    state: &AppState,
    db: &FsPath,
    r: &CodexReviewRequestRegistration,
    target: PostTarget<'_>,
    output: &Value,
    before_post: impl FnOnce() -> Result<()>,
) -> Result<Option<Value>> {
    let head = r.requested_head_sha.as_deref().context("head")?;
    let dir = step_dir(state, r);
    fs::create_dir_all(&dir)?;
    let cached = dir.join("payloads.json");
    // Capture both payloads before deleting the terminal job's checkout. A
    // transient GitHub failure can then be retried after restart without
    // retaining a worktree or rerunning the provider.
    let payloads: Value = if target.cache && cached.exists() {
        serde_json::from_slice(&fs::read(&cached)?)?
    } else {
        let mut diffs = BTreeMap::new();
        let files = git(
            target.checkout,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--name-only",
                "-z",
                target.merge_base,
                head,
            ],
        )?;
        for path in files.split('\0').filter(|p| !p.is_empty()) {
            diffs.insert(
                path.to_owned(),
                git(
                    target.checkout,
                    &[
                        "diff",
                        "--no-ext-diff",
                        "--no-textconv",
                        target.merge_base,
                        head,
                        "--",
                        path,
                    ],
                )?,
            );
        }
        let payloads = json!({"inline":review::payload(r,target.checkout,output,&diffs,true),"body":review::payload(r,target.checkout,output,&diffs,false)});
        fs::write(dir.join("payloads.tmp"), payloads.to_string())?;
        fs::rename(dir.join("payloads.tmp"), &cached)?;
        payloads
    };
    before_post()?;
    let route = format!("repos/{}/pulls/{}/reviews", r.repo, r.pr_number);
    let marker = format!("<!-- sm-review-request:{} -->", r.id);
    // Recover a POST accepted before a crash or an ambiguous network response.
    let existing = api_list(&route)?.into_iter().find(|rev| {
        rev["body"].as_str().is_some_and(|b| b.contains(&marker)) && rev["commit_id"] == head
    });
    let posted = if let Some(v) = existing {
        v
    } else {
        review::post_with_body_fallback(&payloads["inline"], &payloads["body"], |payload| {
            if !posting_still_current(state, db, r)? {
                return Ok(Value::Null);
            }
            let payload_file = dir.join("review.json");
            fs::write(&payload_file, payload.to_string())?;
            gh(&[
                "api",
                &route,
                "--method",
                "POST",
                "--input",
                payload_file.to_str().context("payload path")?,
            ])
        })?
    };
    if posted.is_null() {
        return Ok(None);
    }
    RetainedQueueStore::record_review_findings(db, &r.id, &review::counts(output))?;
    complete_codex_review_request(
        state,
        db,
        &r.id,
        r,
        GitHubReviewMatch {
            source: target.source.to_owned(),
            id: posted.get("id").cloned(),
            head_sha: Some(head.to_owned()),
            url: posted["html_url"].as_str().map(str::to_owned),
            created_at: posted["submitted_at"]
                .as_str()
                .unwrap_or(&now_rfc3339())
                .to_owned(),
        },
        &now_rfc3339(),
    )
    .map_err(anyhow::Error::msg)?;
    Ok(Some(posted))
}

/// Called immediately before *each* POST, including the 422 body-only retry.
fn posting_still_current(
    state: &AppState,
    db: &FsPath,
    r: &CodexReviewRequestRegistration,
) -> Result<bool> {
    if !RetainedQueueStore::get_codex_review_request_from_path(db, &r.id)?
        .is_some_and(|r| r.is_active)
    {
        return Ok(false);
    }
    let head = r.requested_head_sha.as_deref().context("head")?;
    match state
        .github_review_poster
        .current_pr_state(&r.repo, r.pr_number)
        .map_err(anyhow::Error::msg)?
    {
        GitHubPullRequestState::Open { head_sha } if head_sha != head => {
            supersede_codex_review_request_for_head_change(
                state,
                db,
                &r.id,
                r,
                head,
                &head_sha,
                &now_rfc3339(),
            )
            .map_err(anyhow::Error::msg)?;
            return Ok(false);
        }
        GitHubPullRequestState::Closed { state: status } => {
            terminate_codex_review_request_for_closed_pr(
                db,
                &r.id,
                r.pr_number,
                &status,
                &now_rfc3339(),
            )
            .map_err(anyhow::Error::msg)?;
            return Ok(false);
        }
        _ => {}
    }
    if r.latest_request_comment_id.is_some() {
        if let Some(review) = state
            .github_review_poster
            .find_fresh_codex_review_or_comment(
                &r.repo,
                r.pr_number,
                &r.requested_at,
                head,
                r.latest_request_comment_id,
            )
            .map_err(anyhow::Error::msg)?
        {
            complete_codex_review_request(state, db, &r.id, r, review, &now_rfc3339())
                .map_err(anyhow::Error::msg)?;
            return Ok(false);
        }
    }
    Ok(true)
}

fn cleanup_step(state: &AppState, r: &CodexReviewRequestRegistration) -> Result<()> {
    if let Some(job) = RetainedQueueStore::owned_queue_job(&queue_dir(state), &owner(r))? {
        if matches!(job.state.as_str(), "running" | "pending") {
            RetainedQueueStore::cancel_queue_job_in_state_dir(
                &queue_dir(state),
                &expand_home(&state.config.sm_send.db_path),
                &job.id,
                state.config.queue_runner.cancel_grace_seconds,
                queue_admission_policy(state),
                true,
            )?;
        }
    }
    let dir = step_dir(state, r);
    if dir.join("run.json").exists() {
        let run = load_run(&dir)?;
        if run.checkout.exists() {
            git(
                &run.repository,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    run.checkout.to_str().context("checkout path")?,
                ],
            )?;
        }
    }
    Ok(())
}

pub(super) fn cleanup_request(state: &AppState, id: &str) -> Result<()> {
    let db = expand_home(&state.config.sm_send.db_path);
    let Some(mut r) = RetainedQueueStore::get_codex_review_request_from_path(&db, id)? else {
        return Ok(());
    };
    if r.is_active {
        return Ok(());
    }
    let chain: Vec<Value> = serde_json::from_str(r.chain_json.as_deref().unwrap_or("[]"))?;
    for index in 0..chain.len() {
        r.step_index = index as i64;
        cleanup_step(state, &r)?;
    }
    Ok(())
}

pub(super) fn start_sweeper(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            if state.shutdown().is_stopped() {
                return;
            }
            let state = state.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<()> {
                let dir = root(&state);
                if !dir.exists() {
                    return Ok(());
                }
                for entry in fs::read_dir(&dir)? {
                    let entry = entry?;
                    if !entry.file_type()?.is_dir() {
                        continue;
                    }
                    let id = entry.file_name().to_string_lossy().into_owned();
                    cleanup_request(&state, &id)?;
                    let r = RetainedQueueStore::get_codex_review_request_from_path(
                        &expand_home(&state.config.sm_send.db_path),
                        &id,
                    )?;
                    if r.as_ref().is_none_or(|r| !r.is_active)
                        && entry.metadata()?.modified()?.elapsed().unwrap_or_default()
                            > Duration::from_secs(14 * 86400)
                    {
                        fs::remove_dir_all(entry.path())?;
                    }
                }
                Ok(())
            })
            .await;
            if let Ok(Err(e)) = result {
                eprintln!("review cleanup: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekly_meter_advances_the_persisted_request_to_the_other_provider() {
        let dir = std::env::temp_dir().join(format!(
            "sm-review-meter-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut config = AppConfig::default();
        config.paths.state_file = dir.join("sessions.json").to_string_lossy().into_owned();
        config.sm_send.db_path = dir.join("messages.db").to_string_lossy().into_owned();
        config.usage.db_path = dir.join("usage.db").to_string_lossy().into_owned();
        config.rust_core.fixture_writes_enabled = true;
        let mut state = AppState::new(config);
        // Enable only the run-step code under test after constructing isolated
        // stores. The selected provider must be skipped before any process starts.
        state.config.rust_core.runtime_enabled = true;
        let db = expand_home(&state.config.sm_send.db_path);
        let usage = expand_home(&state.config.usage.db_path);
        let _burn = crate::usage_burn::UsageBurnStore::new(&usage).unwrap();
        let c = rusqlite::Connection::open(&usage).unwrap();
        c.execute_batch("INSERT INTO accounts(account_key,provider,external_id,first_seen,last_seen) VALUES('codex:meter-test','codex','meter-test','2026-09-30T12:00:00Z','2026-09-30T12:00:00Z');
            INSERT INTO burn_samples(account_key,window_kind,window_start,percent,resets_at,source,observed_at) VALUES('codex:meter-test','codex_10080','2026-09-28T00:00:00Z',96,'2026-10-05T00:00:00Z','test','2026-09-30T12:00:00Z');").unwrap();
        let r = RetainedQueueStore::create_codex_review_request_in_path(
            &db,
            CreateCodexReviewRequest {
                repo: "example/missing-review-fixture".into(),
                pr_number: 7,
                requester_session_id: Some("author".into()),
                notify_session_id: "author".into(),
                steer: None,
                requested_head_sha: "1111111111111111111111111111111111111111".into(),
                latest_request_comment_id: None,
                latest_request_comment_url: None,
                latest_request_posted_at: now_rfc3339(),
                poll_interval_seconds: 30,
                retry_interval_seconds: 120,
            },
        )
        .unwrap();
        let step = json!({"kind":"codex","model":"gpt-6-sol","effort":"medium"});
        RetainedQueueStore::initialize_review_chain(&db, &r.id, &review::chain(&step), "default")
            .unwrap();
        let r = RetainedQueueStore::get_codex_review_request_from_path(&db, &r.id)
            .unwrap()
            .unwrap();
        assert!(poll_run_blocking(&state, &db, &r, &step).unwrap());
        let advanced = RetainedQueueStore::get_codex_review_request_from_path(&db, &r.id)
            .unwrap()
            .unwrap();
        assert!(advanced.is_active);
        assert_eq!(advanced.step_index, 1);
        assert_eq!(
            advanced.reviewer_label.as_deref(),
            Some("Claude run (opus, high)")
        );
        assert_eq!(
            advanced.last_error.as_deref(),
            Some("Codex weekly meter at 96%")
        );
        assert!(advanced.run_job_id.is_none());
        drop(c);
        drop(state);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn review_job_fields_name_the_request_and_why_this_reviewer() {
        let dir = std::env::temp_dir().join(format!(
            "sm-review-job-fields-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut config = AppConfig::default();
        config.paths.state_file = dir.join("sessions.json").to_string_lossy().into_owned();
        config.sm_send.db_path = dir.join("messages.db").to_string_lossy().into_owned();
        config.usage.db_path = dir.join("usage.db").to_string_lossy().into_owned();
        let state = AppState::new(config);
        let db = expand_home(&state.config.sm_send.db_path);
        let r = RetainedQueueStore::create_codex_review_request_in_path(
            &db,
            CreateCodexReviewRequest {
                repo: "example/far".into(),
                pr_number: 1851,
                requester_session_id: Some("author".into()),
                notify_session_id: "author".into(),
                steer: None,
                requested_head_sha: "f9d61b2d48f9d61b2d48f9d61b2d48f9d61b2d48".into(),
                latest_request_comment_id: None,
                latest_request_comment_url: None,
                latest_request_posted_at: now_rfc3339(),
                poll_interval_seconds: 30,
                retry_interval_seconds: 120,
            },
        )
        .unwrap();
        let chain = review::chain(&json!({"kind": "github_codex"}));
        RetainedQueueStore::initialize_review_chain(&db, &r.id, &chain, "default").unwrap();
        let job = |index: i64| {
            RetainedQueueStore::create_owned_queue_job(
                &queue_dir(&state),
                CreateQueueJob {
                    local_submitter: None,
                    job_type: "review".into(),
                    label: format!("review far #1851 r1 step {index}"),
                    requester_session_id: Some("author".into()),
                    notify_session_id: String::new(),
                    cwd: dir.to_string_lossy().into_owned(),
                    argv: None,
                    script: Some("true".into()),
                    env: Default::default(),
                    timeout_seconds: 60,
                    cpu_percent: None,
                    gpu_percent: None,
                    memory_bytes: None,
                    rank_tickets: None,
                },
                &format!("review_request:{}:{index}", r.id),
            )
            .unwrap()
        };
        let first = job(0);
        let fields = job_fields(&state, &first).unwrap().unwrap();
        assert_eq!(fields["why"], "default");
        assert_eq!(fields["reviewer_label"], "GitHub Codex");
        assert_eq!(fields["author_name"], "author");
        RetainedQueueStore::finish_github_review_step_in_path(
            &db,
            &r.id,
            "out of code-review quota",
            &now_rfc3339(),
            "wake",
        )
        .unwrap();
        let fallback = job(1);
        let fields = job_fields(&state, &fallback).unwrap().unwrap();
        assert_eq!(fields["request_id"], r.id.as_str());
        assert_eq!(fields["pr_number"], 1851);
        assert_eq!(fields["reviewer_label"], "Codex run (gpt-6-sol, medium)");
        assert_eq!(fields["policy_source"], "default");
        assert_eq!(
            fields["why"],
            "GitHub Codex: out of code-review quota, so its fallback"
        );
        let mut other = fallback.clone();
        other.job_type = "tests".into();
        assert!(job_fields(&state, &other).unwrap().is_none());
        drop(state);
        fs::remove_dir_all(dir).unwrap();
    }
}
