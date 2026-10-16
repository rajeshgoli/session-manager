use super::*;
use crate::autopm::recovery::{self, Job, Runtime};
use std::cell::{Cell, RefCell};

#[derive(Default)]
struct Effects {
    filed: RefCell<BTreeMap<String, i64>>,
    starts: RefCell<BTreeSet<i64>>,
    fail_start: Cell<bool>,
    merged: Cell<bool>,
    fail_effects: Cell<bool>,
    fail_close: Cell<bool>,
    messages: RefCell<Vec<Effect>>,
}
impl Runtime for Effects {
    fn file(&self, job: &Job) -> Result<i64> {
        let mut filed = self.filed.borrow_mut();
        let next = 9000 + filed.len() as i64;
        Ok(*filed.entry(job.key.clone()).or_insert(next))
    }
    fn attach(&self, _: i64, _: i64) -> Result<()> {
        Ok(())
    }
    fn start(&self, number: i64, _: &Job) -> Result<()> {
        anyhow::ensure!(!self.fail_start.get(), "injected launch failure");
        self.starts.borrow_mut().insert(number);
        Ok(())
    }
    fn merged(&self, _: i64) -> Result<bool> {
        Ok(self.merged.get())
    }
    fn effects(&self, effects: Vec<Effect>) -> Result<()> {
        anyhow::ensure!(!self.fail_effects.get(), "injected cancellation failure");
        anyhow::ensure!(
            !self.fail_close.get()
                || !effects
                    .iter()
                    .any(|e| matches!(e, Effect::CloseIssue { .. })),
            "injected close failure"
        );
        self.messages.borrow_mut().extend(effects);
        Ok(())
    }
}

fn seed(f: &Fixture, state: &str, switched: bool) {
    f.conn().execute("INSERT INTO autopm_experiments
        (id,cycle_ticket,title,hypothesis,path,state,live_at_ms,kept_at_ms,created_at_ms,updated_at_ms,switch_key)
        VALUES('recovery',3000,'change','helps','shadow',?1,1,?2,1,1,?3)",
        params![state,T0,switched.then_some("recovery-switch")]).unwrap();
    if switched {
        f.conn().execute("INSERT INTO experiment_switches VALUES('recovery-switch','recovery',?1,0,1,'test',NULL)", [state == "kept"]).unwrap();
    }
}

#[test]
fn recovery_downvote_files_once_and_retries_launch_while_disabled() {
    let mut f = Fixture::new("recovery-downvote");
    seed(&f, "live", false);
    experiments::vote(&f.ctx(), "recovery", false, Some("worse")).unwrap();
    assert_eq!(f.get("recovery").state, "rolling_back");
    f.settings.enabled = false;
    f.settings.paused_at = Some(format_ms(T0));
    let effects = Effects::default();
    effects.fail_start.set(true);
    assert!(recovery::tick(&f.store, T0, &effects).is_err());
    assert_eq!(fix(&f.conn(), 9000).unwrap().unwrap().kind, "revert");
    effects.fail_start.set(false);
    recovery::tick(&f.store, T0, &effects).unwrap();
    recovery::tick(&f.store, T0, &effects).unwrap();
    assert_eq!(effects.filed.borrow().len(), 1);
    assert_eq!(*effects.starts.borrow(), BTreeSet::from([9000]));
    assert_eq!(f.get("recovery").state, "rolling_back");
}

#[test]
fn recovery_cleanup_due_date_and_merge_removes_switch() {
    let f = Fixture::new("recovery-kept");
    seed(&f, "kept", true);
    let effects = Effects::default();
    recovery::tick(&f.store, T0 + 30 * DAY_MS - 1, &effects).unwrap();
    assert!(effects.filed.borrow().is_empty());
    recovery::tick(&f.store, T0 + 30 * DAY_MS, &effects).unwrap();
    assert!(effects
        .filed
        .borrow()
        .keys()
        .next()
        .unwrap()
        .contains("remove-old"));
    effects.merged.set(true);
    recovery::tick(&f.store, T0 + 30 * DAY_MS, &effects).unwrap();
    assert!(switches::get(&f.conn(), "recovery-switch")
        .unwrap()
        .is_none());
    assert_eq!(f.get("recovery").state, "kept");
    recovery::tick(&f.store, T0 + 31 * DAY_MS, &effects).unwrap();
    assert_eq!(effects.filed.borrow().len(), 1);
}

#[test]
fn recovery_reenabled_cleanup_is_cancelled_and_later_revert_gets_new_ticket() {
    let f = Fixture::new("recovery-cancel");
    seed(&f, "reverted", true);
    let effects = Effects::default();
    recovery::tick(&f.store, T0, &effects).unwrap();
    f.conn()
        .execute(
            "UPDATE autopm_experiments SET state='live',live_at_ms=2",
            [],
        )
        .unwrap();
    effects.fail_effects.set(true);
    assert!(recovery::tick(&f.store, T0 + 1, &effects).is_err());
    let pending: i64 = f
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM autopm_recovery WHERE done=0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, 1);
    effects.fail_effects.set(false);
    recovery::tick(&f.store, T0 + 1, &effects).unwrap();
    assert!(fix(&f.conn(), 9000)
        .unwrap()
        .unwrap()
        .cancelled_at_ms
        .is_some());
    assert!(effects
        .messages
        .borrow()
        .iter()
        .any(|e| matches!(e, Effect::CancelAutoStart { number: 9000, .. })));
    f.set_state("recovery", "reverted");
    recovery::tick(&f.store, T0 + 2, &effects).unwrap();
    assert_eq!(effects.filed.borrow().len(), 2);
    assert!(fix(&f.conn(), 9000)
        .unwrap()
        .unwrap()
        .cancelled_at_ms
        .is_some());
    assert!(fix(&f.conn(), 9001)
        .unwrap()
        .unwrap()
        .cancelled_at_ms
        .is_none());
}

#[test]
fn recovery_kept_off_on_files_fresh_cleanup_after_cancellation_retries() {
    let f = Fixture::new("recovery-kept-resume");
    seed(&f, "kept", true);
    let effects = Effects::default();
    let now = T0 + 30 * DAY_MS;
    recovery::tick(&f.store, now, &effects).unwrap();
    f.claim(9000, "worker", now, true);
    f.conn()
        .execute("UPDATE experiment_switches SET on_=0", [])
        .unwrap();
    effects.fail_close.set(true);
    assert!(recovery::tick(&f.store, now + 1, &effects).is_err());
    assert_eq!(
        effects
            .messages
            .borrow()
            .iter()
            .filter(|e| matches!(e, Effect::Message { .. }))
            .count(),
        1
    );
    // Return before the failed close succeeds: cancellation must still finish.
    f.conn()
        .execute("UPDATE experiment_switches SET on_=1", [])
        .unwrap();
    assert!(recovery::tick(&f.store, now + 2, &effects).is_err());
    assert_eq!(effects.filed.borrow().len(), 1);
    effects.fail_close.set(false);
    recovery::tick(&f.store, now + 3, &effects).unwrap();
    recovery::tick(&f.store, now + 4, &effects).unwrap();
    recovery::tick(&f.store, now + 5, &effects).unwrap();
    assert_eq!(effects.filed.borrow().len(), 2);
    assert!(fix(&f.conn(), 9000)
        .unwrap()
        .unwrap()
        .cancelled_at_ms
        .is_some());
    assert!(fix(&f.conn(), 9001)
        .unwrap()
        .unwrap()
        .cancelled_at_ms
        .is_none());
    assert_eq!(
        effects
            .messages
            .borrow()
            .iter()
            .filter(|e| matches!(e, Effect::Message { .. }))
            .count(),
        1
    );
    assert_eq!(
        effects
            .messages
            .borrow()
            .iter()
            .filter(|e| matches!(e, Effect::CancelAutoStart { .. }))
            .count(),
        1
    );
    assert_eq!(
        effects
            .messages
            .borrow()
            .iter()
            .filter(|e| matches!(e, Effect::CloseIssue { .. }))
            .count(),
        1
    );
}

#[test]
fn recovery_kept_off_on_revives_completed_obligation_with_same_kept_time() {
    let f = Fixture::new("recovery-kept-completed");
    seed(&f, "kept", true);
    let effects = Effects::default();
    let now = T0 + 30 * DAY_MS;
    recovery::tick(&f.store, now, &effects).unwrap();
    f.conn()
        .execute("UPDATE experiment_switches SET on_=0", [])
        .unwrap();
    recovery::tick(&f.store, now + 1, &effects).unwrap();
    let done: bool = f
        .conn()
        .query_row(
            "SELECT done FROM autopm_recovery WHERE number=9000",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(done);
    f.conn()
        .execute("UPDATE experiment_switches SET on_=1", [])
        .unwrap();
    recovery::tick(&f.store, now + 2, &effects).unwrap();
    recovery::tick(&f.store, now + 3, &effects).unwrap();
    assert_eq!(f.get("recovery").kept_at_ms, Some(T0));
    assert_eq!(effects.filed.borrow().len(), 2);
    assert!(fix(&f.conn(), 9000)
        .unwrap()
        .unwrap()
        .cancelled_at_ms
        .is_some());
    assert!(fix(&f.conn(), 9001)
        .unwrap()
        .unwrap()
        .cancelled_at_ms
        .is_none());
}
