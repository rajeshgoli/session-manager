//! GitHub merge-chain resolution and resumable review-PR creation.
use super::*;
use crate::owner_docs::OwnerDocReview;

fn conflict(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::CONFLICT,
        detail: detail.into(),
    }
}

pub fn resolve_target(
    source: &dyn OwnerDocSource,
    doc: &OwnerDoc,
    latest_blob: &str,
) -> Result<Value, String> {
    let Some(pr) = doc.pr_number else {
        return resolve_without_pr(source, doc, latest_blob);
    };
    let info = source.doc_pr_info(&doc.repo, pr)?;
    if info["state"] == "CLOSED" {
        let branch = info["headRefName"]
            .as_str()
            .ok_or("PR has no head branch")?;
        let step = source.doc_branch_state(&doc.repo, branch)?;
        return if step["tip"].is_string() {
            Ok(json!({"kind":"reopen","pr":pr}))
        } else {
            Err(format!("PR #{pr} is closed and its branch was deleted"))
        };
    }
    if info["state"] == "OPEN" {
        return Ok(json!({"kind":"reopen","pr":pr}));
    }
    let branch = info["baseRefName"]
        .as_str()
        .ok_or("PR has no base branch")?;
    follow_merges(
        source,
        doc,
        latest_blob,
        branch,
        info["mergedAt"].as_str().unwrap_or(""),
    )
}

/// A doc published from a commit (sm#1946): the one open PR that changes its
/// file takes the review; with none, a new PR opens from the default branch,
/// as for a merged doc.
fn resolve_without_pr(
    source: &dyn OwnerDocSource,
    doc: &OwnerDoc,
    latest_blob: &str,
) -> Result<Value, String> {
    let found = source.doc_open_prs(&doc.repo, &doc.path)?;
    let prs: Vec<i64> = found["prs"]
        .as_array()
        .ok_or("missing open PR list")?
        .iter()
        .filter_map(Value::as_i64)
        .collect();
    match prs.as_slice() {
        [pr] => Ok(json!({"kind":"attach","pr":pr})),
        [] => {
            let branch = found["default_branch"]
                .as_str()
                .ok_or("repo has no default branch")?;
            follow_merges(source, doc, latest_blob, branch, "")
                .map_err(|e| format!("No open PR changes {}, and {e}", doc.path))
        }
        _ => Err(format!(
            "Open PRs {} all change {}. Ask the doc's agent to publish it on one with sm doc publish --pr.",
            prs.iter()
                .map(|p| format!("#{p}"))
                .collect::<Vec<_>>()
                .join(", "),
            doc.path
        )),
    }
}

/// Follows `branch` through the PRs that merged it after `after` to where the
/// doc now lives.
fn follow_merges(
    source: &dyn OwnerDocSource,
    doc: &OwnerDoc,
    latest_blob: &str,
    branch: &str,
    after: &str,
) -> Result<Value, String> {
    let mut branch = branch.to_owned();
    let mut after = after.to_owned();
    let mut visited = BTreeSet::new();
    for _ in 0..10 {
        if !visited.insert(branch.clone()) {
            return Err(format!("merge history loops at {branch}"));
        }
        let step = source.doc_branch_state(&doc.repo, &branch)?;
        let rows = step["prs"].as_array().ok_or("missing branch PR history")?;
        let open = rows.iter().any(|p| p["state"] == "OPEN");
        let merged: Vec<_> = rows
            .iter()
            .filter(|p| {
                p["state"] == "MERGED" && p["mergedAt"].as_str().unwrap_or("") > after.as_str()
            })
            .collect();
        let tip = step["tip"].as_str();
        let stop = if open {
            true
        } else if merged.is_empty() {
            if tip.is_none() {
                return Err(format!("{branch} was deleted without a merged PR"));
            }
            true
        } else {
            if merged.len() > 1 {
                return Err(format!(
                    "{branch} merged into more than one branch ({})",
                    merged
                        .iter()
                        .map(|p| format!("#{}", p["number"]))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            tip.is_some_and(|tip| Some(tip) != merged[0]["headRefOid"].as_str())
        };
        if stop {
            let tip = tip.ok_or_else(|| format!("{branch} was deleted without a merged PR"))?;
            let (_, blob) = source
                .fetch_doc_with_blob_sha(&doc.repo, &doc.path, tip)
                .map_err(|_| format!("{} is not on {branch}", doc.path))?;
            return Ok(
                json!({"kind":"new_pr","base":branch,"tip_sha":tip,"doc_changed":blob!=latest_blob}),
            );
        }
        branch = merged[0]["baseRefName"]
            .as_str()
            .ok_or("merged PR has no base")?
            .into();
        after = merged[0]["mergedAt"].as_str().unwrap_or("").into();
    }
    Err("merge history is longer than 10 steps".into())
}

pub(super) fn pr_info(repo: &str, pr: i64) -> Result<Value, String> {
    let args = vec![
        "pr".into(),
        "view".into(),
        pr.to_string(),
        "--repo".into(),
        repo.into(),
        "--json".into(),
        "state,baseRefName,headRefName,headRefOid,mergedAt".into(),
    ];
    command_json(&args)
}
fn command_json(args: &[String]) -> Result<Value, String> {
    let output = gh_command_output(args, Duration::from_secs(30)).map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(command_stderr(&output));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())
}
fn api(repo: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
    let mut args = vec!["api".into(), format!("repos/{repo}/{path}")];
    let input = body
        .map(|body| {
            let mut nonce = [0; 8];
            OsRng.fill_bytes(&mut nonce);
            let path = std::env::temp_dir().join(format!(
                "sm-doc-api-{}.json",
                nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
            ));
            fs::write(&path, body.to_string()).map_err(|e| e.to_string())?;
            args.extend([
                "--method".into(),
                "POST".into(),
                "--input".into(),
                path.display().to_string(),
            ]);
            Ok::<_, String>(path)
        })
        .transpose()?;
    let result = command_json(&args);
    if let Some(path) = input {
        let _ = fs::remove_file(path);
    }
    result
}
pub(super) fn branch_state(repo: &str, branch: &str) -> Result<Value, String> {
    let (owner, name) = repo.split_once('/').ok_or("invalid repo")?;
    let mut rows = Vec::new();
    let mut cursor = Value::Null;
    let mut tip = Value::Null;
    loop {
        let data=gh_graphql("query($o:String!,$n:String!,$b:String!,$ref:String!,$after:String){repository(owner:$o,name:$n){pullRequests(headRefName:$b,first:100,states:[MERGED,OPEN],after:$after){nodes{number state baseRefName headRefOid mergedAt} pageInfo{hasNextPage endCursor}} ref(qualifiedName:$ref){target{oid}}}}",json!({"o":owner,"n":name,"b":branch,"ref":format!("refs/heads/{branch}"),"after":cursor}),true)?;
        let repo = &data["repository"];
        if tip.is_null() {
            tip = repo["ref"]["target"]["oid"].clone();
        }
        rows.extend(
            repo["pullRequests"]["nodes"]
                .as_array()
                .ok_or("missing PR history")?
                .iter()
                .cloned(),
        );
        if repo["pullRequests"]["pageInfo"]["hasNextPage"] != true {
            break;
        }
        cursor = repo["pullRequests"]["pageInfo"]["endCursor"].clone();
    }
    Ok(json!({"tip":tip,"prs":rows}))
}
/// The default branch and the open PRs whose diff includes `path`, among the
/// 100 most recently updated, each read to its first 100 files.
pub(super) fn open_prs(repo: &str, path: &str) -> Result<Value, String> {
    let (owner, name) = repo.split_once('/').ok_or("invalid repo")?;
    let data = gh_graphql("query($o:String!,$n:String!){repository(owner:$o,name:$n){defaultBranchRef{name} pullRequests(states:[OPEN],first:100,orderBy:{field:UPDATED_AT,direction:DESC}){nodes{number files(first:100){nodes{path}}}}}}",json!({"o":owner,"n":name}),true)?;
    let repo = &data["repository"];
    let prs: Vec<Value> = repo["pullRequests"]["nodes"]
        .as_array()
        .ok_or("missing open PRs")?
        .iter()
        .filter(|pr| {
            pr["files"]["nodes"]
                .as_array()
                .is_some_and(|files| files.iter().any(|f| f["path"] == path))
        })
        .map(|pr| pr["number"].clone())
        .collect();
    Ok(json!({"default_branch":repo["defaultBranchRef"]["name"],"prs":prs}))
}
pub(super) fn ensure_pr(
    repo: &str,
    path: &str,
    title: &str,
    base: &str,
    tip: &str,
    branch: &str,
    owner: &str,
) -> Result<(i64, String), String> {
    let refs = api(repo, &format!("git/matching-refs/heads/{branch}"), None)?;
    let reference = format!("refs/heads/{branch}");
    let existing = refs
        .as_array()
        .and_then(|r| r.iter().find(|r| r["ref"] == reference));
    let head = if let Some(r) = existing {
        r["object"]["sha"]
            .as_str()
            .ok_or("missing branch SHA")?
            .to_owned()
    } else {
        let parent = api(repo, &format!("git/commits/{tip}"), None)?;
        let commit = api(
            repo,
            "git/commits",
            Some(
                json!({"message":format!("Open {path} for review"),"tree":parent["tree"]["sha"],"parents":[tip]}),
            ),
        )?;
        let head = commit["sha"]
            .as_str()
            .ok_or("missing created commit SHA")?
            .to_owned();
        api(repo, "git/refs", Some(json!({"ref":reference,"sha":head})))?;
        head
    };
    let prs = command_json(&[
        "pr".into(),
        "list".into(),
        "--repo".into(),
        repo.into(),
        "--head".into(),
        branch.into(),
        "--state".into(),
        "open".into(),
        "--json".into(),
        "number".into(),
    ])?;
    if let Some(pr) = prs
        .as_array()
        .and_then(|a| a.first())
        .and_then(|p| p["number"].as_i64())
    {
        return Ok((pr, head));
    }
    let pr = api(
        repo,
        "pulls",
        Some(
            json!({"head":branch,"base":base,"title":format!("(Docs) {title}: review"),"body":format!("sm opened this PR for {owner}'s review of {path}, from {base} at {}.",&tip[..7.min(tip.len())])}),
        ),
    )?;
    Ok((
        pr["number"].as_i64().ok_or("missing created PR number")?,
        head,
    ))
}
pub(super) fn reopen_pr(repo: &str, pr: i64) -> Result<(), String> {
    let args = vec![
        "pr".into(),
        "reopen".into(),
        pr.to_string(),
        "--repo".into(),
        repo.into(),
    ];
    let output = gh_command_output(&args, Duration::from_secs(30)).map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_stderr(&output))
    }
}

/// Whether the review goes to a PR other than the one the reviewed revision
/// was published on, so its comments travel as quotes in the review body.
pub(super) fn body_only(target: &Value) -> bool {
    matches!(target["kind"].as_str(), Some("new_pr" | "attach"))
}

/// Resolve outside the write transaction, then save this value with the submission.
pub(super) async fn target(state: &Arc<AppState>, doc: &OwnerDoc) -> Result<Value, ApiError> {
    let latest = owner_doc_store(state)
        .publishes(&doc.id)?
        .pop()
        .ok_or(ApiError::NotFound("Doc publish not found"))?;
    let source = state.owner_doc_source.clone();
    let doc = doc.clone();
    tokio::task::spawn_blocking(move || resolve_target(source.as_ref(), &doc, &latest.blob_sha))
        .await
        .map_err(|e| anyhow::anyhow!("target lookup failed: {e}"))?
        .map_err(conflict)
}

/// Each completed step is durable; a lost GitHub response is reconciled by branch name.
pub(super) async fn prepare(
    state: &Arc<AppState>,
    review: &OwnerDocReview,
) -> Result<(), ApiError> {
    let store = owner_doc_store(state);
    let Some(mut target) = store.review_target(&review.id)? else {
        return Ok(());
    };
    let doc = store
        .get(&review.doc_id)?
        .ok_or(ApiError::NotFound("Doc not found"))?;
    if target["ready"] == true {
        return Ok(());
    }
    if target["target_pr"].is_null() {
        let source = state.owner_doc_source.clone();
        let owner = state.config.owner_name.clone();
        let d = doc.clone();
        let t = target.clone();
        let (pr, head) = tokio::task::spawn_blocking(move || {
            if t["kind"] == "new_pr" {
                source.ensure_doc_review_pr(
                    &d.repo,
                    &d.path,
                    &d.title,
                    t["base"].as_str().unwrap_or(""),
                    t["tip_sha"].as_str().unwrap_or(""),
                    t["branch"].as_str().unwrap_or(""),
                    &owner,
                )
            } else {
                let pr = t["pr"].as_i64().ok_or("missing reopen PR")?;
                if !source.pull_request(&d.repo, pr)?.is_open() {
                    source.reopen_doc_pr(&d.repo, pr)?;
                }
                Ok((pr, source.pull_request(&d.repo, pr)?.head_sha))
            }
        })
        .await
        .map_err(|e| anyhow::anyhow!("PR creation task failed: {e}"))?
        .map_err(github_failure)?;
        target["target_pr"] = json!(pr);
        target["head_sha"] = json!(head);
        store.set_review_target(&review.id, &target)?;
    }
    let pr = target["target_pr"]
        .as_i64()
        .ok_or_else(|| conflict("missing review PR"))?;
    if target["hold"] == true && target["held"] != true {
        super::super::merge_holds::change(
            state.clone(),
            super::super::merge_holds::HoldRequest {
                repo: doc.repo.clone(),
                pr,
                reason: None,
                requester_session_id: None,
            },
            false,
            true,
        )
        .await?;
        target["held"] = json!(true);
        store.set_review_target(&review.id, &target)?;
    }
    if body_only(&target) {
        let head = target["head_sha"]
            .as_str()
            .ok_or_else(|| conflict("missing review head"))?
            .to_owned();
        let publishes = store.publishes(&doc.id)?;
        if !publishes
            .iter()
            .any(|p| p.pr_number == Some(pr) && p.commit_sha == head)
        {
            let source = state.owner_doc_source.clone();
            let d = doc.clone();
            let h = head.clone();
            let (bytes, blob) = tokio::task::spawn_blocking(move || {
                source.fetch_doc_with_blob_sha(&d.repo, &d.path, &h)
            })
            .await
            .map_err(|e| anyhow::anyhow!("doc fetch failed: {e}"))?
            .map_err(|e| github_failure(format!("{e:?}")))?;
            if git_blob_sha(&bytes) != blob {
                return Err(github_failure("Document blob mismatch".into()));
            }
            if target["kind"] == "attach" {
                target["doc_changed"] = json!(review.blob_sha != blob);
                store.set_review_target(&review.id, &target)?;
            }
            store.publish_moving(
                PublishOwnerDoc {
                    repo: doc.repo.clone(),
                    path: doc.path.clone(),
                    pr_number: Some(pr),
                    session_id: doc.author_session_id.clone(),
                    session_name: doc.author_session_name.clone(),
                    title: doc.title.clone(),
                    note: doc.note.clone(),
                    commit_sha: head,
                    blob_sha: blob,
                    review_requested: false,
                    checkout_root: publishes.last().and_then(|p| p.checkout_root.clone()),
                },
                |_| true,
                Some(&doc.id),
            )?;
        }
    }
    state
        .owner_doc_pr_cache
        .lock()
        .unwrap()
        .remove(&(doc.repo.clone(), pr));
    state.owner_doc_reopen_cache.lock().unwrap().remove(&doc.id);
    target["ready"] = json!(true);
    store.set_review_target(&review.id, &target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Source {
        info: Value,
        branches: BTreeMap<String, Value>,
        missing: bool,
        open_prs: Value,
    }
    impl OwnerDocSource for Source {
        fn doc_open_prs(&self, _: &str, _: &str) -> Result<Value, String> {
            Ok(json!({"default_branch":"main","prs":self.open_prs}))
        }
        fn doc_pr_info(&self, _: &str, _: i64) -> Result<Value, String> {
            Ok(self.info.clone())
        }
        fn doc_branch_state(&self, _: &str, b: &str) -> Result<Value, String> {
            self.branches
                .get(b)
                .cloned()
                .ok_or(format!("unexpected branch {b}"))
        }
        fn fetch_doc(&self, _: &str, _: &str, _: &str) -> Result<Vec<u8>, DocFetchError> {
            if self.missing {
                Err(DocFetchError::NotFound("missing".into()))
            } else {
                Ok(vec![])
            }
        }
        fn fetch_doc_with_blob_sha(
            &self,
            r: &str,
            p: &str,
            s: &str,
        ) -> Result<(Vec<u8>, String), DocFetchError> {
            Ok((self.fetch_doc(r, p, s)?, "blob".into()))
        }
    }
    fn doc() -> OwnerDoc {
        OwnerDoc {
            id: "doc00001".into(),
            repo: "a/b".into(),
            path: "memo.html".into(),
            pr_number: Some(1),
            author_session_id: "author".into(),
            author_session_name: None,
            title: "Memo".into(),
            note: None,
            retracted_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }
    fn source(branches: Value) -> Source {
        Source {
            info: json!({"state":"MERGED","baseRefName":"epic","mergedAt":"2026-01-01"}),
            branches: serde_json::from_value(branches).unwrap(),
            missing: false,
            open_prs: json!([]),
        }
    }
    fn merged(base: &str, at: &str) -> Value {
        json!({"number":2,"state":"MERGED","baseRefName":base,"mergedAt":at,"headRefOid":"tip"})
    }
    #[test]
    fn doc_without_pr_goes_to_the_one_open_pr_changing_it_else_the_default_branch() {
        let no_pr = OwnerDoc {
            pr_number: None,
            ..doc()
        };
        let mut s = source(json!({"main":{"tip":"main-tip","prs":[]}}));
        s.open_prs = json!([1906]);
        assert_eq!(
            resolve_target(&s, &no_pr, "blob").unwrap(),
            json!({"kind":"attach","pr":1906})
        );
        s.open_prs = json!([]);
        assert_eq!(
            resolve_target(&s, &no_pr, "blob").unwrap(),
            json!({"kind":"new_pr","base":"main","tip_sha":"main-tip","doc_changed":false})
        );
        s.missing = true;
        assert_eq!(
            resolve_target(&s, &no_pr, "blob").unwrap_err(),
            "No open PR changes memo.html, and memo.html is not on main"
        );
        s.open_prs = json!([7, 9]);
        assert!(resolve_target(&s, &no_pr, "blob")
            .unwrap_err()
            .starts_with("Open PRs #7, #9 all change memo.html."));
    }
    #[test]
    fn follows_deleted_or_unchanged_merged_branch_but_stops_at_open_or_new_commits() {
        for tip in [Value::Null, json!("tip")] {
            let s = source(
                json!({"epic":{"tip":tip,"prs":[merged("main","2026-02-01")]},"main":{"tip":"main-tip","prs":[]}}),
            );
            assert_eq!(
                resolve_target(&s, &doc(), "blob").unwrap(),
                json!({"kind":"new_pr","base":"main","tip_sha":"main-tip","doc_changed":false})
            );
        }
        for prs in [
            json!([merged("main", "2026-02-01")]),
            json!([merged("main","2026-02-01"),{"state":"OPEN"}]),
        ] {
            let s = source(json!({"epic":{"tip":"new-tip","prs":prs}}));
            assert_eq!(resolve_target(&s, &doc(), "old").unwrap()["base"], "epic");
            assert_eq!(
                resolve_target(&s, &doc(), "old").unwrap()["doc_changed"],
                true
            );
        }
    }
    #[test]
    fn refuses_missing_branch_missing_doc_ambiguous_loop_and_long_chain() {
        let cases = [
            (json!({"epic":{"tip":null,"prs":[]}}), "deleted"),
            (
                json!({"epic":{"tip":"tip","prs":[merged("main","2026-02-01"),merged("other","2026-03-01")]}}),
                "more than one",
            ),
            (
                json!({"epic":{"tip":null,"prs":[merged("epic","2026-02-01")]}}),
                "loops",
            ),
        ];
        for (branches, reason) in cases {
            assert!(resolve_target(&source(branches), &doc(), "blob")
                .unwrap_err()
                .contains(reason));
        }
        let mut s = source(json!({"epic":{"tip":"tip","prs":[]}}));
        s.missing = true;
        assert!(resolve_target(&s, &doc(), "blob")
            .unwrap_err()
            .contains("not on epic"));
        s.missing = false;
        s.branches.clear();
        for i in 0..11 {
            s.branches.insert(if i==0 {"epic".into()} else {format!("b{i}")},json!({"tip":null,"prs":[merged(&format!("b{}",i+1),&format!("2026-02-{:02}",i+1))]}));
        }
        assert!(resolve_target(&s, &doc(), "blob")
            .unwrap_err()
            .contains("10 steps"));
    }
    #[test]
    fn closed_pr_requires_existing_head_branch() {
        let mut s = source(json!({"topic":{"tip":"tip","prs":[]}}));
        s.info = json!({"state":"CLOSED","headRefName":"topic"});
        assert_eq!(
            resolve_target(&s, &doc(), "blob").unwrap(),
            json!({"kind":"reopen","pr":1})
        );
        s.branches.get_mut("topic").unwrap()["tip"] = Value::Null;
        assert!(resolve_target(&s, &doc(), "blob")
            .unwrap_err()
            .contains("branch was deleted"));
    }
}
