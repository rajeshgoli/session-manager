//! Auto PM library tests (sm#2174; spec 2170 B, C and F.2). This file holds
//! the fixture and the switch, experiment and gate tests; `tests/budget.rs`
//! and `tests/measure.rs` hold the rest.

use super::budget;
use super::experiments::{self, Ctx, Experiment, Git, Pull};
use super::metrics::{self, Sample};
use super::switches::{self, Switches};
use super::*;
use serde_json::json;
use std::sync::Mutex;

mod budget_tests;
mod measure_tests;

const COMMIT: &str = "c0ffee0000000000000000000000000000000000";
const HEAD: &str = "feed000000000000000000000000000000000000";
const BUILD: &str = "b0b0000000000000000000000000000000000000";
const T0: i64 = 1_792_000_000_000;

#[derive(Default)]
struct FakeGit {
    pulls: Mutex<BTreeMap<i64, Pull>>,
    files: Mutex<BTreeMap<(String, String), String>>,
    ancestors: Mutex<BTreeSet<(String, String)>>,
}

impl Git for FakeGit {
    fn pull(&self, pr: i64) -> Result<Pull, String> {
        self.pulls
            .lock()
            .unwrap()
            .get(&pr)
            .cloned()
            .ok_or_else(|| format!("no PR {pr}"))
    }
    fn file_at(&self, path: &str, commit: &str) -> Result<Option<String>, String> {
        Ok(self
            .files
            .lock()
            .unwrap()
            .get(&(path.to_owned(), commit.to_owned()))
            .cloned())
    }
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, String> {
        Ok(ancestor == descendant
            || self
                .ancestors
                .lock()
                .unwrap()
                .contains(&(ancestor.to_owned(), descendant.to_owned())))
    }
    fn close_issue(&self, _number: i64, _comment: &str) -> Result<(), String> {
        Ok(())
    }
}

impl FakeGit {
    fn merged(&self, pr: i64, merge: &str) {
        self.pulls.lock().unwrap().insert(
            pr,
            Pull {
                state: "merged".into(),
                base: "main".into(),
                head_sha: HEAD.into(),
                merge_commit: Some(merge.into()),
            },
        );
    }
    fn open(&self, pr: i64) {
        self.pulls.lock().unwrap().insert(
            pr,
            Pull {
                state: "open".into(),
                base: "main".into(),
                head_sha: HEAD.into(),
                merge_commit: None,
            },
        );
    }
    fn file(&self, path: &str, commit: &str, text: &str) {
        self.files
            .lock()
            .unwrap()
            .insert((path.into(), commit.into()), text.into());
    }
    fn ancestor(&self, ancestor: &str, descendant: &str) {
        self.ancestors
            .lock()
            .unwrap()
            .insert((ancestor.into(), descendant.into()));
    }
}

struct Fixture {
    _dir: PathBuf,
    store: AutopmStore,
    switches: Switches,
    git: FakeGit,
    settings: Settings,
    now: i64,
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sm-autopm-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn enabled_settings() -> Settings {
    let mut owner = crate::owner_settings::defaults();
    owner["autopm"]["enabled"] = json!(true);
    Settings::from_owner(&owner)
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = temp_dir(name);
        let paths = Paths {
            queue_db: dir.join("messages.db"),
            usage_db: dir.join("usage.db"),
            activity_db: dir.join("activity.db"),
            utilization_db: dir.join("utilization.db"),
            bugs_db: dir.join("bugs.db"),
        };
        let store = AutopmStore::new(paths);
        let conn = store.open().unwrap();
        crate::board::init_board_schema(&conn).unwrap();
        crate::work_claims::init_work_claims_schema(&conn).unwrap();
        crate::queue::RetainedQueueStore::ensure_codex_review_requests_schema_from_path(
            &store.paths.queue_db,
        )
        .unwrap();
        Connection::open(&store.paths.usage_db)
            .unwrap()
            .execute_batch(
                "CREATE TABLE accounts (account_key TEXT PRIMARY KEY, provider TEXT NOT NULL,
                   last_seen TEXT NOT NULL);
                 CREATE TABLE burn_samples (id INTEGER PRIMARY KEY AUTOINCREMENT,
                   account_key TEXT NOT NULL, window_kind TEXT NOT NULL, window_scope TEXT,
                   window_start TEXT NOT NULL, percent REAL NOT NULL, resets_at TEXT NOT NULL,
                   observed_at TEXT NOT NULL);
                 CREATE TABLE seat_tokens (seat_id TEXT NOT NULL, account_key TEXT NOT NULL,
                   window_kind TEXT NOT NULL, bucket_ts TEXT NOT NULL, model TEXT NOT NULL,
                   input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
                   cache_write_5m INTEGER NOT NULL DEFAULT 0, cache_write_1h INTEGER NOT NULL DEFAULT 0,
                   cache_read_tokens INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO accounts VALUES ('claude:a', 'claude', '2026-10-01T00:00:00Z'),
                   ('codex:b', 'codex', '2026-10-01T00:00:00Z');",
            )
            .unwrap();
        Self {
            _dir: dir,
            store,
            switches: Switches::new(true),
            git: FakeGit::default(),
            settings: enabled_settings(),
            now: T0,
        }
    }

    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            store: &self.store,
            switches: &self.switches,
            git: &self.git,
            settings: &self.settings,
            build_sha: Some(BUILD),
            now: self.now,
        }
    }

    fn conn(&self) -> Connection {
        self.store.open().unwrap()
    }

    fn get(&self, id: &str) -> Experiment {
        experiments::get(&self.conn(), id).unwrap().unwrap()
    }

    fn switch(&self, key: &str) -> switches::SwitchRow {
        switches::get(&self.conn(), key).unwrap().unwrap()
    }

    /// A weekly sample for `provider`, window starting at `start`.
    fn window(&self, provider: &str, start: i64, percent: f64, observed: i64) {
        let account = if provider == "claude" {
            "claude:a"
        } else {
            "codex:b"
        };
        Connection::open(&self.store.paths.usage_db)
            .unwrap()
            .execute(
                "INSERT INTO burn_samples(account_key, window_kind, window_start, percent, resets_at, observed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    account,
                    budget::window_kind(provider),
                    format_ms(start),
                    percent,
                    format_ms(start + 7 * DAY_MS),
                    format_ms(observed)
                ],
            )
            .unwrap();
    }

    /// `points` fitted points spent by `seat` at `at`.
    fn spend(&self, provider: &str, seat: &str, points: f64, at: i64) {
        let (account, model) = if provider == "claude" {
            ("claude:a", "claude-opus-5-5")
        } else {
            ("codex:b", "gpt-6-sol")
        };
        let rate = crate::quota_rates::pricing(provider, model).rate;
        let input = (points / rate * 1_000_000.0).round() as i64;
        Connection::open(&self.store.paths.usage_db)
            .unwrap()
            .execute(
                "INSERT INTO seat_tokens(seat_id, account_key, window_kind, bucket_ts, model, input_tokens)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![seat, account, budget::window_kind(provider), format_ms(at), model, input],
            )
            .unwrap();
    }

    /// `child` is a sub-issue of `parent` on the board.
    fn under(&self, child: i64, parent: i64) {
        self.conn()
            .execute(
                "INSERT INTO board_edges(waiter_repo, waiter_number, blocker_repo, blocker_number,
                   kind, first_seen_at, source) VALUES (?1, ?2, ?1, ?3, 'sub_issue', 'now', 'test')",
                params![REPO, parent, child],
            )
            .unwrap();
    }

    fn claim(&self, number: i64, session: &str, claimed_at: i64, live: bool) {
        self.conn()
            .execute(
                "INSERT INTO work_claims(id, repo, number, kind, session_id, source, claimed_at, ended_at)
                 VALUES (?1, ?2, ?3, 'ticket', ?4, 'explicit', ?5, ?6)",
                params![
                    format!("{number}-{session}-{claimed_at}"),
                    REPO,
                    number,
                    session,
                    format_ms(claimed_at),
                    (!live).then(|| format_ms(claimed_at + 1)),
                ],
            )
            .unwrap();
    }

    fn end_claims(&self, number: i64) {
        self.conn()
            .execute(
                "UPDATE work_claims SET ended_at = 'x' WHERE number = ?1 AND ended_at IS NULL",
                [number],
            )
            .unwrap();
    }

    fn close_ticket(&self, number: i64) {
        self.conn()
            .execute(
                "INSERT OR REPLACE INTO board_items(repo, number, title, url, state, first_seen_at, synced_at)
                 VALUES (?1, ?2, 't', 'u', 'closed', 'now', 'now')",
                params![REPO, number],
            )
            .unwrap();
    }

    fn link(&self, ticket: i64, pr: i64) {
        self.conn()
            .execute(
                "INSERT INTO work_links(repo, pr_number, ticket_number, source, created_at)
                 VALUES (?1, ?2, ?3, 'test', 'now')",
                params![REPO, pr, ticket],
            )
            .unwrap();
    }

    /// The last `sm request-review` run on `pr`, completed at `head`.
    fn reviewed(&self, pr: i64, head: &str) {
        let r = crate::queue::RetainedQueueStore::create_codex_review_request_in_path(
            &self.store.paths.queue_db,
            crate::queue::CreateCodexReviewRequest {
                repo: REPO.into(),
                pr_number: pr,
                requester_session_id: Some("pm".into()),
                notify_session_id: "pm".into(),
                steer: None,
                requested_head_sha: head.into(),
                latest_request_comment_id: None,
                latest_request_comment_url: None,
                latest_request_posted_at: "2026-10-01T00:00:00Z".into(),
                poll_interval_seconds: 30,
                retry_interval_seconds: 120,
            },
        )
        .unwrap();
        self.conn()
            .execute(
                "UPDATE codex_review_request_registrations SET state = 'completed', is_active = 0
                 WHERE id = ?1",
                [&r.id],
            )
            .unwrap();
    }

    /// A switch experiment whose plan merged after a review at its head.
    fn planned(&self, id: &str, pr: i64) -> Experiment {
        let key = format!(
            "autopm.{}",
            id.rsplit_once('-').map(|(_, s)| s).unwrap_or(id)
        );
        self.planned_with(id, pr, json!({"path": "switch", "switch_key": key}))
    }

    fn planned_with(&self, id: &str, pr: i64, extra: Value) -> Experiment {
        let mut body = json!({
            "id": id, "cycle_ticket": 3001, "title": "Decision card",
            "hypothesis": "Judgment asks get answered faster",
            "plan_path": format!("docs/autopm/plans/{id}.html"), "plan_pr": pr,
            "metric_json": {"query": "docs/autopm/metrics/m.sql", "stat": "median",
                            "direction": "lower", "min_change_pct": 25},
            "guards_json": [{"query": "docs/autopm/metrics/g.sql", "stat": "per_day",
                             "bad_direction": "higher", "max_worse_pct": 25}],
            "points_est_json": {"claude": 2, "codex": 6},
            "tickets_json": [pr + 1],
        });
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        let x = experiments::create(&self.ctx(), Actor::Autopm, &body).unwrap();
        self.git.merged(pr, COMMIT);
        self.reviewed(pr, HEAD);
        self.git
            .file(&format!("docs/autopm/plans/{id}.html"), COMMIT, id);
        self.git.file(
            "docs/autopm/metrics/m.sql",
            COMMIT,
            "SELECT at_ms, value FROM q.obs WHERE at_ms >= :from_ms AND at_ms < :until_ms",
        );
        self.git.file(
            "docs/autopm/metrics/g.sql",
            COMMIT,
            "SELECT at_ms, value FROM q.bad WHERE at_ms >= :from_ms AND at_ms < :until_ms",
        );
        x
    }

    /// Rows for the metric query (`obs`) or the guard query (`bad`).
    fn observe(&self, table: &str, rows: &[(i64, f64)]) {
        let conn = self.conn();
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {table} (at_ms INTEGER, value REAL)"
        ))
        .unwrap();
        for (at, value) in rows {
            conn.execute(
                &format!("INSERT INTO {table} VALUES (?1, ?2)"),
                params![at, value],
            )
            .unwrap();
        }
    }

    fn set_state(&self, id: &str, state: &str) {
        self.conn()
            .execute(
                "UPDATE autopm_experiments SET state = ?2 WHERE id = ?1",
                params![id, state],
            )
            .unwrap();
    }

    /// 20 observations of `value` in the 14 days before now.
    fn baseline(&self, value: f64) {
        let rows: Vec<(i64, f64)> = (0..20)
            .map(|i| (self.now - DAY_MS * 13 + i * HOUR_MS, value))
            .collect();
        self.observe("obs", &rows);
        self.observe("bad", &[]);
    }

    /// Takes an experiment from dark to live by the role's switch.
    fn live(&self, id: &str) -> Experiment {
        self.set_state(id, "dark");
        let key = self.get(id).switch_key.unwrap();
        experiments::set_switch(&self.ctx(), Actor::Autopm, &key, true, None).unwrap();
        self.get(id)
    }
}

fn refusal(fail: Fail) -> Refusal {
    match fail {
        Fail::Refused(refusal) => refusal,
        Fail::Internal(error) => panic!("expected a refusal, got {error:#}"),
    }
}

fn gate_of<T: std::fmt::Debug>(result: Out<T>) -> String {
    refusal(result.expect_err("expected a refusal")).gate
}

// ---- B.1 switches ----

#[test]
fn a_missing_key_reads_off_and_every_switch_reads_off_while_disabled() {
    let f = Fixture::new("missing");
    assert!(!f.switches.is_on("autopm.nothing"));
    f.baseline(10.0);
    f.planned("x-2026w43-card", 10);
    f.live("x-2026w43-card");
    assert!(f.switches.is_on("autopm.card"));
    let before = f.switches.version();
    f.switches.set_enabled(false);
    assert!(f.switches.version() > before, "disable raises the version");
    assert!(!f.switches.is_on("autopm.card"));
    assert!(!f.switches.effective()["autopm.card"]);
    assert!(f.switch("autopm.card").on, "the row itself is untouched");
}

#[test]
fn only_an_experiment_creates_its_switch_and_a_key_is_never_reused() {
    let f = Fixture::new("create-switch");
    f.planned("x-2026w43-card", 10);
    let row = f.switch("autopm.card");
    assert!(!row.on && !row.held);
    assert_eq!(row.experiment_id, "x-2026w43-card");
    let again = experiments::create(
        &f.ctx(),
        Actor::Autopm,
        &json!({"id": "x-2026w43-other", "cycle_ticket": 3001, "title": "t", "hypothesis": "h",
                "path": "switch", "switch_key": "autopm.card",
                "plan_path": "docs/autopm/plans/x-2026w43-other.html", "plan_pr": 20,
                "metric_json": {"query": "docs/autopm/metrics/m.sql", "stat": "mean",
                                "direction": "higher", "min_change_pct": 25},
                "guards_json": [], "points_est_json": {"codex": 3}}),
    );
    assert_eq!(gate_of(again), "switch_exists");
    let missing = experiments::set_switch(&f.ctx(), Actor::Owner, "autopm.none", true, None);
    assert_eq!(refusal(missing.unwrap_err()).status, 404);
}

#[test]
fn a_held_switch_refuses_the_role_and_the_owner_turning_it_on_clears_held() {
    let f = Fixture::new("held");
    f.baseline(10.0);
    f.planned("x-2026w43-card", 10);
    f.set_state("x-2026w43-card", "dark");
    f.conn()
        .execute("UPDATE experiment_switches SET held = 1", [])
        .unwrap();
    let role = experiments::set_switch(&f.ctx(), Actor::Autopm, "autopm.card", true, None);
    let refused = refusal(role.unwrap_err());
    assert_eq!(refused.message(), "held: only Rajesh can turn this back on");
    let (row, _) =
        experiments::set_switch(&f.ctx(), Actor::Owner, "autopm.card", true, None).unwrap();
    assert!(row.on && !row.held);
    assert_eq!(f.get("x-2026w43-card").state, "live");
}

#[test]
fn the_role_switching_off_reverts_a_live_experiment_without_holding_it() {
    let f = Fixture::new("role-off");
    f.baseline(10.0);
    f.planned("x-2026w43-card", 10);
    f.live("x-2026w43-card");
    let (row, effects) =
        experiments::set_switch(&f.ctx(), Actor::Autopm, "autopm.card", false, None).unwrap();
    assert!(!row.on && !row.held);
    let x = f.get("x-2026w43-card");
    assert_eq!(x.state, "reverted");
    assert!(x.decision_note.unwrap().contains("auto PM switched off"));
    assert!(matches!(&effects[0], Effect::WakeAutopm { .. }));
}

#[test]
fn owner_switching_a_reverted_change_back_on_relives_it_and_cancels_its_cleanup() {
    let f = Fixture::new("reenable");
    f.baseline(10.0);
    f.planned("x-2026w43-card", 10);
    let live = f.live("x-2026w43-card");
    let baseline = live.baseline_json.clone().unwrap();
    f.conn()
        .execute(
            "INSERT INTO autopm_measurements(experiment_id, attempt, at_ms, status, decision)
             VALUES ('x-2026w43-card', 1, 1, 'ok', 'revert')",
            [],
        )
        .unwrap();
    experiments::set_switch(&f.ctx(), Actor::Owner, "autopm.card", false, None).unwrap();
    assert_eq!(f.get("x-2026w43-card").state, "reverted");
    // sm filed a cleanup ticket removing the new path, and an agent took it.
    f.store
        .record_fix(4040, "cleanup", Some(3001), Some("x-2026w43-card"), T0)
        .unwrap();
    f.claim(4040, "cleaner", T0, true);
    f.link(4040, 4041);

    let mut off = f.settings.clone();
    off.enabled = false;
    let disabled = Ctx {
        settings: &off,
        ..f.ctx()
    };
    let refused = experiments::set_switch(&disabled, Actor::Owner, "autopm.card", true, None);
    assert_eq!(gate_of(refused), "autopm_disabled");

    let later = Ctx {
        now: T0 + DAY_MS,
        ..f.ctx()
    };
    let (row, effects) =
        experiments::set_switch(&later, Actor::Owner, "autopm.card", true, None).unwrap();
    assert!(row.on && !row.held);
    let x = f.get("x-2026w43-card");
    assert_eq!(x.state, "live");
    assert_eq!(x.live_at_ms, Some(T0 + DAY_MS));
    assert_eq!(x.measure_at_ms, Some(T0 + DAY_MS + 14 * DAY_MS));
    assert!(!x.extended);
    assert_eq!(
        x.baseline_until_ms, live.baseline_until_ms,
        "baseline is fixed"
    );
    assert_eq!(x.baseline_json.unwrap(), baseline, "and never re-queried");
    let superseded: i64 = f
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM autopm_measurements WHERE superseded_at_ms IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(superseded, 1);
    assert!(effects.contains(&Effect::MergeHold {
        pr: 4041,
        reason: "cleanup #4040 was cancelled: Rajesh re-enabled the change".into()
    }));
    assert!(effects.contains(&Effect::CloseIssue {
        number: 4040,
        comment: "Rajesh re-enabled the change".into()
    }));
    assert!(effects
        .iter()
        .any(|e| matches!(e, Effect::CancelAutoStart { number: 4040, .. })));
    assert!(effects.contains(&Effect::Message {
        session_id: "cleaner".into(),
        text: "[sm autopm] cleanup cancelled: Rajesh re-enabled the change. Close your PR unmerged and stop".into()
    }));
    assert!(fix(&f.conn(), 4040)
        .unwrap()
        .unwrap()
        .cancelled_at_ms
        .is_some());
    // A PR opened for the cancelled cleanup afterwards is held too.
    f.link(4040, 4042);
    let holds = experiments::cancelled_cleanup_prs(&f.conn()).unwrap();
    assert!(holds
        .iter()
        .any(|e| matches!(e, Effect::MergeHold { pr: 4042, .. })));
}

// ---- B.5 kill switch ----

#[test]
fn the_kill_switch_holds_every_on_switch_and_re_enabling_turns_nothing_on() {
    let f = Fixture::new("kill");
    f.baseline(10.0);
    f.planned("x-2026w43-card", 10);
    f.planned("x-2026w43-kept", 20);
    f.live("x-2026w43-card");
    f.live("x-2026w43-kept");
    f.set_state("x-2026w43-kept", "kept");
    f.under(3001, GOAL);
    f.under(3002, 3001);
    f.under(3003, 3001);
    f.claim(3002, "builder", T0, true);
    f.claim(3003, "reverter", T0, true);
    f.store
        .record_fix(3003, "revert", Some(3001), None, T0)
        .unwrap();
    let effects = experiments::disable(&f.ctx()).unwrap();
    for key in ["autopm.card", "autopm.kept"] {
        let row = f.switch(key);
        assert!(!row.on && row.held, "{key}");
        assert_eq!(row.changed_by, "killswitch");
    }
    assert_eq!(f.get("x-2026w43-card").state, "reverted");
    assert_eq!(f.get("x-2026w43-kept").state, "kept");
    let text =
        "[sm autopm] disabled by Rajesh: push work in progress, comment on your ticket, stop";
    assert!(effects.contains(&Effect::WakeAutopm { text: text.into() }));
    assert!(effects.contains(&Effect::Message {
        session_id: "builder".into(),
        text: text.into()
    }));
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::Message { session_id, .. } if session_id == "reverter")),
        "a revert ticket's agent is never stopped"
    );
    f.switches.set_enabled(true);
    assert!(!f.switches.is_on("autopm.card") && !f.switches.is_on("autopm.kept"));
    let role = experiments::set_switch(&f.ctx(), Actor::Autopm, "autopm.kept", true, None);
    assert_eq!(gate_of(role), "held");
}

// ---- B.3 state machine and gates ----

#[test]
fn state_moves_only_along_the_arrows() {
    let f = Fixture::new("arrows");
    f.planned("x-2026w43-card", 10);
    let patch = |state: &str, note: Option<&str>| {
        let mut body = json!({ "state": state });
        if let Some(note) = note {
            body["decision_note"] = json!(note);
        }
        experiments::patch(&f.ctx(), Actor::Autopm, "x-2026w43-card", &body)
    };
    assert_eq!(gate_of(patch("live", None)), "transition");
    assert_eq!(gate_of(patch("shadowed", None)), "transition");
    // Plan fields change only until the plan's merge is recorded.
    experiments::patch(
        &f.ctx(),
        Actor::Autopm,
        "x-2026w43-card",
        &json!({"window_days": 20}),
    )
    .unwrap();
    assert_eq!(
        gate_of(experiments::patch(
            &f.ctx(),
            Actor::Autopm,
            "x-2026w43-card",
            &json!({"window_days": 15})
        )),
        "invalid",
        "odd windows are refused"
    );
    patch("building", None).unwrap();
    let fixed = experiments::patch(
        &f.ctx(),
        Actor::Autopm,
        "x-2026w43-card",
        &json!({"window_days": 16}),
    );
    assert_eq!(gate_of(fixed), "fixed");
    assert_eq!(f.get("x-2026w43-card").window_days, 20);
    assert_eq!(gate_of(patch("dark", None)), "server_only");
    f.set_state("x-2026w43-card", "dark");
    assert_eq!(gate_of(patch("live", None)), "use_switch");
    assert_eq!(gate_of(patch("abandoned", None)), "note");
    patch("abandoned", Some("superseded by a better plan")).unwrap();
    assert_eq!(f.get("x-2026w43-card").state, "abandoned");
    for state in ["live", "measuring", "kept"] {
        f.set_state("x-2026w43-card", state);
        assert_eq!(
            gate_of(patch("abandoned", Some("no"))),
            "not_pre_live",
            "{state}"
        );
    }
}

#[test]
fn shadow_experiments_wait_for_the_owners_yes_and_proposals_take_no_plan() {
    let f = Fixture::new("shadow");
    f.planned_with(
        "x-2026w43-rows",
        10,
        json!({"path": "shadow", "change_class": "rows"}),
    );
    let p = |state: &str, actor| {
        experiments::patch(
            &f.ctx(),
            actor,
            "x-2026w43-rows",
            &json!({ "state": state }),
        )
    };
    p("building", Actor::Autopm).unwrap();
    p("shadowed", Actor::Autopm).unwrap();
    assert_eq!(gate_of(p("awaiting_owner", Actor::Autopm)), "evidence");
    experiments::patch(
        &f.ctx(),
        Actor::Autopm,
        "x-2026w43-rows",
        &json!({"state": "awaiting_owner",
                "decision_note": "restore byte-identical; cannot show: your reaction"}),
    )
    .unwrap();
    assert_eq!(gate_of(p("building", Actor::Autopm)), "owner_answer");
    experiments::answer(&f.ctx(), "x-2026w43-rows", true, "Rajesh said yes").unwrap();
    assert_eq!(f.get("x-2026w43-rows").state, "building");

    let shadow_without_class = experiments::create(
        &f.ctx(),
        Actor::Autopm,
        &json!({"id": "x-2026w43-noclass", "cycle_ticket": 3001, "title": "t", "hypothesis": "h",
                "path": "shadow", "plan_path": "docs/autopm/plans/x-2026w43-noclass.html",
                "plan_pr": 30, "metric_json": {"query": "docs/autopm/metrics/m.sql",
                "stat": "mean", "direction": "higher", "min_change_pct": 25},
                "guards_json": [], "points_est_json": {"codex": 1}}),
    );
    assert_eq!(gate_of(shadow_without_class), "invalid");

    let proposal = |extra: Value| {
        let mut body = json!({"id": "x-2026w43-far", "cycle_ticket": 3001, "title": "Faster far",
            "hypothesis": "h", "path": "proposal", "target_repo": "rajeshgoli/fractal-algo-rust",
            "proposal_md": "Do the thing."});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        experiments::create(&f.ctx(), Actor::Autopm, &body)
    };
    assert_eq!(gate_of(proposal(json!({"plan_pr": 5}))), "proposal_fields");
    assert_eq!(
        gate_of(proposal(json!({"switch_key": "autopm.x"}))),
        "proposal_fields"
    );
    assert_eq!(
        gate_of(proposal(json!({"window_days": 14}))),
        "proposal_fields"
    );
    assert_eq!(
        gate_of(proposal(
            json!({"target_repo": "rajeshgoli/session-manager"})
        )),
        "invalid"
    );
    let long = vec!["word"; 401].join(" ");
    assert_eq!(gate_of(proposal(json!({ "proposal_md": long }))), "invalid");
    assert_eq!(proposal(json!({})).unwrap().state, "planned");
    experiments::patch(
        &f.ctx(),
        Actor::Autopm,
        "x-2026w43-far",
        &json!({"state": "awaiting_owner"}),
    )
    .unwrap();
    let handed = experiments::answer(
        &f.ctx(),
        "x-2026w43-far",
        true,
        "handed to rajeshgoli/fractal-algo-rust#9",
    )
    .unwrap();
    assert_eq!(handed.state, "abandoned");
}

#[test]
fn plan_merged_needs_the_merge_a_completed_review_at_the_final_head_and_the_named_plan() {
    let f = Fixture::new("plan-merged");
    let x = experiments::create(
        &f.ctx(),
        Actor::Autopm,
        &json!({"id": "x-2026w43-card", "cycle_ticket": 3001, "title": "t", "hypothesis": "h",
                "path": "switch", "switch_key": "autopm.card",
                "plan_path": "docs/autopm/plans/x-2026w43-card.html", "plan_pr": 10,
                "metric_json": {"query": "docs/autopm/metrics/m.sql", "stat": "median",
                                "direction": "lower", "min_change_pct": 25},
                "guards_json": [], "points_est_json": {"claude": 1}}),
    )
    .unwrap();
    let check = || gate_of(experiments::plan_merged(&f.ctx(), &x));
    f.git.open(10);
    assert_eq!(check(), "plan_merged");
    f.git.merged(10, COMMIT);
    assert_eq!(check(), "plan_merged", "no review");
    f.reviewed(10, "0ld0000000000000000000000000000000000000");
    assert_eq!(check(), "plan_merged", "reviewed at an older head");
    f.reviewed(10, HEAD);
    assert_eq!(check(), "plan_merged", "plan file missing");
    f.git.file(
        "docs/autopm/plans/x-2026w43-card.html",
        COMMIT,
        "another plan",
    );
    assert_eq!(check(), "plan_merged", "plan does not name the experiment");
    f.git.file(
        "docs/autopm/plans/x-2026w43-card.html",
        COMMIT,
        "<h1>x-2026w43-card</h1>",
    );
    assert_eq!(
        experiments::plan_merged(&f.ctx(), &x).unwrap(),
        (COMMIT.to_owned(), HEAD.to_owned())
    );
    let x = f.get("x-2026w43-card");
    assert_eq!(x.plan_commit.as_deref(), Some(COMMIT));
    assert_eq!(x.plan_reviewed_sha.as_deref(), Some(HEAD));

    // The issue gate (B.3) builds on it.
    let issue = |body: &str| experiments::gate_issue(&f.ctx(), "x-2026w43-card", body);
    assert_eq!(gate_of(issue("no id here")), "names_experiment");
    issue("Build x-2026w43-card\nTier: Mid").unwrap();
    f.set_state("x-2026w43-card", "dark");
    assert_eq!(gate_of(issue("x-2026w43-card")), "state");
}

#[test]
fn switching_on_needs_enabled_dark_plan_merged_room_under_max_live_and_a_baseline() {
    let mut f = Fixture::new("switch-on");
    f.baseline(10.0);
    for (i, id) in ["x-2026w43-a", "x-2026w43-b", "x-2026w43-c", "x-2026w43-d"]
        .iter()
        .enumerate()
    {
        f.planned(id, 10 * (i as i64 + 1));
    }
    let on =
        |f: &Fixture, key: &str| experiments::set_switch(&f.ctx(), Actor::Autopm, key, true, None);
    assert_eq!(gate_of(on(&f, "autopm.a")), "not_dark");
    f.set_state("x-2026w43-a", "dark");
    f.settings.enabled = false;
    assert_eq!(gate_of(on(&f, "autopm.a")), "autopm_disabled");
    f.settings.enabled = true;
    f.git.open(10);
    assert_eq!(gate_of(on(&f, "autopm.a")), "plan_merged");
    f.git.merged(10, COMMIT);
    on(&f, "autopm.a").unwrap();
    let a = f.get("x-2026w43-a");
    assert_eq!((a.state.as_str(), a.live_at_ms), ("live", Some(T0)));
    assert_eq!(a.measure_at_ms, Some(T0 + 14 * DAY_MS));
    assert_eq!(a.baseline_until_ms, Some(T0));
    let baseline: metrics::Baseline = serde_json::from_value(a.baseline_json.unwrap()).unwrap();
    assert_eq!(
        baseline.metric,
        Sample {
            stat: Some(10.0),
            n: 20
        }
    );
    f.live("x-2026w43-b");
    f.set_state("x-2026w43-b", "measuring");
    f.live("x-2026w43-c");
    f.set_state("x-2026w43-c", "rolling_back");
    f.set_state("x-2026w43-d", "dark");
    assert_eq!(
        gate_of(on(&f, "autopm.d")),
        "max_live",
        "live, measuring and rolling back count"
    );
    f.set_state("x-2026w43-c", "reverted");
    on(&f, "autopm.d").unwrap();

    let zero = Fixture::new("baseline-zero");
    zero.baseline(0.0);
    zero.planned("x-2026w43-z", 10);
    zero.set_state("x-2026w43-z", "dark");
    assert_eq!(gate_of(on(&zero, "autopm.z")), "baseline_zero");
    assert_eq!(zero.get("x-2026w43-z").state, "dark");
}

#[test]
fn a_switchless_change_goes_live_only_by_its_route_and_refuses_a_zero_baseline() {
    let f = Fixture::new("switchless");
    f.baseline(0.0);
    f.planned_with(
        "x-2026w43-text",
        10,
        json!({"path": "shadow", "change_class": "agent_text"}),
    );
    f.planned("x-2026w43-card", 20);
    f.set_state("x-2026w43-text", "dark");
    f.set_state("x-2026w43-card", "dark");
    let go = |id| experiments::go_live_switchless(&f.ctx(), Actor::Autopm, id);
    assert_eq!(gate_of(go("x-2026w43-card")), "has_switch");
    assert_eq!(gate_of(go("x-2026w43-text")), "baseline_zero");
    f.conn().execute("UPDATE obs SET value = 4", []).unwrap();
    let x = go("x-2026w43-text").unwrap();
    assert_eq!(x.state, "live");
    assert!(x.switch_key.is_none());
}

#[test]
fn a_thumbs_down_on_a_switchless_change_rolls_back_until_the_build_drops_it() {
    let f = Fixture::new("rollback");
    f.baseline(10.0);
    f.planned_with(
        "x-2026w43-text",
        10,
        json!({"path": "shadow", "change_class": "agent_text"}),
    );
    f.set_state("x-2026w43-text", "dark");
    experiments::go_live_switchless(&f.ctx(), Actor::Autopm, "x-2026w43-text").unwrap();
    let (x, effects) = experiments::vote(&f.ctx(), "x-2026w43-text", false, Some("worse")).unwrap();
    assert_eq!((x.state.as_str(), x.votes_down), ("rolling_back", 1));
    assert!(!effects.is_empty());
    assert_eq!(
        experiments::revert_tickets_due(&f.conn()).unwrap()[0].id,
        "x-2026w43-text",
        "sm#2175 files the revert ticket from this hook"
    );
    f.store
        .record_fix(5001, "revert", Some(3001), Some("x-2026w43-text"), T0)
        .unwrap();
    assert!(experiments::revert_tickets_due(&f.conn())
        .unwrap()
        .is_empty());
    // The revert PR merges, but the running build does not contain it yet.
    let revert = "aaaa000000000000000000000000000000000000";
    f.link(5001, 5002);
    f.git.merged(5002, revert);
    experiments::tick(&f.ctx(), &[]).unwrap();
    let x = f.get("x-2026w43-text");
    assert_eq!(x.state, "rolling_back");
    assert_eq!(x.revert_commit.as_deref(), Some(revert));
    f.git.ancestor(revert, BUILD);
    experiments::tick(&f.ctx(), &[]).unwrap();
    assert_eq!(f.get("x-2026w43-text").state, "reverted");
}

#[test]
fn a_rows_change_with_a_switch_turns_it_off_and_reverts_only_after_its_restore() {
    let f = Fixture::new("rows");
    f.baseline(10.0);
    f.planned_with(
        "x-2026w43-rows",
        10,
        json!({"path": "shadow", "change_class": "rows", "switch_key": "autopm.rows"}),
    );
    f.live("x-2026w43-rows");
    let (x, _) = experiments::vote(&f.ctx(), "x-2026w43-rows", false, None).unwrap();
    assert_eq!(x.state, "rolling_back");
    let row = f.switch("autopm.rows");
    assert!(!row.on && row.held && row.changed_by == "feedback");
    f.store
        .record_fix(5001, "revert", Some(3001), Some("x-2026w43-rows"), T0)
        .unwrap();
    f.link(5001, 5002);
    f.git.merged(5002, BUILD);
    experiments::tick(&f.ctx(), &[]).unwrap();
    assert_eq!(
        f.get("x-2026w43-rows").state,
        "rolling_back",
        "no restore record yet"
    );
    let restore =
        |body: Value| experiments::patch(&f.ctx(), Actor::Autopm, "x-2026w43-rows", &body);
    assert_eq!(gate_of(restore(json!({"restore_ok": true}))), "invalid");
    restore(json!({"restore_ok": true, "decision_note": "restored; byte-identical"})).unwrap();
    assert!(f.get("x-2026w43-rows").restore_ok_at_ms.is_some());
    experiments::tick(&f.ctx(), &[]).unwrap();
    assert_eq!(f.get("x-2026w43-rows").state, "reverted");
}

#[test]
fn sm_moves_building_to_dark_when_merged_into_the_build_and_the_app_is_installed() {
    let f = Fixture::new("dark");
    let x = f.planned("x-2026w43-card", 10);
    let ticket = x.tickets()[0];
    f.set_state("x-2026w43-card", "building");
    let tick = |versions: &[String]| {
        experiments::tick(&f.ctx(), versions).unwrap();
        f.get("x-2026w43-card").state
    };
    assert_eq!(tick(&[]), "building", "no PR yet");
    f.link(ticket, 12);
    f.git.open(12);
    assert_eq!(tick(&[]), "building", "PR still open");
    let merge = "dddd000000000000000000000000000000000000";
    f.git.merged(12, merge);
    assert_eq!(tick(&[]), "building", "merged, not in the running build");
    f.git.ancestor(merge, BUILD);
    f.store
        .record_app_deploy(ticket, "apk1", Some("0.5.70"), T0)
        .unwrap();
    assert_eq!(tick(&["0.5.67".into()]), "building", "the app is older");
    assert_eq!(tick(&["0.5.70".into()]), "dark");
    assert_eq!(experiments::version_at_least("0.10.0", "0.9.9"), Some(true));
    assert_eq!(experiments::version_at_least("1877-check", "0.1"), None);
}

#[test]
fn fix_gates_need_side_effects_none_a_tier_line_and_an_open_fix_ticket() {
    let f = Fixture::new("fix");
    assert_eq!(
        experiments::gate_fix_issue("Fix it\nTier: Mid")
            .unwrap_err()
            .gate,
        "side_effects"
    );
    assert_eq!(
        experiments::gate_fix_issue("side effects: none")
            .unwrap_err()
            .gate,
        "tier"
    );
    experiments::gate_fix_issue("Fix the badge.\nSide effects: none\nTier: Mid").unwrap();
    f.window("codex", T0 - DAY_MS, 10.0, T0);
    budget::sync_windows(&f.store, &f.settings, T0).unwrap();
    let start = |n, points| budget::admit_fix_start(&f.ctx(), n, "codex", points);
    assert_eq!(gate_of(start(77, 2.0)), "not_fix");
    f.store.record_fix(77, "fix", Some(3001), None, T0).unwrap();
    assert_eq!(start(77, 2.0).unwrap().unwrap().points, 2.0);
    f.store
        .record_fix(78, "revert", Some(3001), Some("x"), T0)
        .unwrap();
    assert!(
        start(78, 999.0).unwrap().is_none(),
        "revert tickets reserve nothing"
    );
    f.close_ticket(77);
    assert_eq!(gate_of(start(77, 2.0)), "not_fix");
}

#[test]
fn an_experiments_auto_start_needs_its_ticket_its_merged_plan_and_admission() {
    let f = Fixture::new("auto-start");
    f.window("codex", T0 - DAY_MS, 10.0, T0);
    budget::sync_windows(&f.store, &f.settings, T0).unwrap();
    f.planned("x-2026w43-card", 10);
    let start = |ctx: &Ctx<'_>, n, points| {
        budget::admit_experiment_start(ctx, "x-2026w43-card", n, "codex", points)
    };
    assert_eq!(gate_of(start(&f.ctx(), 99, 3.0)), "ticket");
    f.git.open(10);
    assert_eq!(gate_of(start(&f.ctx(), 11, 3.0)), "plan_merged");
    f.git.merged(10, COMMIT);
    assert_eq!(gate_of(start(&f.ctx(), 11, 30.0)), "budget");
    start(&f.ctx(), 11, 3.0).unwrap();
    let mut off = f.settings.clone();
    off.enabled = false;
    let disabled = Ctx {
        settings: &off,
        ..f.ctx()
    };
    assert_eq!(gate_of(start(&disabled, 11, 3.0)), "autopm_disabled");
}

#[test]
fn the_app_deploy_cap_refuses_the_second_autopm_deploy_and_leaves_other_agents_alone() {
    let f = Fixture::new("deploy-cap");
    f.under(3001, GOAL);
    f.under(3004, 3001);
    f.claim(3004, "app-agent", T0, true);
    let check =
        |settings: &Settings, session: &str, at| f.store.check_app_deploy(settings, session, at);
    assert_eq!(check(&f.settings, "app-agent", T0).unwrap(), Some(3004));
    f.store
        .record_app_deploy(3004, "apk1", Some("0.5.70"), T0)
        .unwrap();
    let refused = refusal(check(&f.settings, "app-agent", T0 + 1).unwrap_err());
    assert_eq!(
        (refused.status, refused.gate.as_str()),
        (429, "app_update_cap")
    );
    assert_eq!(check(&f.settings, "someone-else", T0 + 1).unwrap(), None);
    assert_eq!(
        check(&f.settings, "app-agent", T0 + 8 * DAY_MS).unwrap(),
        Some(3004),
        "a new week"
    );
    let mut off = f.settings.clone();
    off.enabled = false;
    assert_eq!(check(&off, "app-agent", T0 + 1).unwrap(), None);
}
