//! `--setup-worktree` on real git repos, and the keep and retire output.

use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

fn temp_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = env::temp_dir().join(format!(
        "sm-setup-worktree-{}-{}-{}",
        process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&dir).unwrap();
    fs::canonicalize(dir).unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    run_ok(&ProcessTools, "git", dir, args).unwrap()
}

/// An origin with one commit on `main`, cloned into `checkout`; origin then
/// gains a second commit the clone has not fetched.
struct Checkout {
    dir: PathBuf,
    checkout: PathBuf,
    origin_head: String,
}

fn checkout() -> Checkout {
    let dir = temp_dir();
    let seed = dir.join("seed");
    fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q", "-b", "main"]);
    git(&seed, &["config", "user.email", "t@example.com"]);
    git(&seed, &["config", "user.name", "t"]);
    git(&seed, &["commit", "-q", "--allow-empty", "-m", "one"]);
    let origin = dir.join("origin.git");
    git(
        &dir,
        &[
            "clone",
            "-q",
            "--bare",
            seed.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    let checkout = dir.join("checkout");
    git(
        &dir,
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            checkout.to_str().unwrap(),
        ],
    );
    git(
        &seed,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&seed, &["commit", "-q", "--allow-empty", "-m", "two"]);
    git(&seed, &["push", "-q", "origin", "main"]);
    let origin_head = git(&seed, &["rev-parse", "HEAD"]);
    Checkout {
        dir,
        checkout,
        origin_head,
    }
}

fn claim(root: &Path, recorded: Option<WorktreePlan>) -> SetupClaim {
    SetupClaim {
        claim_id: "c0ffee01".into(),
        number: 1452,
        title: "Agent work claims (sm ticket / sm pr) and an agent history page".into(),
        recorded,
        root: root.to_path_buf(),
        prefix: "sm".into(),
    }
}

#[derive(Default)]
struct Recorded {
    calls: Vec<(String, PathBuf, String, String)>,
    fail_intent: bool,
}

impl WorktreeRecorder for Recorded {
    fn record(&mut self, state: &str, plan: &WorktreePlan, base_sha: Option<&str>) -> Result<()> {
        if self.fail_intent && state == "intent" {
            bail!("HTTP 502");
        }
        self.calls.push((
            state.to_owned(),
            plan.path.clone(),
            plan.branch.clone(),
            base_sha.unwrap_or("").to_owned(),
        ));
        Ok(())
    }
}

#[test]
fn slug_and_path_rules_including_the_prefix() {
    assert_eq!(
        title_slug("Agent work claims (sm ticket / sm pr) and an agent history page"),
        "agent-work-claims-sm"
    );
    assert_eq!(
        title_slug("Supercalifragilistic expialidocious words"),
        "supercalifragilistic-expialido"
    );
    let plan = plan_worktree(
        1452,
        "Agent work claims (sm ticket / sm pr) and an agent history page",
        None,
        Path::new("/home/r/worktrees"),
        "sm",
    )
    .unwrap();
    assert_eq!(
        plan,
        WorktreePlan {
            path: PathBuf::from("/home/r/worktrees/sm-1452-agent-work-claims-sm"),
            branch: "1452-agent-work-claims-sm".into(),
        }
    );
    let given = plan_worktree(
        7,
        "x",
        Some("Spec Fix"),
        Path::new("/w"),
        "fractal-algo-rust",
    )
    .unwrap();
    assert_eq!(given.path, PathBuf::from("/w/fractal-algo-rust-7-spec-fix"));
    assert_eq!(given.branch, "7-spec-fix");
    assert!(plan_worktree(8, "!!!", None, Path::new("/w"), "sm").is_err());
}

#[test]
fn creates_the_branch_from_origin_default_records_base_and_reuses_on_rerun() {
    let c = checkout();
    let root = c.dir.join("worktrees");
    let mut recorder = Recorded::default();
    let outcome = setup_worktree(
        &ProcessTools,
        &c.checkout,
        &claim(&root, None),
        None,
        None,
        &mut recorder,
    )
    .unwrap();
    let path = root.join("sm-1452-agent-work-claims-sm");
    let SetupOutcome::Created {
        plan,
        base,
        base_sha,
    } = &outcome
    else {
        panic!("{outcome:?}");
    };
    assert_eq!(plan.path, path);
    assert_eq!(base, "origin/main");
    assert_eq!(*base_sha, c.origin_head, "fetched before branching");
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), c.origin_head);
    assert_eq!(
        git(&path, &["branch", "--show-current"]),
        "1452-agent-work-claims-sm"
    );
    let states = recorder
        .calls
        .iter()
        .map(|c| c.0.as_str())
        .collect::<Vec<_>>();
    assert_eq!(states, ["intent", "created"]);
    assert_eq!(recorder.calls[0].3, c.origin_head);
    assert!(outcome.line().starts_with("Worktree "));
    assert!(outcome.line().ends_with(&format!(
        "on branch 1452-agent-work-claims-sm from origin/main ({}).",
        &c.origin_head[..7]
    )));

    // Rerun, with the worktree now recorded on the claim: reused.
    let recorded = Some(plan.clone());
    let again = setup_worktree(
        &ProcessTools,
        &c.checkout,
        &claim(&root, recorded.clone()),
        None,
        None,
        &mut Recorded::default(),
    )
    .unwrap();
    assert_eq!(again, SetupOutcome::Existing { plan: plan.clone() });
    // Rerun without the record (a lost confirmation): the path is this
    // ticket's worktree, so it is reused too. A reuse sends no base: origin
    // may have moved since, and the worktree's base is what was recorded
    // when it was created.
    let mut rerun = Recorded::default();
    let again = setup_worktree(
        &ProcessTools,
        &c.checkout,
        &claim(&root, None),
        None,
        None,
        &mut rerun,
    )
    .unwrap();
    assert_eq!(again, SetupOutcome::Existing { plan: plan.clone() });
    assert_eq!(rerun.calls.len(), 2);
    assert!(
        rerun.calls.iter().all(|call| call.3.is_empty()),
        "{:?}",
        rerun.calls
    );
    // A different slug on a claim that has a worktree is refused.
    let error = setup_worktree(
        &ProcessTools,
        &c.checkout,
        &claim(&root, recorded),
        Some("other"),
        None,
        &mut Recorded::default(),
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .starts_with("This claim already has worktree "));
}

#[test]
fn an_explicit_base_and_an_existing_local_branch() {
    let c = checkout();
    let root = c.dir.join("worktrees");
    let first = git(&c.checkout, &["rev-parse", "HEAD"]);
    git(
        &c.checkout,
        &["branch", "1452-agent-work-claims-sm", &first],
    );
    let outcome = setup_worktree(
        &ProcessTools,
        &c.checkout,
        &claim(&root, None),
        None,
        Some("origin/main"),
        &mut Recorded::default(),
    )
    .unwrap();
    let SetupOutcome::Created { plan, .. } = outcome else {
        panic!()
    };
    assert_eq!(
        git(&plan.path, &["rev-parse", "HEAD"]),
        first,
        "the branch is checked out"
    );
}

#[test]
fn a_foreign_existing_path_is_refused_and_creates_nothing() {
    let c = checkout();
    let root = c.dir.join("worktrees");
    fs::create_dir_all(root.join("sm-1452-agent-work-claims-sm")).unwrap();
    let mut recorder = Recorded::default();
    let error = setup_worktree(
        &ProcessTools,
        &c.checkout,
        &claim(&root, None),
        None,
        None,
        &mut recorder,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .ends_with("exists and is not this ticket's worktree."),
        "{error}"
    );
    assert!(recorder.calls.is_empty());
}

#[test]
fn a_failed_intent_creates_nothing_and_a_git_failure_leaves_the_intent() {
    let c = checkout();
    let root = c.dir.join("worktrees");
    let mut recorder = Recorded {
        fail_intent: true,
        ..Recorded::default()
    };
    assert!(setup_worktree(
        &ProcessTools,
        &c.checkout,
        &claim(&root, None),
        None,
        None,
        &mut recorder
    )
    .is_err());
    assert!(!root.join("sm-1452-agent-work-claims-sm").exists());

    let mut recorder = Recorded::default();
    let error = setup_worktree(
        &ProcessTools,
        &c.checkout,
        &claim(&root, None),
        None,
        Some("no-such-ref"),
        &mut recorder,
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Base no-such-ref is not a commit in this repo."
    );

    // git itself refuses: the branch is checked out in the main checkout.
    git(
        &c.checkout,
        &["checkout", "-q", "-b", "1452-agent-work-claims-sm"],
    );
    let mut recorder = Recorded::default();
    assert!(setup_worktree(
        &ProcessTools,
        &c.checkout,
        &claim(&root, None),
        None,
        None,
        &mut recorder
    )
    .is_err());
    let states = recorder
        .calls
        .iter()
        .map(|c| c.0.as_str())
        .collect::<Vec<_>>();
    assert_eq!(states, ["intent"], "the claim keeps the intent");
}

#[test]
fn keep_and_retire_output() {
    let home = env::var("HOME").unwrap();
    let body =
        json!({"path": format!("{home}/worktrees/x"), "kept": true, "reason": "server on :8421"});
    assert_eq!(
        keep_output(200, &body).stdout,
        vec!["Keeping ~/worktrees/x: server on :8421."]
    );
    let body = json!({"path": format!("{home}/worktrees/x"), "kept": false});
    assert_eq!(
        keep_output(200, &body).stdout,
        vec!["No longer keeping ~/worktrees/x."]
    );
    assert_eq!(
        keep_output(400, &json!({"detail": "path must be absolute"})).exit,
        1
    );

    let payload = json!({"status": "retired", "worktrees": [
        {"path": format!("{home}/worktrees/sm-1449-owner-docs"), "removed": true, "reason": "PR #1460 merged"},
        {"path": format!("{home}/worktrees/sm-1480-queue"), "removed": false, "reason": "process 4121 (node) runs in it"},
        {"path": "/w/sm-9-x", "removed": true, "reason": "no commits"},
        {"path": "/w/sm-10-y", "removed": false, "reason": "absent"},
        {"path": "/w/sm-11-z", "removed": false, "reason": "cleanup pending"},
    ]});
    assert_eq!(
        retire_worktree_lines(&payload),
        vec![
            "Deleted worktree ~/worktrees/sm-1449-owner-docs (PR #1460 merged).",
            "Kept worktree ~/worktrees/sm-1480-queue: process 4121 (node) runs in it.",
            "Deleted worktree /w/sm-9-x (no commits).",
            "Kept worktree /w/sm-11-z: cleanup pending.",
        ]
    );
    assert!(retire_worktree_lines(&json!({"status": "retired"})).is_empty());
}
