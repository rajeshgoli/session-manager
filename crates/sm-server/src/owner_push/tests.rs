use std::sync::Mutex;

use super::*;

fn temp_store() -> (OwnerPushStore, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "sm-owner-push-{}-{}",
        std::process::id(),
        OsRng.next_u64()
    ));
    fs::create_dir_all(&dir).unwrap();
    (OwnerPushStore::new(dir.join("owner_push.db")), dir)
}

fn at(value: &str) -> OffsetDateTime {
    parse_ts(value).unwrap()
}

const OWNER: &str = "owner@example.com";

fn session_target(id: &str) -> FollowTarget {
    FollowTarget::Session {
        session_id: id.to_owned(),
        session_name: format!("{id}-name"),
    }
}

fn job_target(id: &str) -> FollowTarget {
    FollowTarget::QueueJob {
        job_id: id.to_owned(),
        job_label: format!("{id}-label"),
        session_id: "agent1".to_owned(),
        session_name: "agent1-name".to_owned(),
    }
}

#[derive(Default)]
struct FakeWorld {
    sessions: Vec<SessionView>,
    jobs: BTreeMap<String, JobView>,
    reports: BTreeMap<String, Vec<ReportView>>,
}

impl FollowWorld for FakeWorld {
    fn sessions(&self) -> Result<Vec<SessionView>> {
        Ok(self.sessions.clone())
    }
    fn job(&self, job_id: &str) -> Result<Option<JobView>> {
        Ok(self.jobs.get(job_id).cloned())
    }
    fn reports(&self, session_id: &str) -> Result<Vec<ReportView>> {
        Ok(self.reports.get(session_id).cloned().unwrap_or_default())
    }
}

fn live(id: &str) -> SessionView {
    SessionView {
        id: id.to_owned(),
        name: format!("{id}-name"),
        stopped: false,
        task_completed_at: None,
    }
}

fn job(id: &str, state: &str) -> JobView {
    JobView {
        id: id.to_owned(),
        label: format!("{id}-label"),
        state: state.to_owned(),
        exit_code: None,
        started_at: Some("2026-09-25T10:00:00Z".to_owned()),
        finished_at: Some("2026-09-25T12:14:30Z".to_owned()),
    }
}

fn report(title: &str, published_at: &str) -> ReportView {
    ReportView {
        doc_id: format!("doc-{title}"),
        title: title.to_owned(),
        reader_path: format!("/docs/widgets/{title}.html?version=abc"),
        published_at: published_at.to_owned(),
    }
}

type SentPush = (String, BTreeMap<String, String>);

/// Records sends; each token answers with its scripted errors in order,
/// then success.
#[derive(Default)]
struct FakeSender {
    sent: Mutex<Vec<SentPush>>,
    script: Mutex<BTreeMap<String, Vec<PushError>>>,
}

impl FakeSender {
    fn failing(token: &str, errors: Vec<PushError>) -> Self {
        let sender = Self::default();
        sender
            .script
            .lock()
            .unwrap()
            .insert(token.to_owned(), errors);
        sender
    }
    fn sent(&self) -> Vec<SentPush> {
        self.sent.lock().unwrap().clone()
    }
}

impl PushSender for FakeSender {
    fn send(&self, token: &str, data: &BTreeMap<String, String>) -> Result<(), PushError> {
        if let Some(errors) = self.script.lock().unwrap().get_mut(token) {
            if !errors.is_empty() {
                return Err(errors.remove(0));
            }
        }
        self.sent
            .lock()
            .unwrap()
            .push((token.to_owned(), data.clone()));
        Ok(())
    }
}

#[derive(Default)]
struct FakeMailer {
    sent: Mutex<Vec<(String, Notification)>>,
    /// Email not set up at all.
    unavailable: bool,
    /// Sends that fail transiently before one succeeds.
    transient_failures: Mutex<usize>,
}

impl FollowMailer for FakeMailer {
    fn send(&self, follow: &Follow, notification: &Notification) -> Result<(), MailError> {
        if self.unavailable {
            return Err(MailError::Unavailable("no bridge".to_owned()));
        }
        let mut failures = self.transient_failures.lock().unwrap();
        if *failures > 0 {
            *failures -= 1;
            return Err(MailError::Transient("resend 503".to_owned()));
        }
        self.sent
            .lock()
            .unwrap()
            .push((follow.id.clone(), notification.clone()));
        Ok(())
    }
}

fn register(store: &OwnerPushStore, token: &str, device_id: Option<&str>) {
    store
        .upsert_token(
            &PushTokenRegistration {
                user_id: OWNER.to_owned(),
                token: token.to_owned(),
                device_id: device_id.map(ToOwned::to_owned),
                device_name: format!("{token}-phone"),
                app_version: "abc".to_owned(),
            },
            at("2026-09-25T09:00:00Z"),
        )
        .unwrap();
}

fn fired_follow(
    store: &OwnerPushStore,
    target: FollowTarget,
    created: &str,
    fired: &str,
    reason: &str,
) -> Follow {
    let (follow, _) = store
        .create_follow(OWNER, &target, None, at(created))
        .unwrap();
    store.fire(&follow.id, reason, at(fired), None).unwrap();
    store.get(&follow.id).unwrap().unwrap()
}

fn fired_job(store: &OwnerPushStore, id: &str) -> Follow {
    fired_follow(
        store,
        job_target(id),
        "2026-09-25T10:00:00Z",
        "2026-09-25T11:00:00Z",
        REASON_JOB_FINISHED,
    )
}

#[test]
fn follow_create_is_idempotent_per_target_and_refollows_after_firing() {
    let (store, dir) = temp_store();
    let now = at("2026-09-25T10:00:00Z");
    let (first, created) = store
        .create_follow(OWNER, &session_target("agent1"), Some("hi"), now)
        .unwrap();
    assert!(created);
    assert!(first.id.starts_with("fol_") && first.id.len() == 16);
    assert_eq!(first.state(), "active");
    let (again, created) = store
        .create_follow(OWNER, &session_target("agent1"), Some("other"), now)
        .unwrap();
    assert!(!created);
    assert_eq!(again.id, first.id);
    assert_eq!(again.message_text.as_deref(), Some("hi"));

    // An agent follow and a job follow of the same agent are independent.
    let (job_follow, created) = store
        .create_follow(OWNER, &job_target("job1"), None, now)
        .unwrap();
    assert!(created && job_follow.id != first.id);
    assert_eq!(job_follow.session_id, "agent1");

    assert!(store
        .fire(&first.id, REASON_TASK_COMPLETE, now, None)
        .unwrap());
    assert!(!store
        .fire(&first.id, REASON_SESSION_ENDED, now, None)
        .unwrap());
    let (next, created) = store
        .create_follow(OWNER, &session_target("agent1"), None, now)
        .unwrap();
    assert!(created && next.id != first.id);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn cancel_ends_only_an_active_follow() {
    let (store, dir) = temp_store();
    let now = at("2026-09-25T10:00:00Z");
    let (follow, _) = store
        .create_follow(OWNER, &session_target("agent1"), None, now)
        .unwrap();
    assert!(store
        .cancel_active(OWNER, TARGET_SESSION, "agent1", now)
        .unwrap());
    assert_eq!(store.get(&follow.id).unwrap().unwrap().state(), "cancelled");
    assert!(!store
        .cancel_active(OWNER, TARGET_SESSION, "agent1", now)
        .unwrap());

    let fired = fired_job(&store, "job1");
    assert!(!store
        .cancel_active(OWNER, TARGET_QUEUE_JOB, "job1", now)
        .unwrap());
    assert_eq!(store.get(&fired.id).unwrap().unwrap().state(), "fired");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn task_complete_hook_fires_only_active_agent_follows_of_the_session() {
    let (store, dir) = temp_store();
    let now = at("2026-09-25T10:00:00Z");
    let (agent, _) = store
        .create_follow(OWNER, &session_target("agent1"), None, now)
        .unwrap();
    let (job_follow, _) = store
        .create_follow(OWNER, &job_target("job1"), None, now)
        .unwrap();
    let (other, _) = store
        .create_follow(OWNER, &session_target("agent2"), None, now)
        .unwrap();
    assert_eq!(
        store
            .fire_task_complete("agent1", "2026-09-25T11:00:00.123456Z")
            .unwrap(),
        1
    );
    let agent = store.get(&agent.id).unwrap().unwrap();
    assert_eq!(agent.fired_at.as_deref(), Some("2026-09-25T11:00:00Z"));
    assert_eq!(agent.notify_after.as_deref(), Some("2026-09-25T11:00:00Z"));
    assert_eq!(agent.fire_reason.as_deref(), Some(REASON_TASK_COMPLETE));
    assert!(store.get(&job_follow.id).unwrap().unwrap().is_active());
    assert!(store.get(&other.id).unwrap().unwrap().is_active());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn sweep_fires_session_ended_for_stopped_or_missing_sessions() {
    let (store, dir) = temp_store();
    let now = at("2026-09-25T10:00:00Z");
    for id in ["alive", "stopped", "gone"] {
        store
            .create_follow(OWNER, &session_target(id), None, now)
            .unwrap();
    }
    let mut stopped = live("stopped");
    stopped.stopped = true;
    let world = FakeWorld {
        sessions: vec![live("alive"), stopped],
        ..Default::default()
    };
    let later = at("2026-09-25T10:05:00Z");
    assert_eq!(sweep(&store, &world, later).unwrap(), 2);
    let follows = store.list_for_owner(OWNER, later).unwrap();
    let by_session = |id: &str| {
        follows
            .iter()
            .find(|follow| follow.session_id == id)
            .unwrap()
            .clone()
    };
    assert!(by_session("alive").is_active());
    for id in ["stopped", "gone"] {
        let follow = by_session(id);
        assert_eq!(follow.fire_reason.as_deref(), Some(REASON_SESSION_ENDED));
        assert_eq!(follow.fired_at.as_deref(), Some("2026-09-25T10:05:00Z"));
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn sweep_catches_a_missed_task_complete_but_not_one_before_the_follow() {
    let (store, dir) = temp_store();
    let now = at("2026-09-25T10:00:00Z");
    store
        .create_follow(OWNER, &session_target("after"), None, now)
        .unwrap();
    store
        .create_follow(OWNER, &session_target("before"), None, now)
        .unwrap();
    let mut after = live("after");
    after.task_completed_at = Some("2026-09-25T10:02:00Z".to_owned());
    let mut before = live("before");
    before.task_completed_at = Some("2026-09-25T09:59:00Z".to_owned());
    let world = FakeWorld {
        sessions: vec![after, before],
        ..Default::default()
    };
    assert_eq!(
        sweep(&store, &world, at("2026-09-25T10:03:00Z")).unwrap(),
        1
    );
    let follows = store.list_for_owner(OWNER, now).unwrap();
    let fired = follows
        .iter()
        .find(|follow| follow.session_id == "after")
        .unwrap();
    assert_eq!(fired.fire_reason.as_deref(), Some(REASON_TASK_COMPLETE));
    assert_eq!(fired.fired_at.as_deref(), Some("2026-09-25T10:02:00Z"));
    assert!(follows
        .iter()
        .find(|follow| follow.session_id == "before")
        .unwrap()
        .is_active());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn sweep_fires_job_finished_for_each_terminal_state() {
    let (store, dir) = temp_store();
    let now = at("2026-09-25T10:00:00Z");
    let states = [
        "succeeded",
        "failed",
        "timed_out",
        "cancelled",
        "displaced",
        "memory_exceeded",
    ];
    let mut world = FakeWorld::default();
    for state in states {
        store
            .create_follow(OWNER, &job_target(state), None, now)
            .unwrap();
        world.jobs.insert(state.to_owned(), job(state, state));
    }
    store
        .create_follow(OWNER, &job_target("running"), None, now)
        .unwrap();
    world
        .jobs
        .insert("running".to_owned(), job("running", "running"));
    store
        .create_follow(OWNER, &job_target("vanished"), None, now)
        .unwrap();

    assert_eq!(sweep(&store, &world, now).unwrap(), states.len() + 1);
    let follows = store.list_for_owner(OWNER, now).unwrap();
    let by_job = |id: &str| {
        follows
            .iter()
            .find(|follow| follow.job_id.as_deref() == Some(id))
            .unwrap()
            .clone()
    };
    for state in states {
        let follow = by_job(state);
        assert_eq!(follow.fire_reason.as_deref(), Some(REASON_JOB_FINISHED));
        assert_eq!(follow.job_state.as_deref(), Some(state));
        assert_eq!(
            follow.job_finished_at.as_deref(),
            Some("2026-09-25T12:14:30Z")
        );
    }
    assert_eq!(by_job("vanished").job_state.as_deref(), Some("unknown"));
    assert!(by_job("running").is_active());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn report_lookup_takes_the_newest_publish_since_the_follow() {
    let (store, dir) = temp_store();
    let follow = fired_follow(
        &store,
        session_target("agent1"),
        "2026-09-25T10:00:00Z",
        "2026-09-25T11:00:00Z",
        REASON_TASK_COMPLETE,
    );
    let reports = |list: Vec<ReportView>| FakeWorld {
        reports: BTreeMap::from([("agent1".to_owned(), list)]),
        ..Default::default()
    };
    let world = reports(vec![
        report("newest", "2026-09-25T10:50:00Z"),
        report("older", "2026-09-25T10:30:00Z"),
        report("before-follow", "2026-09-25T09:00:00Z"),
    ]);
    assert_eq!(
        find_report(&world, &follow).unwrap().unwrap().title,
        "newest"
    );
    let only_before = reports(vec![report("before-follow", "2026-09-25T09:59:59.900Z")]);
    assert!(find_report(&only_before, &follow).unwrap().is_none());
    // The follow row stores whole seconds; a publish in the same second counts.
    let same_second = reports(vec![report("same-second", "2026-09-25T10:00:00.400Z")]);
    assert!(find_report(&same_second, &follow).unwrap().is_some());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn report_grace_waits_then_sends_without_a_report() {
    let (store, dir) = temp_store();
    register(&store, "tok1", None);
    let follow = fired_follow(
        &store,
        session_target("agent1"),
        "2026-09-25T10:00:00Z",
        "2026-09-25T11:00:00Z",
        REASON_TASK_COMPLETE,
    );
    let mut world = FakeWorld {
        sessions: vec![live("agent1")],
        ..Default::default()
    };
    let sender = FakeSender::default();
    let mailer = FakeMailer::default();

    deliver(
        &store,
        &world,
        Some(&sender),
        &mailer,
        at("2026-09-25T11:00:05Z"),
    )
    .unwrap();
    assert!(sender.sent().is_empty());
    let waiting = store.get(&follow.id).unwrap().unwrap();
    assert_eq!(
        waiting.notify_after.as_deref(),
        Some("2026-09-25T11:02:00Z")
    );
    assert_eq!(waiting.state(), "fired");

    // Not due before the grace ends.
    deliver(
        &store,
        &world,
        Some(&sender),
        &mailer,
        at("2026-09-25T11:01:00Z"),
    )
    .unwrap();
    assert!(sender.sent().is_empty());

    // A report published inside the grace is picked up.
    world.reports.insert(
        "agent1".to_owned(),
        vec![report("late", "2026-09-25T11:00:40Z")],
    );
    deliver(
        &store,
        &world,
        Some(&sender),
        &mailer,
        at("2026-09-25T11:02:00Z"),
    )
    .unwrap();
    let sent = sender.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1["title"], "agent1-name finished");
    assert_eq!(sent[0].1["body"], "Report: late");
    assert_eq!(
        sent[0].1["reader_path"],
        "/docs/widgets/late.html?version=abc"
    );
    assert_eq!(sent[0].1["follow_id"], follow.id);
    let done = store.get(&follow.id).unwrap().unwrap();
    assert_eq!(done.state(), "notified");
    assert_eq!(done.notified_via.as_deref(), Some("push"));
    assert_eq!(done.report_title.as_deref(), Some("late"));

    // Without any report the send happens once the grace is over.
    let bare = fired_follow(
        &store,
        session_target("agent2"),
        "2026-09-25T10:00:00Z",
        "2026-09-25T11:00:00Z",
        REASON_TASK_COMPLETE,
    );
    deliver(
        &store,
        &world,
        Some(&sender),
        &mailer,
        at("2026-09-25T11:03:00Z"),
    )
    .unwrap();
    let sent = sender.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].1["body"], "No completion report published");
    assert!(!sent[1].1.contains_key("reader_path"));
    assert_eq!(store.get(&bare.id).unwrap().unwrap().state(), "notified");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn push_retry_schedule_then_email() {
    let (store, dir) = temp_store();
    register(&store, "tok1", None);
    let sender = FakeSender::failing(
        "tok1",
        (0..6)
            .map(|_| PushError::Retryable("503".to_owned()))
            .collect(),
    );
    let mailer = FakeMailer::default();
    let world = FakeWorld::default();
    let follow = fired_job(&store, "job1");
    let mut now = at("2026-09-25T11:00:00Z");
    for (attempt, delay) in [30_i64, 60, 120, 300, 600].into_iter().enumerate() {
        let attempts = i64::try_from(attempt).unwrap() + 1;
        deliver(&store, &world, Some(&sender), &mailer, now).unwrap();
        let current = store.get(&follow.id).unwrap().unwrap();
        assert_eq!(current.push_attempts, attempts);
        assert_eq!(
            current.notify_after.clone().unwrap(),
            format_ts(now + Duration::seconds(delay))
        );
        assert_eq!(current.state(), "fired");
        // Not due one second early.
        deliver(
            &store,
            &world,
            Some(&sender),
            &mailer,
            now + Duration::seconds(delay - 1),
        )
        .unwrap();
        assert_eq!(
            store.get(&follow.id).unwrap().unwrap().push_attempts,
            attempts
        );
        now += Duration::seconds(delay);
    }
    deliver(&store, &world, Some(&sender), &mailer, now).unwrap();
    let emailed = store.get(&follow.id).unwrap().unwrap();
    assert_eq!(emailed.push_attempts, 6);
    assert_eq!(emailed.notified_via.as_deref(), Some("email"));
    assert_eq!(mailer.sent.lock().unwrap().len(), 1);
    assert!(sender.sent().is_empty());
    // Retryable errors leave the token usable.
    assert_eq!(store.valid_tokens(OWNER).unwrap().len(), 1);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unregistered_token_is_invalidated_and_the_other_device_still_gets_it() {
    let (store, dir) = temp_store();
    register(&store, "dead", None);
    register(&store, "good", None);
    let sender = FakeSender::failing("dead", vec![PushError::InvalidToken("404".to_owned())]);
    let mailer = FakeMailer::default();
    let follow = fired_job(&store, "job1");
    deliver(
        &store,
        &FakeWorld::default(),
        Some(&sender),
        &mailer,
        at("2026-09-25T11:00:00Z"),
    )
    .unwrap();
    assert_eq!(sender.sent().len(), 1);
    assert_eq!(sender.sent()[0].0, "good");
    let tokens = store.valid_tokens(OWNER).unwrap();
    assert_eq!(
        tokens
            .iter()
            .map(|token| token.token.as_str())
            .collect::<Vec<_>>(),
        ["good"]
    );
    assert_eq!(
        store
            .get(&follow.id)
            .unwrap()
            .unwrap()
            .notified_via
            .as_deref(),
        Some("push")
    );

    // Registering the token again revives it.
    register(&store, "dead", None);
    assert_eq!(store.valid_tokens(OWNER).unwrap().len(), 2);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn all_tokens_invalid_or_absent_or_push_unconfigured_goes_to_email() {
    let (store, dir) = temp_store();
    let mailer = FakeMailer::default();
    let sender = FakeSender::failing("dead", vec![PushError::InvalidToken("gone".to_owned())]);
    let world = FakeWorld::default();
    let now = at("2026-09-25T11:00:00Z");
    let via =
        |store: &OwnerPushStore, id: &str| store.get(id).unwrap().unwrap().notified_via.unwrap();

    // No tokens at all.
    let first = fired_job(&store, "a");
    deliver(&store, &world, Some(&sender), &mailer, now).unwrap();
    assert_eq!(via(&store, &first.id), "email");

    // Only an invalid token.
    register(&store, "dead", None);
    let second = fired_job(&store, "b");
    deliver(&store, &world, Some(&sender), &mailer, now).unwrap();
    assert_eq!(via(&store, &second.id), "email");

    // Push not configured: email straight away even with a valid token.
    register(&store, "good", None);
    let third = fired_job(&store, "c");
    deliver(&store, &world, None, &mailer, now).unwrap();
    let third = store.get(&third.id).unwrap().unwrap();
    assert_eq!(third.notified_via.as_deref(), Some("email"));
    assert_eq!(third.email_sent_at.as_deref(), Some("2026-09-25T11:00:00Z"));
    assert_eq!(mailer.sent.lock().unwrap().len(), 3);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn no_channel_marks_notified_instead_of_looping() {
    let (store, dir) = temp_store();
    let mailer = FakeMailer {
        unavailable: true,
        ..Default::default()
    };
    let follow = fired_job(&store, "a");
    let problems = deliver(
        &store,
        &FakeWorld::default(),
        None,
        &mailer,
        at("2026-09-25T11:00:00Z"),
    )
    .unwrap();
    assert_eq!(problems.len(), 1);
    let follow = store.get(&follow.id).unwrap().unwrap();
    assert_eq!(follow.notified_via.as_deref(), Some("email"));
    assert_eq!(follow.last_push_error.as_deref(), Some("no channel"));
    // Nothing is pending any more.
    assert!(deliver(
        &store,
        &FakeWorld::default(),
        None,
        &mailer,
        at("2026-09-25T12:00:00Z")
    )
    .unwrap()
    .is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn ack_fallback_emails_once_after_fifteen_minutes() {
    let (store, dir) = temp_store();
    register(&store, "tok1", None);
    let sender = FakeSender::default();
    let mailer = FakeMailer::default();
    let world = FakeWorld::default();
    let unacked = fired_job(&store, "a");
    let acked = fired_job(&store, "b");
    deliver(
        &store,
        &world,
        Some(&sender),
        &mailer,
        at("2026-09-25T11:00:00Z"),
    )
    .unwrap();
    assert_eq!(sender.sent().len(), 2);
    assert!(store
        .ack(OWNER, &acked.id, at("2026-09-25T11:00:10Z"))
        .unwrap());
    assert!(!store
        .ack("someone@else", &acked.id, at("2026-09-25T11:00:10Z"))
        .unwrap());
    assert_eq!(store.get(&acked.id).unwrap().unwrap().state(), "acked");

    for now in [
        "2026-09-25T11:14:59Z",
        "2026-09-25T11:15:00Z",
        "2026-09-25T11:30:00Z",
    ] {
        deliver(&store, &world, Some(&sender), &mailer, at(now)).unwrap();
    }
    let emails = mailer.sent.lock().unwrap().clone();
    assert_eq!(emails.len(), 1);
    assert_eq!(emails[0].0, unacked.id);
    assert_eq!(emails[0].1.title, "a-label ended");
    assert_eq!(
        store
            .get(&unacked.id)
            .unwrap()
            .unwrap()
            .email_sent_at
            .as_deref(),
        Some("2026-09-25T11:15:00Z")
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn list_for_owner_hides_cancelled_old_and_other_owners() {
    let (store, dir) = temp_store();
    let now = at("2026-09-25T10:00:00Z");
    let (active, _) = store
        .create_follow(OWNER, &session_target("a"), None, now)
        .unwrap();
    let recent = fired_follow(
        &store,
        session_target("b"),
        "2026-09-24T10:00:00Z",
        "2026-09-24T11:00:00Z",
        REASON_TASK_COMPLETE,
    );
    fired_follow(
        &store,
        session_target("c"),
        "2026-09-10T10:00:00Z",
        "2026-09-10T11:00:00Z",
        REASON_TASK_COMPLETE,
    );
    store
        .create_follow(OWNER, &session_target("d"), None, now)
        .unwrap();
    store
        .cancel_active(OWNER, TARGET_SESSION, "d", now)
        .unwrap();
    store
        .create_follow("other@example.com", &session_target("e"), None, now)
        .unwrap();
    let listed = store.list_for_owner(OWNER, now).unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|follow| follow.id.as_str())
            .collect::<Vec<_>>(),
        [active.id.as_str(), recent.id.as_str()]
    );
    let json = serde_json::to_value(&listed[0]).unwrap();
    assert!(json.get("user_id").is_none() && json.get("message_text").is_none());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn revoking_a_device_deletes_its_tokens() {
    let (store, dir) = temp_store();
    register(&store, "tok1", Some("android-1"));
    register(&store, "tok2", Some("android-2"));
    assert_eq!(store.delete_device_tokens("android-1").unwrap(), 1);
    let tokens = store.valid_tokens(OWNER).unwrap();
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].device_id.as_deref(), Some("android-2"));
    store.delete_token(OWNER, "tok2").unwrap();
    assert!(store.valid_tokens(OWNER).unwrap().is_empty());
    fs::remove_dir_all(dir).unwrap();
}

fn fired_job_follow(
    state: &str,
    exit_code: Option<i64>,
    started: Option<&str>,
    finished: Option<&str>,
) -> Follow {
    Follow {
        id: "fol_x".to_owned(),
        user_id: OWNER.to_owned(),
        target_kind: TARGET_QUEUE_JOB.to_owned(),
        session_id: "agent1".to_owned(),
        session_name: "1679-engineer".to_owned(),
        job_id: Some("job_1".to_owned()),
        job_label: Some("1679-stage1-prepare-2".to_owned()),
        job_state: Some(state.to_owned()),
        job_exit_code: exit_code,
        job_started_at: started.map(ToOwned::to_owned),
        job_finished_at: finished.map(ToOwned::to_owned),
        message_text: None,
        created_at: "2026-09-25T09:00:00Z".to_owned(),
        cancelled_at: None,
        fired_at: Some("2026-09-25T12:14:30Z".to_owned()),
        fire_reason: Some(REASON_JOB_FINISHED.to_owned()),
        report_doc_id: None,
        report_title: None,
        report_reader_path: None,
        notify_after: None,
        push_attempts: 0,
        last_push_error: None,
        notified_at: None,
        notified_via: None,
        acked_at: None,
        email_sent_at: None,
    }
}

#[test]
fn job_notification_text() {
    let started = Some("2026-09-25T10:00:00Z");
    let finished = Some("2026-09-25T12:14:30Z");
    let cases = [
        ("succeeded", None, "1679-stage1-prepare-2 succeeded"),
        ("failed", Some(1), "1679-stage1-prepare-2 failed (exit 1)"),
        ("failed", None, "1679-stage1-prepare-2 failed"),
        ("timed_out", None, "1679-stage1-prepare-2 timed out"),
        ("cancelled", None, "1679-stage1-prepare-2 was cancelled"),
        ("displaced", None, "1679-stage1-prepare-2 was displaced"),
        ("unknown", None, "1679-stage1-prepare-2 ended"),
        (
            "memory_exceeded",
            None,
            "1679-stage1-prepare-2 ended (memory exceeded)",
        ),
    ];
    for (state, exit_code, title) in cases {
        let notification = notification_for(&fired_job_follow(state, exit_code, started, finished));
        assert_eq!(notification.title, title);
        assert_eq!(notification.body, "1679-engineer · ran 2h 14m");
        assert_eq!(notification.kind, REASON_JOB_FINISHED);
    }
    let never_started = fired_job_follow("cancelled", None, None, finished);
    let notification = notification_for(&never_started);
    assert_eq!(notification.body, "1679-engineer");
    let data = notification.data(Some(&never_started));
    assert_eq!(data["job_id"], "job_1");
    assert_eq!(data["kind"], "job_finished");
    assert_eq!(data["session_id"], "agent1");
}

#[test]
fn agent_notification_text() {
    let mut follow = fired_job_follow("succeeded", None, None, None);
    follow.target_kind = TARGET_SESSION.to_owned();
    follow.job_id = None;
    follow.session_name = "sm-1569-engineer".to_owned();
    follow.fire_reason = Some(REASON_SESSION_ENDED.to_owned());
    let ended = notification_for(&follow);
    assert_eq!(ended.title, "sm-1569-engineer ended without completing");
    assert_eq!(ended.body, "Session stopped before sm task-complete");
    follow.report_title = Some("Follow — completion".to_owned());
    follow.report_reader_path = Some("/docs/x?version=1".to_owned());
    assert_eq!(
        notification_for(&follow).body,
        "Report: Follow — completion"
    );
    follow.fire_reason = Some(REASON_TASK_COMPLETE.to_owned());
    let done = notification_for(&follow);
    assert_eq!(done.title, "sm-1569-engineer finished");
    assert_eq!(done.body, "Report: Follow — completion");
    let data = done.data(Some(&follow));
    assert_eq!(data["reader_path"], "/docs/x?version=1");
    assert!(!data.contains_key("job_id"));
}

#[test]
fn durations_use_the_largest_two_units() {
    assert_eq!(format_duration(Duration::seconds(35)), "35s");
    assert_eq!(
        format_duration(Duration::minutes(41) + Duration::seconds(3)),
        "41m 3s"
    );
    assert_eq!(
        format_duration(Duration::hours(2) + Duration::minutes(14) + Duration::seconds(30)),
        "2h 14m"
    );
    assert_eq!(
        format_duration(Duration::hours(1) + Duration::seconds(5)),
        "1h"
    );
    assert_eq!(
        format_duration(Duration::days(1) + Duration::hours(3)),
        "1d 3h"
    );
    assert_eq!(format_duration(Duration::ZERO), "0s");
}

#[test]
fn follow_message_marker_is_added_once() {
    assert_eq!(
        follow_message_text("  please report  "),
        "[sm follow] please report"
    );
    assert_eq!(
        follow_message_text("[sm follow] already"),
        "[sm follow] already"
    );
}

#[test]
fn test_push_reports_failures_and_invalidates_dead_tokens() {
    let (store, dir) = temp_store();
    register(&store, "good", None);
    register(&store, "dead", None);
    let sender = FakeSender::failing("dead", vec![PushError::InvalidToken("gone".to_owned())]);
    let (sent, failed) =
        send_test(&store, &sender, OWNER, "mac", at("2026-09-25T11:00:00Z")).unwrap();
    assert_eq!(sent, 1);
    assert_eq!(
        failed,
        vec![("dead-phone".to_owned(), "invalid token: gone".to_owned())]
    );
    assert_eq!(sender.sent()[0].1["title"], "sm notifications work");
    assert_eq!(sender.sent()[0].1["body"], "Sent from mac");
    assert_eq!(store.valid_tokens(OWNER).unwrap().len(), 1);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn transient_email_failure_retries_within_the_hour() {
    let (store, dir) = temp_store();
    let world = FakeWorld::default();
    let mailer = FakeMailer {
        transient_failures: Mutex::new(2),
        ..Default::default()
    };
    let follow = fired_job(&store, "a");
    let problems = deliver(&store, &world, None, &mailer, at("2026-09-25T11:00:00Z")).unwrap();
    assert_eq!(problems.len(), 1);
    let waiting = store.get(&follow.id).unwrap().unwrap();
    assert_eq!(waiting.state(), "fired");
    assert_eq!(
        waiting.notify_after.as_deref(),
        Some("2026-09-25T11:05:00Z")
    );
    // Not retried before its time.
    deliver(&store, &world, None, &mailer, at("2026-09-25T11:04:59Z")).unwrap();
    assert_eq!(*mailer.transient_failures.lock().unwrap(), 1);
    deliver(&store, &world, None, &mailer, at("2026-09-25T11:05:00Z")).unwrap();
    deliver(&store, &world, None, &mailer, at("2026-09-25T11:10:00Z")).unwrap();
    let sent = store.get(&follow.id).unwrap().unwrap();
    assert_eq!(sent.notified_via.as_deref(), Some("email"));
    assert_eq!(mailer.sent.lock().unwrap().len(), 1);

    // Past the retry window the follow is closed out instead of retried forever.
    let late = fired_job(&store, "b");
    *mailer.transient_failures.lock().unwrap() = 1;
    deliver(&store, &world, None, &mailer, at("2026-09-25T12:00:00Z")).unwrap();
    let late = store.get(&late.id).unwrap().unwrap();
    assert_eq!(late.state(), "notified");
    assert_eq!(
        late.last_push_error.as_deref(),
        Some("email failed: resend 503")
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn transient_fallback_email_failure_retries_on_its_own_schedule() {
    let (store, dir) = temp_store();
    register(&store, "tok1", None);
    let sender = FakeSender::default();
    let world = FakeWorld::default();
    let mailer = FakeMailer {
        transient_failures: Mutex::new(1),
        ..Default::default()
    };
    let follow = fired_job(&store, "a");
    deliver(
        &store,
        &world,
        Some(&sender),
        &mailer,
        at("2026-09-25T11:00:00Z"),
    )
    .unwrap();
    deliver(
        &store,
        &world,
        Some(&sender),
        &mailer,
        at("2026-09-25T11:15:00Z"),
    )
    .unwrap();
    assert!(store
        .get(&follow.id)
        .unwrap()
        .unwrap()
        .email_sent_at
        .is_none());
    deliver(
        &store,
        &world,
        Some(&sender),
        &mailer,
        at("2026-09-25T11:16:00Z"),
    )
    .unwrap();
    assert!(mailer.sent.lock().unwrap().is_empty());
    deliver(
        &store,
        &world,
        Some(&sender),
        &mailer,
        at("2026-09-25T11:20:00Z"),
    )
    .unwrap();
    assert_eq!(mailer.sent.lock().unwrap().len(), 1);
    assert_eq!(
        store
            .get(&follow.id)
            .unwrap()
            .unwrap()
            .email_sent_at
            .as_deref(),
        Some("2026-09-25T11:20:00Z")
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn concurrent_follows_of_one_target_all_return_the_same_follow() {
    let (store, dir) = temp_store();
    let path = dir.join("owner_push.db");
    store.valid_tokens(OWNER).unwrap(); // creates the schema
    let ids = std::thread::scope(|scope| {
        let handles = (0..8)
            .map(|_| {
                let path = path.clone();
                scope.spawn(move || {
                    OwnerPushStore::new(path)
                        .create_follow(
                            OWNER,
                            &session_target("agent1"),
                            None,
                            at("2026-09-25T10:00:00Z"),
                        )
                        .unwrap()
                        .0
                        .id
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(ids.iter().all(|id| id == &ids[0]), "{ids:?}");
    fs::remove_dir_all(dir).unwrap();
}

// ---------------------------------------------------------------------------
// Owner notices (sm#1580).

/// Subjects the owner has answered, unread counts, and repair candidates.
#[derive(Default)]
struct FakeNoticeWorld {
    answered: Mutex<std::collections::BTreeSet<String>>,
    opened: Mutex<std::collections::BTreeSet<String>>,
    unread: i64,
    candidates: Vec<NewNotice>,
}

impl FakeNoticeWorld {
    fn answer(&self, subject_id: &str) {
        self.answered.lock().unwrap().insert(subject_id.to_owned());
    }
    fn open(&self, subject_id: &str) {
        self.opened.lock().unwrap().insert(subject_id.to_owned());
    }
}

impl NoticeWorld for FakeNoticeWorld {
    fn still_wanted(&self, notice: &Notice) -> Result<bool> {
        Ok(!self.answered.lock().unwrap().contains(&notice.subject_id))
    }
    fn opened(&self, notice: &Notice) -> Result<bool> {
        Ok(self.opened.lock().unwrap().contains(&notice.subject_id))
    }
    fn unread_count(&self, _notice: &Notice) -> Result<i64> {
        Ok(self.unread)
    }
    fn notice_candidates(&self, _since: OffsetDateTime) -> Result<Vec<NewNotice>> {
        Ok(self.candidates.clone())
    }
}

#[derive(Default)]
struct FakeNoticeMailer {
    sent: Mutex<Vec<String>>,
    unavailable: bool,
    transient_failures: Mutex<usize>,
}

impl NoticeMailer for FakeNoticeMailer {
    fn send(&self, notice: &Notice) -> Result<(), MailError> {
        if self.unavailable {
            return Err(MailError::Unavailable("no bridge".to_owned()));
        }
        let mut failures = self.transient_failures.lock().unwrap();
        if *failures > 0 {
            *failures -= 1;
            return Err(MailError::Transient("resend 503".to_owned()));
        }
        self.sent.lock().unwrap().push(notice.id.clone());
        Ok(())
    }
}

fn message_notice(message_id: &str, blocking: bool) -> NewNotice {
    NewNotice::message(
        OWNER,
        "eng00001",
        "sm-1679-engineer",
        message_id,
        "Keep the old fills table or drop it?",
        blocking,
    )
}

fn review_notice(publish_id: i64) -> NewNotice {
    NewNotice::review_requested(
        OWNER,
        "eng00001",
        "sm-1679-engineer",
        publish_id,
        "Decision memo",
        "/docs/widgets/memo.html?version=aaaaaaaaaaaa",
    )
}

fn notice_of(store: &OwnerPushStore, kind: &str, subject: &str) -> Notice {
    store.notice_for_subject(kind, subject).unwrap().unwrap()
}

#[test]
fn message_creates_notice() {
    let (store, dir) = temp_store();
    let now = at("2026-09-26T10:00:00Z");
    assert!(store
        .create_notice(&message_notice("msg_00000001", true), now)
        .unwrap());
    assert!(!store
        .create_notice(&message_notice("msg_00000001", true), now)
        .unwrap());
    assert!(store.create_notice(&review_notice(7), now).unwrap());
    let notice = notice_of(&store, NOTICE_MESSAGE, "msg_00000001");
    assert!(
        notice.id.starts_with("not_") && notice.id.len() == 16,
        "{}",
        notice.id
    );
    assert_eq!(notice.created_at, "2026-09-26T10:00:00Z");
    assert_eq!(notice.notify_after, notice.created_at);
    assert_eq!(notice.reader_path, "/messages/msg_00000001");
    assert_eq!(store.list_notices(OWNER, now).unwrap().len(), 2);
    assert!(store
        .list_notices(OWNER, now + Duration::days(8))
        .unwrap()
        .is_empty());
    assert!(store
        .list_notices("other@example.com", now)
        .unwrap()
        .is_empty());
    // Acks are the owner's only.
    assert!(!store
        .ack_notice("other@example.com", &notice.id, now)
        .unwrap());
    assert!(store.ack_notice(OWNER, &notice.id, now).unwrap());
    assert!(store
        .notice(&notice.id)
        .unwrap()
        .unwrap()
        .acked_at
        .is_some());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn repair_pass_creates_missing_notices() {
    let (store, dir) = temp_store();
    let now = at("2026-09-26T10:00:00Z");
    store
        .create_notice(&message_notice("msg_00000001", false), now)
        .unwrap();
    let world = FakeNoticeWorld {
        candidates: vec![
            message_notice("msg_00000001", false),
            message_notice("msg_00000002", true),
            review_notice(9),
        ],
        ..Default::default()
    };
    assert_eq!(repair_notices(&store, &world, now).unwrap(), 2);
    assert_eq!(repair_notices(&store, &world, now).unwrap(), 0);
    assert_eq!(store.list_notices(OWNER, now).unwrap().len(), 3);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn create_and_repair_race_leaves_one_notice() {
    let (store, dir) = temp_store();
    let db = dir.join("owner_push.db");
    store
        .create_notice(&review_notice(1), at("2026-09-26T10:00:00Z"))
        .unwrap();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let db = db.clone();
            std::thread::spawn(move || {
                OwnerPushStore::new(db)
                    .create_notice(
                        &message_notice("msg_00000001", true),
                        OffsetDateTime::now_utc(),
                    )
                    .unwrap()
            })
        })
        .collect();
    let created = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .filter(|created| *created)
        .count();
    assert_eq!(created, 1);
    let rows: i64 = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM owner_notices WHERE subject_id = 'msg_00000001'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn shown_notices_are_withdrawn_once_answered_or_opened() {
    let (store, dir) = temp_store();
    register(&store, "tok1", None);
    let now = at("2026-09-26T10:00:00Z");
    let world = FakeNoticeWorld::default();
    let mailer = FakeNoticeMailer::default();
    let withdrawals = |sender: &FakeSender| {
        sender
            .sent()
            .into_iter()
            .filter(|(_, data)| data["kind"] == NOTICE_WITHDRAW)
            .map(|(_, data)| data["notice_id"].clone())
            .collect::<Vec<_>>()
    };
    for subject in ["msg_answered", "msg_opened", "msg_waiting", "msg_unshown"] {
        store
            .create_notice(&message_notice(subject, false), now)
            .unwrap();
    }
    let sender = FakeSender::default();
    deliver_notices(&store, &world, Some(&sender), &mailer, now).unwrap();
    // The phone showed all but one.
    for subject in ["msg_answered", "msg_opened", "msg_waiting"] {
        let notice = notice_of(&store, NOTICE_MESSAGE, subject);
        assert!(store.ack_notice(OWNER, &notice.id, now).unwrap());
    }

    // Nothing answered or opened yet: nothing withdrawn.
    let sender = FakeSender::default();
    withdraw_notices(&store, &world, Some(&sender), now).unwrap();
    assert!(sender.sent().is_empty());

    world.answer("msg_answered");
    world.open("msg_opened");
    world.answer("msg_unshown");
    let flaky = FakeSender::failing("tok1", vec![PushError::Retryable("503".to_owned())]);
    withdraw_notices(&store, &world, Some(&flaky), now).unwrap();
    // The first withdrawal failed and is retried; the unshown notice has nothing to take down.
    assert_eq!(
        withdrawals(&flaky),
        vec![notice_of(&store, NOTICE_MESSAGE, "msg_opened").id]
    );
    withdraw_notices(&store, &world, Some(&flaky), now).unwrap();
    assert_eq!(
        withdrawals(&flaky),
        vec![
            notice_of(&store, NOTICE_MESSAGE, "msg_opened").id,
            notice_of(&store, NOTICE_MESSAGE, "msg_answered").id,
        ]
    );
    // Each goes once.
    withdraw_notices(&store, &world, Some(&flaky), now).unwrap();
    assert_eq!(withdrawals(&flaky).len(), 2);
    // Opened before the first push: never sent, so nothing to withdraw.
    store
        .create_notice(&message_notice("msg_early", false), now)
        .unwrap();
    world.open("msg_early");
    let quiet = FakeSender::default();
    deliver_notices(&store, &world, Some(&quiet), &mailer, now).unwrap();
    assert!(quiet.sent().is_empty());
    assert_eq!(
        notice_of(&store, NOTICE_MESSAGE, "msg_early")
            .notified_via
            .as_deref(),
        Some("resolved")
    );
    // Without a push channel there is nothing to do.
    world.open("msg_waiting");
    withdraw_notices(&store, &world, None, now).unwrap();
    withdraw_notices(&store, &world, Some(&flaky), now).unwrap();
    assert_eq!(withdrawals(&flaky).len(), 3);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn resolved_subject_closes_notice_without_sending() {
    let (store, dir) = temp_store();
    register(&store, "tok1", None);
    let now = at("2026-09-26T10:00:00Z");
    let world = FakeNoticeWorld::default();
    let mailer = FakeNoticeMailer::default();

    // Answered before the first push: closed, nothing sent.
    store
        .create_notice(&message_notice("msg_replied", true), now)
        .unwrap();
    world.answer("msg_replied");
    let sender = FakeSender::default();
    deliver_notices(&store, &world, Some(&sender), &mailer, now).unwrap();
    let closed = notice_of(&store, NOTICE_MESSAGE, "msg_replied");
    assert_eq!(closed.notified_via.as_deref(), Some("resolved"));
    assert_eq!(closed.notified_at.as_deref(), Some("2026-09-26T10:00:00Z"));
    assert!(sender.sent().is_empty());

    // Answered while a push is retrying: the retry doesn't happen.
    store.create_notice(&review_notice(3), now).unwrap();
    let flaky = FakeSender::failing("tok1", vec![PushError::Retryable("503".to_owned())]);
    deliver_notices(&store, &world, Some(&flaky), &mailer, now).unwrap();
    assert_eq!(
        notice_of(&store, NOTICE_REVIEW_REQUESTED, "3").push_attempts,
        1
    );
    world.answer("3");
    deliver_notices(
        &store,
        &world,
        Some(&flaky),
        &mailer,
        now + Duration::seconds(30),
    )
    .unwrap();
    assert_eq!(
        notice_of(&store, NOTICE_REVIEW_REQUESTED, "3")
            .notified_via
            .as_deref(),
        Some("resolved")
    );
    assert!(flaky.sent().is_empty());

    // Answered after the push, before the 15-minute fallback: no email.
    store
        .create_notice(&message_notice("msg_handled", true), now)
        .unwrap();
    let sender = FakeSender::default();
    deliver_notices(&store, &world, Some(&sender), &mailer, now).unwrap();
    assert_eq!(sender.sent().len(), 1);
    world.answer("msg_handled");
    deliver_notices(&store, &world, Some(&sender), &mailer, now + ACK_FALLBACK).unwrap();
    let pushed = notice_of(&store, NOTICE_MESSAGE, "msg_handled");
    assert_eq!(pushed.notified_via.as_deref(), Some("resolved"));
    assert_eq!(pushed.notified_at.as_deref(), Some("2026-09-26T10:00:00Z"));
    assert!(pushed.email_sent_at.is_none());
    // Answered with push unconfigured: no email either.
    store
        .create_notice(&message_notice("msg_nopush", false), now)
        .unwrap();
    world.answer("msg_nopush");
    deliver_notices(&store, &world, None, &mailer, now + Duration::hours(1)).unwrap();
    assert!(mailer.sent.lock().unwrap().is_empty());
    fs::remove_dir_all(dir).unwrap();
}

/// The follow rules, case by case, for notices.
#[test]
fn notice_delivery_follows_follow_rules() {
    let now = at("2026-09-26T10:00:00Z");
    let world = FakeNoticeWorld::default();

    // Push succeeds on one token; the dead one is invalidated.
    let (store, dir) = temp_store();
    register(&store, "tok1", None);
    register(&store, "dead", None);
    store
        .create_notice(&message_notice("msg_1", false), now)
        .unwrap();
    let sender = FakeSender::failing(
        "dead",
        vec![PushError::InvalidToken("UNREGISTERED".to_owned())],
    );
    let mailer = FakeNoticeMailer::default();
    deliver_notices(&store, &world, Some(&sender), &mailer, now).unwrap();
    let notice = notice_of(&store, NOTICE_MESSAGE, "msg_1");
    assert_eq!(notice.notified_via.as_deref(), Some("push"));
    assert_eq!(sender.sent().len(), 1);
    assert_eq!(store.valid_tokens(OWNER).unwrap().len(), 1);
    // Acked: no fallback. Not acked: one email at 15 minutes, never twice.
    store
        .create_notice(&message_notice("msg_2", false), now)
        .unwrap();
    deliver_notices(&store, &world, Some(&sender), &mailer, now).unwrap();
    store.ack_notice(OWNER, &notice.id, now).unwrap();
    for later in [Duration::minutes(14), ACK_FALLBACK, Duration::minutes(40)] {
        deliver_notices(&store, &world, Some(&sender), &mailer, now + later).unwrap();
    }
    let unacked = notice_of(&store, NOTICE_MESSAGE, "msg_2");
    assert_eq!(*mailer.sent.lock().unwrap(), vec![unacked.id.clone()]);
    assert_eq!(
        unacked.email_sent_at.as_deref(),
        Some("2026-09-26T10:15:00Z")
    );
    fs::remove_dir_all(dir).unwrap();

    // Retryable failures back off 30 s, 1, 2, 5, 10 minutes, then email.
    let (store, dir) = temp_store();
    register(&store, "tok1", None);
    store
        .create_notice(&message_notice("msg_3", false), now)
        .unwrap();
    let sender = FakeSender::failing(
        "tok1",
        (0..6)
            .map(|_| PushError::Retryable("503".to_owned()))
            .collect(),
    );
    let mailer = FakeNoticeMailer::default();
    let mut clock = now;
    for (attempt, delay) in [30_i64, 60, 120, 300, 600].into_iter().enumerate() {
        deliver_notices(&store, &world, Some(&sender), &mailer, clock).unwrap();
        let notice = notice_of(&store, NOTICE_MESSAGE, "msg_3");
        assert_eq!(notice.push_attempts, i64::try_from(attempt).unwrap() + 1);
        assert_eq!(
            notice.notify_after,
            format_ts(clock + Duration::seconds(delay))
        );
        clock += Duration::seconds(delay);
    }
    deliver_notices(&store, &world, Some(&sender), &mailer, clock).unwrap();
    let notice = notice_of(&store, NOTICE_MESSAGE, "msg_3");
    assert_eq!(notice.notified_via.as_deref(), Some("email"));
    assert_eq!(mailer.sent.lock().unwrap().len(), 1);
    fs::remove_dir_all(dir).unwrap();

    // No push configured, or push configured but no valid token: email at once.
    for push_configured in [false, true] {
        let (store, dir) = temp_store();
        store
            .create_notice(&message_notice("msg_4", false), now)
            .unwrap();
        let mailer = FakeNoticeMailer::default();
        let sender = FakeSender::default();
        let sender = push_configured.then_some(&sender as &dyn PushSender);
        deliver_notices(&store, &world, sender, &mailer, now).unwrap();
        assert_eq!(
            notice_of(&store, NOTICE_MESSAGE, "msg_4")
                .notified_via
                .as_deref(),
            Some("email")
        );
        assert_eq!(mailer.sent.lock().unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    // A transient email failure retries every 5 minutes within the hour.
    let (store, dir) = temp_store();
    store
        .create_notice(&message_notice("msg_5", false), now)
        .unwrap();
    let mailer = FakeNoticeMailer {
        transient_failures: Mutex::new(2),
        ..Default::default()
    };
    deliver_notices(&store, &world, None, &mailer, now).unwrap();
    let notice = notice_of(&store, NOTICE_MESSAGE, "msg_5");
    assert!(notice.notified_at.is_none());
    assert_eq!(notice.notify_after, "2026-09-26T10:05:00Z");
    deliver_notices(&store, &world, None, &mailer, now + Duration::minutes(5)).unwrap();
    deliver_notices(&store, &world, None, &mailer, now + Duration::minutes(10)).unwrap();
    assert_eq!(
        notice_of(&store, NOTICE_MESSAGE, "msg_5")
            .notified_via
            .as_deref(),
        Some("email")
    );
    assert_eq!(mailer.sent.lock().unwrap().len(), 1);
    fs::remove_dir_all(dir).unwrap();

    // Email not set up at all: marked notified rather than looping.
    let (store, dir) = temp_store();
    store
        .create_notice(&message_notice("msg_6", false), now)
        .unwrap();
    let mailer = FakeNoticeMailer {
        unavailable: true,
        ..Default::default()
    };
    deliver_notices(&store, &world, None, &mailer, now).unwrap();
    let notice = notice_of(&store, NOTICE_MESSAGE, "msg_6");
    assert_eq!(notice.last_push_error.as_deref(), Some("no channel"));
    assert!(notice.notified_at.is_some());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn notice_text_and_payload() {
    let (store, dir) = temp_store();
    register(&store, "tok1", None);
    let now = at("2026-09-26T10:00:00Z");
    store
        .create_notice(&message_notice("msg_3f9a2c1d", true), now)
        .unwrap();
    store
        .create_notice(&message_notice("msg_00000002", false), now)
        .unwrap();
    store.create_notice(&review_notice(12), now).unwrap();
    let world = FakeNoticeWorld {
        unread: 3,
        ..Default::default()
    };
    let sender = FakeSender::default();
    deliver_notices(
        &store,
        &world,
        Some(&sender),
        &FakeNoticeMailer::default(),
        now,
    )
    .unwrap();
    let sent = sender.sent();
    let blocking = notice_of(&store, NOTICE_MESSAGE, "msg_3f9a2c1d");
    assert_eq!(
        sent[0].1,
        BTreeMap::from([
            ("kind".to_owned(), "message".to_owned()),
            ("notice_id".to_owned(), blocking.id.clone()),
            ("session_id".to_owned(), "eng00001".to_owned()),
            ("title".to_owned(), "sm-1679-engineer needs you".to_owned()),
            (
                "body".to_owned(),
                "Keep the old fills table or drop it?".to_owned()
            ),
            (
                "reader_path".to_owned(),
                "/messages/msg_3f9a2c1d".to_owned()
            ),
            ("blocking".to_owned(), "1".to_owned()),
            ("unread_count".to_owned(), "3".to_owned()),
        ])
    );
    assert_eq!(sent[1].1["title"], "sm-1679-engineer");
    assert_eq!(sent[1].1["blocking"], "0");
    assert_eq!(sent[2].1["kind"], "review_requested");
    assert_eq!(sent[2].1["unread_count"], "0");
    assert_eq!(sent[2].1["title"], "sm-1679-engineer asks for your review");
    assert_eq!(sent[2].1["body"], "Decision memo");
    assert_eq!(
        sent[2].1["reader_path"],
        "/docs/widgets/memo.html?version=aaaaaaaaaaaa"
    );
    assert_eq!(
        blocking.email_subject(),
        "sm-1679-engineer needs you: Keep the old fills table or drop it?"
    );
    assert_eq!(
        notice_of(&store, NOTICE_REVIEW_REQUESTED, "12").email_subject(),
        "sm-1679-engineer asks for your review: Decision memo"
    );
    fs::remove_dir_all(dir).unwrap();
}
