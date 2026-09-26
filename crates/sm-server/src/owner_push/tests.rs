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
    fail: bool,
}

impl FollowMailer for FakeMailer {
    fn send(&self, follow: &Follow, notification: &Notification) -> Result<()> {
        if self.fail {
            anyhow::bail!("bridge down");
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
        fail: true,
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
