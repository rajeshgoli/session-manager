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
fn kept_when_head_has_commits_not_in_a_merged_pr_and_that_is_final() {
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
    fs::write(Path::new(&path).join("new.txt"), "work").unwrap();
    run_git(Path::new(&path), &["add", "new.txt"]);
    run_git(Path::new(&path), &["commit", "-q", "-m", "work"]);
    let sessions = [session("eng1", "/elsewhere", true)];
    assert_eq!(
        repo.pass(&sessions),
        vec![outcome(&path, false, "commits not in a merged PR")]
    );
    assert!(Path::new(&path).exists());
    assert_eq!(repo.pass(&sessions), vec![], "final: not retried");
}

#[test]
fn kept_by_git_with_an_untracked_file() {
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
    let outcomes = repo.pass(&[session("eng1", "/elsewhere", true)]);
    assert_eq!(outcomes.len(), 1);
    assert!(!outcomes[0].removed);
    assert!(
        outcomes[0].reason.starts_with("git refused: ")
            && outcomes[0].reason.contains("modified or untracked files"),
        "{}",
        outcomes[0].reason
    );
    assert!(Path::new(&path).join("notes.txt").exists());
    assert!(repo.branch_exists("5-feature"));
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
        vec![outcome(&main, false, "not a linked worktree")]
    );
    assert!(repo.main.join(".git").exists());
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
            progress: None,
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
