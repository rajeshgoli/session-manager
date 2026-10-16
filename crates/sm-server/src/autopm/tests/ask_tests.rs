use super::*;
use crate::autopm::ask::{self, Decision, Effects};
use rusqlite::params;
use std::cell::{Cell, RefCell};

#[derive(Default)]
struct Fake {
    messages: RefCell<BTreeSet<String>>,
    clarifications: RefCell<BTreeSet<String>>,
    proposals: RefCell<BTreeSet<String>>,
    fail: Cell<bool>,
}
impl Effects for Fake {
    fn message(&self, key: &str, _: &str, _: &str, _: &str) -> Result<String> {
        self.messages.borrow_mut().insert(key.into());
        Ok(format!("message-{key}"))
    }
    fn clarify(&self, key: &str, _: &str) -> Result<()> {
        self.clarifications.borrow_mut().insert(key.into());
        Ok(())
    }
    fn proposal(&self, x: &Experiment) -> Result<(String, i64)> {
        anyhow::ensure!(!self.fail.get(), "temporary GitHub failure");
        self.proposals.borrow_mut().insert(x.id.clone());
        Ok((x.target_repo.clone().unwrap(), 42))
    }
}
fn seed(f: &Fixture, id: &str, path: &str) -> Decision {
    f.store.open().unwrap().execute("INSERT INTO autopm_experiments(id,cycle_ticket,title,hypothesis,path,target_repo,proposal_md,state,created_at_ms,updated_at_ms) VALUES(?1,3001,'Title','Hypothesis',?2,'owner/other','Proposal text','awaiting_owner',?3,?3)",params![id,path,f.now]).unwrap();
    Decision {
        experiment_id: id.into(),
        text: format!("Change {id}. Shadow evidence is recorded. Yes ships; no abandons."),
    }
}

#[test]
fn strict_numbered_parser_rejects_ambiguity_and_accepts_partial_answers() {
    assert_eq!(
        ask::parse("1 yes, 2 no", 3),
        Some(BTreeMap::from([(1, true), (2, false)]))
    );
    assert_eq!(ask::parse("YES", 1), Some(BTreeMap::from([(1, true)])));
    assert_eq!(
        ask::parse("2: no; 1. YES.", 2),
        Some(BTreeMap::from([(1, true), (2, false)]))
    );
    for bad in [
        "yes",
        "1 yes maybe",
        "1 yes, 1 no",
        "4 yes",
        "2 yes",
        "1 yes please",
        "11 yes",
        "1 yesterday",
        "",
    ] {
        let count = if bad == "yes" { 2 } else { 1 };
        assert!(ask::parse(bad, count).is_none(), "{bad}");
    }
}

#[test]
fn ask_limits_once_per_cycle_and_no_repeat_experiment() {
    let f = Fixture::new("asks");
    let effects = Fake::default();
    let a = seed(&f, "a", "shadow");
    let id = ask::send(&f.ctx(), 3001, "pm", std::slice::from_ref(&a), &effects).unwrap();
    assert_eq!(
        id,
        ask::send(
            &f.ctx(),
            3001,
            "successor",
            std::slice::from_ref(&a),
            &effects
        )
        .unwrap()
    );
    assert_eq!(effects.messages.borrow().len(), 1);
    let b = seed(&f, "b", "proposal");
    assert!(ask::send(&f.ctx(), 3001, "pm", &[b], &effects).is_err());
    assert!(ask::send(&f.ctx(), 3002, "pm", std::slice::from_ref(&a), &effects).is_err());
    assert!(
        matches!(ask::validate(&vec![a.clone();4]),Err(Fail::Refused(r)) if r.status==400 && r.gate=="too_many_decisions")
    );
    assert!(ask::validate(&[]).is_err());
    assert!(ask::validate(&[Decision {
        experiment_id: "long".into(),
        text: "word ".repeat(121)
    }])
    .is_err());
}

#[test]
fn reply_is_idempotent_partial_invalid_replies_do_not_decide_and_timely_answers_survive_downtime() {
    let mut f = Fixture::new("answers");
    let e = Fake::default();
    let a = seed(&f, "a", "shadow");
    let b = seed(&f, "b", "shadow");
    let c = seed(&f, "c", "shadow");
    let id = ask::send(&f.ctx(), 3001, "pm", &[a, b, c], &e).unwrap();
    let sent = f.now;
    ask::reply(&f.ctx(), &id, "bad", "1 yes maybe", sent + 1, &e).unwrap();
    ask::reply(&f.ctx(), &id, "bad", "1 yes maybe", sent + 1, &e).unwrap();
    assert_eq!(e.clarifications.borrow().len(), 1);
    assert_eq!(
        experiments::get(&f.store.open().unwrap(), "a")
            .unwrap()
            .unwrap()
            .state,
        "awaiting_owner"
    );
    // Replay a reply that arrived before expiry after a 15-day server outage.
    f.now += 15 * DAY_MS;
    ask::reply(&f.ctx(), &id, "reply1", "1 yes, 2 no", sent + DAY_MS, &e).unwrap();
    ask::reply(&f.ctx(), &id, "reply1", "1 yes, 2 no", sent + DAY_MS, &e).unwrap();
    ask::apply(&f.ctx(), &e).unwrap();
    ask::apply(&f.ctx(), &e).unwrap();
    let conn = f.store.open().unwrap();
    assert_eq!(
        experiments::get(&conn, "a").unwrap().unwrap().state,
        "building"
    );
    assert_eq!(
        experiments::get(&conn, "b")
            .unwrap()
            .unwrap()
            .decision_note
            .as_deref(),
        Some("owner said no")
    );
    assert_eq!(
        experiments::get(&conn, "c")
            .unwrap()
            .unwrap()
            .decision_note
            .as_deref(),
        Some("no answer")
    );
    ask::reply(&f.ctx(), &id, "late", "3 yes", f.now, &e).unwrap();
    ask::apply(&f.ctx(), &e).unwrap();
    assert_eq!(
        experiments::get(&conn, "c").unwrap().unwrap().state,
        "abandoned"
    );
}

#[test]
fn approved_proposal_files_target_repo_before_abandoning_and_retries_failure() {
    let f = Fixture::new("proposal");
    let e = Fake::default();
    let d = seed(&f, "p", "proposal");
    let id = ask::send(&f.ctx(), 3001, "pm", &[d], &e).unwrap();
    ask::reply(&f.ctx(), &id, "answer", "yes", f.now + 1, &e).unwrap();
    e.fail.set(true);
    assert!(ask::apply(&f.ctx(), &e).is_err());
    assert_eq!(
        experiments::get(&f.store.open().unwrap(), "p")
            .unwrap()
            .unwrap()
            .state,
        "awaiting_owner"
    );
    e.fail.set(false);
    ask::apply(&f.ctx(), &e).unwrap();
    ask::apply(&f.ctx(), &e).unwrap();
    let x = experiments::get(&f.store.open().unwrap(), "p")
        .unwrap()
        .unwrap();
    assert_eq!(x.state, "abandoned");
    assert_eq!(x.decision_note.as_deref(), Some("handed to owner/other#42"));
    assert_eq!(e.proposals.borrow().len(), 1);
}
