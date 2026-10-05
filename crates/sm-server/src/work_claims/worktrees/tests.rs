//! Deletion at retire, on real git repos in a temp dir (appendix L, L9).

use super::*;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

const REPO: &str = "acme/widgets";
const RETIRED_AT: &str = "2026-01-01T00:00:00Z";

fn temp_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "sm-worktree-cleanup-{}-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&dir).unwrap();
    fs::canonicalize(dir).unwrap()
}

fn run_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A main checkout with one commit and `target/` ignored.
struct Repo {
    dir: PathBuf,
    main: PathBuf,
    store: WorkClaimStore,
}

impl Repo {
    fn new() -> Self {
        let dir = temp_dir();
        let main = dir.join("main");
        fs::create_dir_all(&main).unwrap();
        run_git(&main, &["init", "-q", "-b", "main"]);
        run_git(&main, &["config", "user.email", "t@example.com"]);
        run_git(&main, &["config", "user.name", "t"]);
        fs::write(main.join(".gitignore"), "target/\n").unwrap();
        run_git(&main, &["add", "."]);
        run_git(&main, &["commit", "-q", "-m", "init"]);
        let store = WorkClaimStore::new(dir.join("queue.db"));
        store.ensure_schema().unwrap();
        Self { dir, main, store }
    }

    /// A linked worktree on a new branch; returns its path and HEAD.
    fn worktree(&self, name: &str, branch: &str) -> (String, String) {
        let path = self.dir.join(name);
        run_git(
            &self.main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                path.to_str().unwrap(),
            ],
        );
        let path = fs::canonicalize(path).unwrap().display().to_string();
        let head = run_git(Path::new(&path), &["rev-parse", "HEAD"]);
        (path, head)
    }

    fn conn(&self) -> Connection {
        Connection::open(self.dir.join("queue.db")).unwrap()
    }

    fn merged_pr(&self, number: i64, head_ref: &str, head_sha: &str) {
        self.conn()
            .execute(
                "INSERT INTO work_items (repo, number, kind, title, state, url, head_ref, head_sha,
                                         synced_at)
                 VALUES (?1, ?2, 'pr', 'PR', 'merged', '', ?3, ?4, ?5)",
                params![REPO, number, head_ref, head_sha, RETIRED_AT],
            )
            .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn claim(
        &self,
        id: &str,
        session: &str,
        kind: &str,
        number: i64,
        path: Option<&str>,
        branch: Option<&str>,
        managed_base: Option<&str>,
    ) {
        self.conn()
            .execute(
                "INSERT INTO work_claims (id, repo, number, kind, session_id, source, worktree_path,
                     branch, claimed_at, ended_at, end_reason, managed_worktree, base_sha)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'explicit', ?6, ?7, '2025-12-31T00:00:00Z', ?8,
                         'retired', ?9, ?10)",
                params![
                    id,
                    REPO,
                    number,
                    kind,
                    session,
                    path,
                    branch,
                    RETIRED_AT,
                    managed_base.is_some() as i64,
                    managed_base
                ],
            )
            .unwrap();
    }

    fn keep(&self, path: &str, reason: &str) {
        self.store
            .set_worktree_keep(path, "keeper01", reason)
            .unwrap();
    }

    fn pass(&self, sessions: &[CleanupSession]) -> Vec<WorktreeOutcome> {
        run_worktree_cleanup(
            &self.store,
            CleanupRequest {
                sessions,
                ..CleanupRequest::default()
            },
        )
        .unwrap()
    }

    /// A pass that rechecks everything left for its content.
    fn recheck(&self, sessions: &[CleanupSession]) -> Vec<WorktreeOutcome> {
        run_worktree_cleanup(
            &self.store,
            CleanupRequest {
                sessions,
                recheck_after: Some(Duration::ZERO),
                ..CleanupRequest::default()
            },
        )
        .unwrap()
    }

    fn worktree_events(&self) -> Vec<(String, Value)> {
        self.store
            .events()
            .unwrap()
            .into_iter()
            .filter(|event| event.kind.starts_with("worktree."))
            .map(|event| (event.kind, event.payload))
            .collect()
    }

    fn branch_exists(&self, branch: &str) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(&self.main)
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .status()
            .unwrap()
            .success()
    }
}

fn session(id: &str, working_dir: &str, retired: bool) -> CleanupSession {
    CleanupSession {
        id: id.to_owned(),
        name: format!("{id}-agent"),
        working_dir: working_dir.to_owned(),
        retired,
        stopped: retired,
        retired_at: retired.then(|| RETIRED_AT.to_owned()),
        local: true,
    }
}

fn outcome(path: &str, removed: bool, reason: &str) -> WorktreeOutcome {
    WorktreeOutcome {
        path: path.to_owned(),
        removed,
        reason: reason.to_owned(),
    }
}

#[test]
fn removed_when_head_is_the_merged_head_even_with_ignored_build_output() {
    let repo = Repo::new();
    let (path, head) = repo.worktree("wt", "9-fix");
    fs::create_dir_all(Path::new(&path).join("target")).unwrap();
    fs::write(Path::new(&path).join("target/out.bin"), "x").unwrap();
    repo.merged_pr(9, "9-fix", &head);
    repo.claim("c1", "eng1", "pr", 9, Some(&path), Some("9-fix"), None);
    let sessions = [session("eng1", "/elsewhere", true)];

    assert_eq!(
        repo.pass(&sessions),
        vec![outcome(&path, true, "PR #9 merged")]
    );
    assert!(!Path::new(&path).exists());
    assert!(!repo.branch_exists("9-fix"), "branch tip was the head");
    let events = repo.worktree_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "worktree.removed");
    assert_eq!(events[0].1["path"], path);
    assert_eq!(events[0].1["branch"], "9-fix");

    // Settled: the next start does nothing more.
    assert_eq!(repo.pass(&sessions), vec![]);
    assert_eq!(repo.worktree_events().len(), 1);
}

#[test]
fn removed_for_a_managed_worktree_still_at_its_base() {
    let repo = Repo::new();
    let (path, head) = repo.worktree("wt", "5-feature");
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        5,
        Some(&path),
        Some("5-feature"),
        Some(&head),
    );
    let sessions = [session("eng1", &path, true)];
    assert_eq!(
        repo.pass(&sessions),
        vec![outcome(&path, true, "no commits")]
    );
    assert!(!repo.branch_exists("5-feature"));
}

#[test]
fn kept_with_commits_only_on_this_mac_until_they_are_pushed() {
    let repo = Repo::new();
    repo.origin();
    let (path, head) = repo.worktree("wt", "5-feature");
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        5,
        Some(&path),
        Some("5-feature"),
        Some(&head),
    );
    fs::write(Path::new(&path).join("new.txt"), "work").unwrap();
    run_git(Path::new(&path), &["add", "new.txt"]);
    run_git(Path::new(&path), &["commit", "-q", "-m", "work"]);
    let sessions = [session("eng1", "/elsewhere", true)];
    assert_eq!(
        repo.pass(&sessions),
        vec![outcome(&path, false, "1 commit only on this Mac")]
    );
    assert!(Path::new(&path).exists());
    assert_eq!(repo.pass(&sessions), vec![], "waits for the hourly recheck");
    run_git(Path::new(&path), &["push", "-q", "origin", "5-feature"]);
    assert_eq!(
        repo.recheck(&sessions),
        vec![outcome(&path, true, "pushed to origin/5-feature")]
    );
    assert!(!Path::new(&path).exists());
}

#[test]
fn kept_with_an_uncommitted_change_and_removed_once_it_is_gone() {
    let repo = Repo::new();
    let (path, head) = repo.worktree("wt", "5-feature");
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        5,
        Some(&path),
        Some("5-feature"),
        Some(&head),
    );
    fs::write(Path::new(&path).join("notes.txt"), "unsaved").unwrap();
    let sessions = [session("eng1", "/elsewhere", true)];
    assert_eq!(
        repo.pass(&sessions),
        vec![outcome(&path, false, "1 uncommitted change")]
    );
    assert!(Path::new(&path).join("notes.txt").exists());
    assert!(repo.branch_exists("5-feature"));
    // The same reason again writes nothing.
    assert_eq!(
        repo.recheck(&sessions),
        vec![outcome(&path, false, "1 uncommitted change")]
    );
    assert_eq!(repo.worktree_events().len(), 1);
    fs::remove_file(Path::new(&path).join("notes.txt")).unwrap();
    assert_eq!(
        repo.recheck(&sessions),
        vec![outcome(&path, true, "no commits")]
    );
}

#[test]
fn a_keep_holds_it_until_cleared_and_a_retry_writes_only_on_a_new_reason() {
    let repo = Repo::new();
    let (path, head) = repo.worktree("wt", "5-feature");
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        5,
        Some(&path),
        Some("5-feature"),
        Some(&head),
    );
    repo.keep(&path, "server on :8421");
    let sessions = [session("eng1", "/elsewhere", true)];
    let kept = outcome(&path, false, "kept: server on :8421");
    assert_eq!(repo.pass(&sessions), vec![kept.clone()]);
    assert_eq!(repo.pass(&sessions), vec![kept]);
    let events = repo.worktree_events();
    assert_eq!(events.len(), 1, "same reason: one event");
    assert_eq!(events[0].1["retryable"], true);

    assert!(repo.store.clear_worktree_keep(&path).unwrap());
    assert_eq!(
        repo.pass(&sessions),
        vec![outcome(&path, true, "no commits")]
    );
}

#[test]
fn a_keep_ends_when_its_path_is_gone() {
    let repo = Repo::new();
    let gone = repo.dir.join("gone").display().to_string();
    repo.keep(&gone, "results");
    repo.pass(&[]);
    assert!(repo.store.worktree_keeps().unwrap().is_empty());
}

#[test]
fn kept_while_a_process_runs_inside_then_removed_once_it_exits() {
    let repo = Repo::new();
    let (path, head) = repo.worktree("wt", "5-feature");
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        5,
        Some(&path),
        Some("5-feature"),
        Some(&head),
    );
    let mut child = Command::new("sleep")
        .arg("60")
        .current_dir(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let sessions = [session("eng1", "/elsewhere", true)];
    let outcomes = repo.pass(&sessions);
    assert_eq!(
        outcomes,
        vec![outcome(
            &path,
            false,
            &format!("process {} (sleep) runs in it", child.id())
        )]
    );
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
        repo.pass(&sessions),
        vec![outcome(&path, true, "no commits")]
    );
}

#[test]
fn kept_while_another_session_works_inside_even_a_stopped_one() {
    let repo = Repo::new();
    let (path, head) = repo.worktree("wt", "5-feature");
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        5,
        Some(&path),
        Some("5-feature"),
        Some(&head),
    );
    let mut stopped = session("fixer01", &format!("{path}/src"), false);
    stopped.stopped = true;
    let mut sessions = vec![session("eng1", "/elsewhere", true), stopped];
    assert_eq!(
        repo.pass(&sessions),
        vec![outcome(&path, false, "in use by fixer01-agent")]
    );
    // Retired too: deleted on the next pass.
    sessions[1].retired = true;
    sessions[1].retired_at = Some(RETIRED_AT.to_owned());
    assert_eq!(
        repo.pass(&sessions),
        vec![outcome(&path, true, "no commits")]
    );
}

#[test]
fn a_parent_and_child_sharing_one_worktree_make_one_candidate() {
    let repo = Repo::new();
    let (path, head) = repo.worktree("wt", "5-feature");
    repo.claim(
        "c1",
        "lead",
        "ticket",
        5,
        Some(&path),
        Some("5-feature"),
        Some(&head),
    );
    repo.claim(
        "c2",
        "kid",
        "ticket",
        5,
        Some(&path),
        Some("5-feature"),
        None,
    );
    let sessions = [session("lead", &path, true), session("kid", &path, true)];
    assert_eq!(
        repo.pass(&sessions),
        vec![outcome(&path, true, "no commits")]
    );
    assert_eq!(repo.worktree_events().len(), 1);
}

#[test]
fn a_main_checkout_is_never_deleted() {
    let repo = Repo::new();
    let main = repo.main.display().to_string();
    let head = run_git(&repo.main, &["rev-parse", "HEAD"]);
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        5,
        Some(&main),
        Some("main"),
        Some(&head),
    );
    assert_eq!(
        repo.pass(&[session("eng1", &main, true)]),
        vec![outcome(&main, false, "a main checkout")]
    );
    assert!(repo.main.join(".git").exists());
    assert_eq!(
        repo.recheck(&[session("eng1", &main, true)]),
        vec![],
        "settled"
    );
}

#[test]
fn the_branch_is_deleted_only_when_its_tip_is_the_checked_head() {
    let repo = Repo::new();
    let (path, head) = repo.worktree("wt", "9-fix");
    repo.merged_pr(9, "9-fix", &head);
    // The branch moved on after the merge; the worktree sits detached at
    // the merged head.
    run_git(
        Path::new(&path),
        &["commit", "-q", "--allow-empty", "-m", "later"],
    );
    run_git(Path::new(&path), &["checkout", "-q", "--detach", &head]);
    repo.claim("c1", "eng1", "pr", 9, Some(&path), Some("9-fix"), None);
    assert_eq!(
        repo.pass(&[session("eng1", "/elsewhere", true)]),
        vec![outcome(&path, true, "PR #9 merged")]
    );
    assert!(repo.branch_exists("9-fix"), "its tip is a later commit");
}

#[test]
fn the_checked_out_branch_is_deleted_when_the_agent_renamed_setups_branch() {
    // sm#1567: setup made `5-feature`, the agent renamed it to the repo's
    // `ticket/5-feature` convention and its PR merged from that.
    let repo = Repo::new();
    let (path, base) = repo.worktree("wt", "5-feature");
    run_git(Path::new(&path), &["branch", "-m", "ticket/5-feature"]);
    run_git(
        Path::new(&path),
        &["commit", "-q", "--allow-empty", "-m", "work"],
    );
    let head = run_git(Path::new(&path), &["rev-parse", "HEAD"]);
    repo.merged_pr(6, "ticket/5-feature", &head);
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        5,
        Some(&path),
        Some("5-feature"),
        Some(&base),
    );
    repo.claim(
        "c2",
        "eng1",
        "pr",
        6,
        Some(&path),
        Some("ticket/5-feature"),
        None,
    );
    assert_eq!(
        repo.pass(&[session("eng1", &path, true)]),
        vec![outcome(&path, true, "PR #6 merged")]
    );
    assert!(!repo.branch_exists("ticket/5-feature"));
    let events = repo.worktree_events();
    assert_eq!(events[0].1["branch"], "ticket/5-feature");
}

#[test]
fn a_detached_head_deletes_any_recorded_branch_at_that_head() {
    let repo = Repo::new();
    let (path, base) = repo.worktree("wt", "5-feature");
    run_git(Path::new(&path), &["branch", "-m", "ticket/5-feature"]);
    run_git(
        Path::new(&path),
        &["commit", "-q", "--allow-empty", "-m", "work"],
    );
    let head = run_git(Path::new(&path), &["rev-parse", "HEAD"]);
    run_git(Path::new(&path), &["checkout", "-q", "--detach", &head]);
    repo.merged_pr(6, "ticket/5-feature", &head);
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        5,
        Some(&path),
        Some("5-feature"),
        Some(&base),
    );
    repo.claim(
        "c2",
        "eng1",
        "pr",
        6,
        Some(&path),
        Some("ticket/5-feature"),
        None,
    );
    assert_eq!(
        repo.pass(&[session("eng1", "/elsewhere", true)]),
        vec![outcome(&path, true, "PR #6 merged")]
    );
    assert!(!repo.branch_exists("ticket/5-feature"));
}

#[test]
fn the_working_dir_on_a_merged_pr_branch_is_a_candidate() {
    let repo = Repo::new();
    let (path, head) = repo.worktree("wt", "9-fix");
    repo.merged_pr(9, "9-fix", &head);
    // An implicit claim: no worktree recorded.
    repo.claim("c1", "eng2", "pr", 9, None, None, None);
    assert_eq!(
        repo.pass(&[session("eng2", &format!("{path}/src"), true)]),
        vec![]
    );
    fs::create_dir_all(Path::new(&path).join("src")).unwrap();
    lock(working_dirs_done()).remove("eng2");
    assert_eq!(
        repo.pass(&[session("eng2", &format!("{path}/src"), true)]),
        vec![outcome(&path, true, "PR #9 merged")]
    );
}

#[test]
fn an_absent_path_is_settled_once_and_live_sessions_are_ignored() {
    let repo = Repo::new();
    let never = repo.dir.join("never-created").display().to_string();
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        5,
        Some(&never),
        Some("5-x"),
        Some("abc"),
    );
    repo.claim("c2", "live1", "ticket", 6, Some(&never), Some("6-x"), None);
    let sessions = [session("eng1", "/x", true), session("live1", "/x", false)];
    assert_eq!(repo.pass(&sessions), vec![outcome(&never, false, "absent")]);
    assert_eq!(repo.pass(&sessions), vec![]);
    assert_eq!(repo.worktree_events().len(), 1);
}

#[test]
fn a_crash_after_retire_leaves_the_candidate_pending_for_the_next_start() {
    let repo = Repo::new();
    let (path, head) = repo.worktree("wt", "5-feature");
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        5,
        Some(&path),
        Some("5-feature"),
        Some(&head),
    );
    // A removal recorded before this session retired does not settle it.
    write_event(
        &repo.conn(),
        REMOVED,
        Some("older"),
        Some(REPO),
        Some(5),
        None,
        json!({"path": path, "reason": "no commits"}),
        "2025-06-01T00:00:00Z",
    )
    .unwrap();
    let sessions = [session("eng1", "/elsewhere", true)];
    assert_eq!(
        repo.pass(&sessions),
        vec![outcome(&path, true, "no commits")]
    );
    assert_eq!(repo.pass(&sessions), vec![]);
}

#[test]
fn a_stopped_session_counts_as_retired_once_retire_ended_its_claims() {
    let repo = Repo::new();
    let (path, head) = repo.worktree("wt", "5-feature");
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        5,
        Some(&path),
        Some("5-feature"),
        Some(&head),
    );
    let mut stopped = session("eng1", "/elsewhere", false);
    stopped.stopped = true;
    assert_eq!(
        repo.pass(&[stopped]),
        vec![outcome(&path, true, "no commits")]
    );
}

#[test]
fn lsof_records_parse_into_pid_command_and_cwd() {
    let text = "p4121\ncnode\nfcwd\nn/Users/r/worktrees/sm-1480-queue\np77\nczsh\nfcwd\nn/tmp\n";
    assert_eq!(
        parse_lsof(text),
        vec![
            (
                4121,
                "node".to_owned(),
                "/Users/r/worktrees/sm-1480-queue".to_owned()
            ),
            (77, "zsh".to_owned(), "/tmp".to_owned()),
        ]
    );
    let listing = parse_lsof(text);
    assert_eq!(
        processes_inside("/Users/r/worktrees/sm-1480-queue", &listing),
        Some((4121, "node".to_owned()))
    );
    assert_eq!(
        processes_inside("/Users/r/worktrees/sm-1480", &listing),
        None
    );

    // The shell a server was started from is not the process to name.
    let wrapped = parse_lsof("p10\nczsh\nn/w/x\np11\ncPython\nn/w/x/sub\np12\nc-zsh\nn/w/y\n");
    assert_eq!(
        processes_inside("/w/x", &wrapped),
        Some((11, "Python".to_owned()))
    );
    assert_eq!(
        processes_inside("/w/y", &wrapped),
        Some((12, "-zsh".to_owned()))
    );
}

#[test]
fn a_failed_process_check_keeps_the_worktree_and_retries() {
    let repo = Repo::new();
    let (path, head) = repo.worktree("wt", "5-feature");
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        5,
        Some(&path),
        Some("5-feature"),
        Some(&head),
    );
    let sessions = [session("eng1", "/elsewhere", true)];
    LSOF_PROGRAM.with(|program| program.set("sm-no-such-lsof"));
    let outcomes = repo.pass(&sessions);
    LSOF_PROGRAM.with(|program| program.set("lsof"));
    assert_eq!(outcomes.len(), 1);
    assert!(!outcomes[0].removed);
    assert!(
        outcomes[0]
            .reason
            .starts_with("process check failed: lsof could not run"),
        "{}",
        outcomes[0].reason
    );
    assert!(Path::new(&path).exists());
    // Retryable: the next pass with a working lsof deletes it.
    assert_eq!(
        repo.pass(&sessions),
        vec![outcome(&path, true, "no commits")]
    );
}

#[test]
fn a_keep_set_while_the_pass_runs_is_honoured() {
    let repo = Repo::new();
    let (path, head) = repo.worktree("wt", "5-feature");
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        5,
        Some(&path),
        Some("5-feature"),
        Some(&head),
    );
    // A process inside holds the pass in its retire grace wait; meanwhile
    // another session keeps the worktree, then the process exits.
    let mut child = Command::new("sleep")
        .arg("60")
        .current_dir(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let store = repo.store.clone();
    let keep_path = path.clone();
    let keeper = thread::spawn(move || {
        thread::sleep(Duration::from_millis(1500));
        store
            .set_worktree_keep(&keep_path, "keeper01", "results not yet pushed")
            .unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    });
    let sessions = [session("eng1", "/elsewhere", true)];
    let outcomes = run_worktree_cleanup(
        &repo.store,
        CleanupRequest {
            sessions: &sessions,
            first: Some("eng1"),
            ..CleanupRequest::default()
        },
    )
    .unwrap();
    keeper.join().unwrap();
    assert_eq!(
        outcomes,
        vec![outcome(&path, false, "kept: results not yet pushed")]
    );
    assert!(Path::new(&path).exists());
}

/// sm#1987: work continued on a later branch and the worktree was left
/// detached at a commit that branch, pushed, contains.
#[test]
fn removed_when_a_detached_head_is_on_a_later_remote_branch() {
    let repo = Repo::new();
    repo.origin();
    let (path, _) = repo.worktree("wt", "1748-first");
    let dir = Path::new(&path);
    fs::write(dir.join("a.txt"), "a").unwrap();
    run_git(dir, &["add", "a.txt"]);
    run_git(dir, &["commit", "-q", "-m", "a"]);
    let head = run_git(dir, &["rev-parse", "HEAD"]);
    run_git(dir, &["checkout", "-q", "-b", "1748-chain-compaction"]);
    fs::write(dir.join("b.txt"), "b").unwrap();
    run_git(dir, &["add", "b.txt"]);
    run_git(dir, &["commit", "-q", "-m", "b"]);
    run_git(dir, &["push", "-q", "origin", "1748-chain-compaction"]);
    run_git(dir, &["checkout", "-q", "--detach", &head]);
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        1748,
        Some(&path),
        Some("1748-first"),
        None,
    );
    assert_eq!(
        repo.pass(&[session("eng1", "/elsewhere", true)]),
        vec![outcome(
            &path,
            true,
            "pushed to origin/1748-chain-compaction"
        )]
    );
    assert!(!dir.exists());
    assert!(
        repo.branch_exists("1748-chain-compaction"),
        "not the checked head"
    );
}

/// sm#1987: another agent claimed and merged the PR whose head this is.
#[test]
fn removed_when_head_is_a_pr_another_agent_merged() {
    let repo = Repo::new();
    let (path, _) = repo.worktree("wt", "1771-fix");
    let dir = Path::new(&path);
    fs::write(dir.join("a.txt"), "a").unwrap();
    run_git(dir, &["add", "a.txt"]);
    run_git(dir, &["commit", "-q", "-m", "a"]);
    let head = run_git(dir, &["rev-parse", "HEAD"]);
    repo.merged_pr(1797, "1797-revision", &head);
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        1771,
        Some(&path),
        Some("1771-fix"),
        None,
    );
    repo.claim("c2", "reviser", "pr", 1797, None, None, None);
    assert_eq!(
        repo.pass(&[session("eng1", "/elsewhere", true)]),
        vec![outcome(&path, true, "PR #1797 merged")]
    );
}

/// sm#1987: a keep naming a PR ends when the PR merges.
#[test]
fn a_keep_naming_a_pr_expires_once_it_merges() {
    let repo = Repo::new();
    let (path, head) = repo.worktree("wt", "1787-memo");
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        1787,
        Some(&path),
        Some("1787-memo"),
        Some(&head),
    );
    repo.keep(&path, "handoff: memo PR #1789 mid-review");
    let sessions = [session("eng1", "/elsewhere", true)];
    assert_eq!(
        repo.pass(&sessions),
        vec![outcome(
            &path,
            false,
            "kept: handoff: memo PR #1789 mid-review"
        )]
    );
    repo.merged_pr(
        1789,
        "1789-memo",
        "0000000000000000000000000000000000000000",
    );
    assert_eq!(
        repo.pass(&sessions),
        vec![outcome(&path, true, "no commits")]
    );
    assert!(repo.store.worktree_keeps().unwrap().is_empty());
    let expired = repo
        .worktree_events()
        .into_iter()
        .find(|(kind, _)| kind == "worktree.keep_expired")
        .expect("keep_expired event");
    assert_eq!(expired.1["prs"], json!([1789]));
}

#[test]
fn keep_reasons_name_prs_by_number_or_link() {
    assert_eq!(keep_prs("handoff: memo PR #1789 mid-review"), vec![1789]);
    assert_eq!(keep_prs("pr#12 and PR 13"), vec![12, 13]);
    assert_eq!(
        keep_prs("see https://github.com/acme/widgets/pull/44"),
        vec![44]
    );
    assert!(keep_prs("server on :8421 for ticket #12").is_empty());
}

/// sm#1987: a worktree whose removal git refused is checked again, and a
/// populated `target/` goes first.
#[test]
fn a_refused_removal_is_rechecked_and_build_output_goes_first() {
    let repo = Repo::new();
    let (path, head) = repo.worktree("wt", "1709-fix");
    let dir = Path::new(&path);
    fs::create_dir_all(dir.join("target/debug/deps")).unwrap();
    fs::write(dir.join("target/debug/deps/lib.rlib"), "x").unwrap();
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        1709,
        Some(&path),
        Some("1709-fix"),
        Some(&head),
    );
    // A lock makes git refuse without --force.
    run_git(&repo.main, &["worktree", "lock", &path]);
    let sessions = [session("eng1", "/elsewhere", true)];
    let outcomes = repo.pass(&sessions);
    assert!(
        !outcomes[0].removed && outcomes[0].reason.starts_with("git refused: "),
        "{outcomes:?}"
    );
    assert!(!dir.join("target").exists(), "build output cleared first");
    run_git(&repo.main, &["worktree", "unlock", &path]);
    assert_eq!(
        repo.recheck(&sessions),
        vec![outcome(&path, true, "no commits")]
    );
}

/// Records written before sm#1987 settled a worktree for good; it is now
/// checked again.
#[test]
fn an_old_final_record_is_checked_again() {
    let repo = Repo::new();
    let (path, head) = repo.worktree("wt", "1705-fix");
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        1705,
        Some(&path),
        Some("1705-fix"),
        Some(&head),
    );
    write_event(
        &repo.conn(),
        LEFT,
        Some("eng1"),
        Some(REPO),
        Some(1705),
        None,
        json!({"path": path, "reason": "commits not in a merged PR"}),
        "2026-01-02T00:00:00Z",
    )
    .unwrap();
    assert_eq!(
        repo.pass(&[session("eng1", "/elsewhere", true)]),
        vec![outcome(&path, true, "no commits")]
    );
}

#[test]
fn leftovers_list_and_delete_build_output_or_the_worktree() {
    let repo = Repo::new();
    let (path, head) = repo.worktree("wt", "1854-fix");
    let dir = Path::new(&path);
    fs::create_dir_all(dir.join("target")).unwrap();
    fs::write(dir.join("target/out.bin"), "x").unwrap();
    fs::write(dir.join("notes.txt"), "unsaved").unwrap();
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        1854,
        Some(&path),
        Some("1854-fix"),
        Some(&head),
    );
    let sessions = [session("eng1", "/elsewhere", true)];
    repo.pass(&sessions);
    let leftovers = leftover_worktrees(&repo.store, &sessions).unwrap();
    assert_eq!(leftovers.len(), 1);
    assert_eq!(leftovers[0].path, path);
    assert_eq!(leftovers[0].ticket, Some(1854));
    assert_eq!(leftovers[0].sessions, vec!["eng1-agent".to_owned()]);
    assert_eq!(leftovers[0].reason, "1 uncommitted change");
    assert_eq!(leftover_build_dirs(&path), vec![dir.join("target")]);

    let refused = delete_leftover(
        &repo.store,
        &sessions,
        "/elsewhere/x",
        DeleteScope::Worktree,
        "owner",
    )
    .unwrap();
    assert_eq!(
        refused,
        Err("/elsewhere/x is not a left-over worktree".to_owned())
    );
    let busy = [
        session("eng1", "/elsewhere", true),
        session("live1", &path, false),
    ];
    assert_eq!(
        delete_leftover(&repo.store, &busy, &path, DeleteScope::Worktree, "owner").unwrap(),
        Err("in use by live1-agent".to_owned())
    );

    let build = delete_leftover(
        &repo.store,
        &sessions,
        &path,
        DeleteScope::BuildOutput,
        "owner",
    )
    .unwrap()
    .unwrap();
    assert!(!build.removed);
    assert_eq!(
        build.cleared,
        vec![dir.join("target").display().to_string()]
    );
    assert!(dir.join("notes.txt").exists());

    let deleted = delete_leftover(
        &repo.store,
        &sessions,
        &path,
        DeleteScope::Worktree,
        "owner",
    )
    .unwrap()
    .unwrap();
    assert!(deleted.removed);
    assert!(!dir.exists());
    assert!(
        repo.branch_exists("1854-fix"),
        "the branch keeps its commits"
    );
    assert!(leftover_worktrees(&repo.store, &sessions)
        .unwrap()
        .is_empty());
    assert_eq!(repo.recheck(&sessions), vec![], "settled by the removal");
}

/// A folder git no longer knows as a worktree (a removal that died half
/// way) is listed, and Delete removes it.
#[test]
fn a_folder_that_is_no_longer_a_worktree_is_listed_and_deletable() {
    let repo = Repo::new();
    let (path, head) = repo.worktree("wt", "1709-fix");
    repo.claim(
        "c1",
        "eng1",
        "ticket",
        1709,
        Some(&path),
        Some("1709-fix"),
        Some(&head),
    );
    fs::remove_file(Path::new(&path).join(".git")).unwrap();
    run_git(&repo.main, &["worktree", "prune"]);
    let sessions = [session("eng1", "/elsewhere", true)];
    assert_eq!(
        repo.pass(&sessions),
        vec![outcome(&path, false, "not a git worktree")]
    );
    assert_eq!(
        leftover_worktrees(&repo.store, &sessions).unwrap()[0].reason,
        "not a git worktree"
    );
    assert!(
        delete_leftover(
            &repo.store,
            &sessions,
            &path,
            DeleteScope::Worktree,
            "owner"
        )
        .unwrap()
        .unwrap()
        .removed
    );
    assert!(!Path::new(&path).exists());
}

impl Repo {
    /// A bare `origin` holding `main`, fetched into the main checkout.
    fn origin(&self) -> PathBuf {
        let origin = self.dir.join("origin.git");
        run_git(
            &self.dir,
            &["init", "-q", "--bare", origin.to_str().unwrap()],
        );
        run_git(
            &self.main,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        run_git(&self.main, &["push", "-q", "origin", "main"]);
        run_git(&self.main, &["fetch", "-q", "origin"]);
        origin
    }

    fn rebuild(&self, session: &str, path: &str) -> Result<WorktreeRebuild, String> {
        rebuild_worktree(&self.store, session, path, &[])
    }
}

#[test]
fn restore_rebuilds_a_worktree_deleted_by_hand_at_the_same_path_on_its_branch() {
    let repo = Repo::new();
    let (path, _) = repo.worktree("wt", "7-fix");
    repo.claim(
        "c1",
        "restorer1",
        "ticket",
        7,
        Some(&path),
        Some("7-fix"),
        None,
    );
    fs::remove_dir_all(&path).unwrap();
    // No removal recorded the repository: the main checkout is found by
    // its origin, which here is not on GitHub, so only the event path works.
    assert_eq!(
        repo.rebuild("restorer1", &path),
        Err(format!(
            "worktree {path} is gone and no checkout of {REPO} is known"
        ))
    );
    repo.pass(&[session("restorer1", &path, true)]);
    assert!(repo
        .worktree_events()
        .iter()
        .all(|(kind, _)| kind != "worktree.removed"));
    // A removal by the cleanup pass records the repository instead.
    let (path2, head2) = repo.worktree("wt2", "8-fix");
    repo.merged_pr(8, "8-fix", &head2);
    repo.claim(
        "c2",
        "restorer2",
        "pr",
        8,
        Some(&path2),
        Some("8-fix"),
        None,
    );
    repo.pass(&[session("restorer2", "/elsewhere", true)]);
    run_git(&repo.main, &["branch", "8-fix", &head2]);
    assert_eq!(
        repo.rebuild("restorer2", &path2),
        Ok(WorktreeRebuild::Rebuilt {
            branch: "8-fix".to_owned()
        })
    );
    assert_eq!(run_git(Path::new(&path2), &["rev-parse", "HEAD"]), head2);
    assert_eq!(
        run_git(Path::new(&path2), &["branch", "--show-current"]),
        "8-fix"
    );
    assert_eq!(
        repo.rebuild("restorer2", &path2),
        Ok(WorktreeRebuild::Present)
    );
}

#[test]
fn restore_fetches_a_branch_cleanup_deleted_or_detaches_when_origin_lost_it_too() {
    let repo = Repo::new();
    repo.origin();
    let (path, head) = repo.worktree("wt", "9-fix");
    run_git(&repo.main, &["push", "-q", "origin", "9-fix"]);
    repo.merged_pr(9, "9-fix", &head);
    repo.claim("c1", "restorer3", "pr", 9, Some(&path), Some("9-fix"), None);
    repo.pass(&[session("restorer3", "/elsewhere", true)]);
    assert!(!Path::new(&path).exists());
    assert!(!repo.branch_exists("9-fix"));
    let events = repo.worktree_events();
    assert_eq!(
        events[0].1["git_common_dir"],
        path_key(&repo.main.join(".git").display().to_string())
    );

    assert_eq!(
        repo.rebuild("restorer3", &path),
        Ok(WorktreeRebuild::Rebuilt {
            branch: "9-fix".to_owned()
        })
    );
    assert_eq!(
        run_git(Path::new(&path), &["branch", "--show-current"]),
        "9-fix"
    );
    assert_eq!(run_git(Path::new(&path), &["rev-parse", "HEAD"]), head);

    // Merged and deleted on origin too: detached at origin's main, and the
    // timeline says so.
    run_git(&repo.main, &["push", "-q", "origin", "--delete", "9-fix"]);
    run_git(&repo.main, &["worktree", "remove", &path]);
    run_git(&repo.main, &["branch", "-D", "9-fix"]);
    assert_eq!(
        repo.rebuild("restorer3", &path),
        Ok(WorktreeRebuild::Detached {
            branch: "9-fix".to_owned(),
            base: "origin/main".to_owned()
        })
    );
    assert_eq!(run_git(Path::new(&path), &["branch", "--show-current"]), "");
    let rebuilt: Vec<_> = repo
        .worktree_events()
        .into_iter()
        .filter(|(kind, _)| kind == "worktree.rebuilt")
        .collect();
    assert_eq!(rebuilt.len(), 2);
    assert_eq!(rebuilt[1].1["detached_at"], "origin/main");
}

#[test]
fn restore_refuses_a_missing_working_dir_no_claim_recorded() {
    let repo = Repo::new();
    let path = repo.dir.join("never").display().to_string();
    assert_eq!(
        repo.rebuild("restorer3", &path),
        Err(format!(
            "worktree {path} is gone and has no branch to rebuild from"
        ))
    );
}

#[test]
fn restore_uses_the_branch_the_agent_renamed_to_and_refuses_when_origin_is_unreachable() {
    let repo = Repo::new();
    let origin = repo.origin();
    let (path, _) = repo.worktree("wt", "11-fix");
    repo.claim(
        "c1",
        "restorer4",
        "pr",
        11,
        Some(&path),
        Some("11-fix"),
        None,
    );
    // The agent renamed setup's branch and pushed it under the new name.
    run_git(Path::new(&path), &["branch", "-m", "11-fix-v2"]);
    run_git(Path::new(&path), &["push", "-q", "origin", "11-fix-v2"]);
    let head = run_git(Path::new(&path), &["rev-parse", "HEAD"]);
    repo.merged_pr(11, "11-fix-v2", &head);
    repo.pass(&[session("restorer4", "/elsewhere", true)]);
    assert!(!repo.branch_exists("11-fix-v2"));

    // Origin unreachable: not proof the branch is gone, so no detached
    // rebuild and no worktree.
    let moved = repo.dir.join("origin-moved.git");
    fs::rename(&origin, &moved).unwrap();
    let refused = repo.rebuild("restorer4", &path).unwrap_err();
    assert!(
        refused.starts_with(&format!(
            "rebuilding worktree {path} failed: cannot read origin"
        )),
        "{refused}"
    );
    assert!(!Path::new(&path).exists());

    fs::rename(&moved, &origin).unwrap();
    assert_eq!(
        repo.rebuild("restorer4", &path),
        Ok(WorktreeRebuild::Rebuilt {
            branch: "11-fix-v2".to_owned()
        })
    );
    assert_eq!(
        run_git(Path::new(&path), &["branch", "--show-current"]),
        "11-fix-v2"
    );
}
