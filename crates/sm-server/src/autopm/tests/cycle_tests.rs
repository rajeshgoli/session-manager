use super::*;
use crate::autopm::cycle::{self, Actions, Cycle};
use std::cell::{Cell, RefCell};
use time::UtcOffset;

#[derive(Default)]
struct Fake {
    issues: RefCell<BTreeMap<String, i64>>,
    events: RefCell<BTreeSet<String>>,
    paused: RefCell<Option<String>>,
    fail_attach: Cell<bool>,
}
impl Actions for Fake {
    fn lane_and_policy(&self) -> Result<()> {
        self.events.borrow_mut().insert("lane+policy".into());
        Ok(())
    }
    fn issue(&self, c: &Cycle) -> Result<i64> {
        let mut issues = self.issues.borrow_mut();
        let next = 3000 + issues.len() as i64;
        Ok(*issues.entry(c.week.clone()).or_insert(next))
    }
    fn attach(&self, t: i64) -> Result<()> {
        anyhow::ensure!(!self.fail_attach.get(), "temporary link failure");
        self.events.borrow_mut().insert(format!("attach:{t}"));
        Ok(())
    }
    fn rollover(&self, a: i64, b: i64) -> Result<()> {
        self.events.borrow_mut().insert(format!("close:{a}:{b}"));
        Ok(())
    }
    fn wake(&self, k: &str, _: &str) -> Result<()> {
        self.events.borrow_mut().insert(k.into());
        Ok(())
    }
    fn start(&self, c: &Cycle) -> Result<()> {
        self.events
            .borrow_mut()
            .insert(format!("start:{}", c.ticket.unwrap()));
        Ok(())
    }
    fn pause(&self, at: &str) -> Result<()> {
        *self.paused.borrow_mut() = Some(at.into());
        Ok(())
    }
    fn pause_notice(&self, at: &str, _: &[String]) -> Result<()> {
        self.events.borrow_mut().insert(format!("notice:{at}"));
        Ok(())
    }
}
fn instant(text: &str) -> i64 {
    parse_ms(text).unwrap()
}
fn tick(f: &Fixture, a: &Fake, at: &str, skip: bool) -> Result<()> {
    cycle::tick(&f.store, &f.settings, instant(at), UtcOffset::UTC, skip, a)
}

#[test]
fn monday_boundary_iso_year_and_local_offsets() {
    let at = instant("2026-10-19T09:06:59Z");
    assert_eq!(cycle::due_week(at, UtcOffset::UTC, false), "2026-W42");
    assert_eq!(cycle::due_week(at, UtcOffset::UTC, true), "2026-W43");
    assert_eq!(
        cycle::due_week(at + 1000, UtcOffset::UTC, false),
        "2026-W43"
    );
    assert_eq!(
        cycle::due_week(instant("2027-01-04T09:07:00Z"), UtcOffset::UTC, false),
        "2027-W01"
    );
    assert_eq!(
        cycle::due_week(
            instant("2026-10-19T16:06:00Z"),
            UtcOffset::from_hms(-7, 0, 0).unwrap(),
            false
        ),
        "2026-W42"
    );
    assert_eq!(
        cycle::due_week(
            instant("2026-11-02T17:07:00Z"),
            UtcOffset::from_hms(-8, 0, 0).unwrap(),
            false
        ),
        "2026-W45"
    );
}

#[test]
fn cycles_recover_partial_creation_rollover_once_and_suppress_skipped_weeks() {
    let mut f = Fixture::new("cycles");
    let a = Fake::default();
    f.settings.enabled = false;
    tick(&f, &a, "2026-10-15T12:00:00Z", false).unwrap();
    assert!(a.issues.borrow().is_empty());
    f.settings.enabled = true;
    a.fail_attach.set(true);
    assert!(tick(&f, &a, "2026-10-15T12:00:00Z", false).is_err());
    assert!(!cycle::current(&f.store).unwrap().unwrap().ready);
    a.fail_attach.set(false);
    tick(&f, &a, "2026-10-15T12:01:00Z", false).unwrap();
    tick(&f, &a, "2026-10-19T09:06:00Z", false).unwrap();
    assert_eq!(a.issues.borrow().len(), 1);
    tick(&f, &a, "2026-10-19T09:07:00Z", false).unwrap();
    tick(&f, &a, "2026-10-19T09:08:00Z", false).unwrap();
    assert_eq!(a.issues.borrow().len(), 2);
    assert!(a.events.borrow().contains("close:3000:3001"));
    tick(&f, &a, "2026-10-26T09:07:00Z", true).unwrap();
    assert_eq!(a.issues.borrow().len(), 2);
    assert!(a.events.borrow().contains("start:3001"));
    assert!(a.events.borrow().contains("lane+policy"));
}

fn decision(f: &Fixture, at: &str, kind: &str) {
    let at = instant(at);
    f.store.open().unwrap().execute("INSERT INTO autopm_measurements(experiment_id,attempt,at_ms,status,decision) VALUES('score',?1,?1,'ok',?2)",rusqlite::params![at,kind]).unwrap();
}

#[test]
fn four_counting_cycles_pause_empty_cycles_do_not_count_and_resume_resets_score() {
    let mut f = Fixture::new("cycle-pause");
    let a = Fake::default();
    tick(&f, &a, "2026-10-05T09:07:00Z", false).unwrap();
    for (day, next) in [("06", "12"), ("13", "19"), ("20", "26")] {
        decision(&f, &format!("2026-10-{day}T12:00:00Z"), "revert");
        tick(&f, &a, &format!("2026-10-{next}T09:07:00Z"), false).unwrap();
    }
    // One empty cycle neither resets nor increments the losing run.
    tick(&f, &a, "2026-11-02T09:07:00Z", false).unwrap();
    assert!(a.paused.borrow().is_none());
    decision(&f, "2026-11-03T12:00:00Z", "revert");
    tick(&f, &a, "2026-11-09T09:07:00Z", false).unwrap();
    f.settings.paused_at = a.paused.borrow().clone();
    assert!(f.settings.paused_at.is_some());
    let count = a.issues.borrow().len();
    tick(&f, &a, "2026-11-09T09:08:00Z", false).unwrap();
    tick(&f, &a, "2026-11-09T09:09:00Z", false).unwrap();
    assert_eq!(a.issues.borrow().len(), count);
    assert_eq!(
        a.events
            .borrow()
            .iter()
            .filter(|e| e.starts_with("notice:"))
            .count(),
        1
    );
    f.settings.paused_at = None;
    tick(&f, &a, "2026-11-09T09:10:00Z", false).unwrap();
    assert_eq!(a.issues.borrow().len(), count + 1);
}

#[test]
fn keep_resets_run_and_live_measurements_delay_final_notice_without_changing_switches() {
    let mut f = Fixture::new("cycle-keep");
    let a = Fake::default();
    tick(&f, &a, "2026-10-05T09:07:00Z", false).unwrap();
    decision(&f, "2026-10-06T12:00:00Z", "revert");
    tick(&f, &a, "2026-10-12T09:07:00Z", false).unwrap();
    decision(&f, "2026-10-13T12:00:00Z", "keep");
    tick(&f, &a, "2026-10-19T09:07:00Z", false).unwrap();
    assert!(cycle::losing_run(&f.store.open().unwrap(), 0)
        .unwrap()
        .is_empty());
    f.settings.paused_at = Some("2026-10-20T00:00:00Z".into());
    let conn = f.store.open().unwrap();
    conn.execute_batch("INSERT INTO autopm_experiments(id,cycle_ticket,title,hypothesis,path,state,created_at_ms,updated_at_ms) VALUES('still-live',3000,'x','h','switch','live',1,1);
        INSERT INTO experiment_switches VALUES('autopm.x','still-live',1,0,1,'autopm',NULL);").unwrap();
    tick(&f, &a, "2026-10-20T09:00:00Z", false).unwrap();
    assert!(!a.events.borrow().iter().any(|e| e.starts_with("notice:")));
    assert!(switches::get(&conn, "autopm.x").unwrap().unwrap().on);
    conn.execute(
        "UPDATE autopm_experiments SET state='kept' WHERE id='still-live'",
        [],
    )
    .unwrap();
    tick(&f, &a, "2026-10-20T09:01:00Z", false).unwrap();
    assert!(a.events.borrow().iter().any(|e| e.starts_with("notice:")));
    assert!(switches::get(&conn, "autopm.x").unwrap().unwrap().on);
}
