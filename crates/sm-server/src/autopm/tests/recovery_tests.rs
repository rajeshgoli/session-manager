use super::*;
use crate::autopm::recovery::{self, Job, Runtime};
use std::cell::{Cell, RefCell};

#[derive(Default)]
struct Effects {
    filed: RefCell<BTreeMap<String, i64>>,
    starts: RefCell<BTreeSet<i64>>,
    fail_start: Cell<bool>,
    merged: Cell<bool>,
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
    fn effects(&self, effects: Vec<Effect>) {
        self.messages.borrow_mut().extend(effects);
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
