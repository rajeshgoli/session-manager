use super::*;

const REPO: &str = "acme/widgets";

fn new_store() -> (WorkClaimStore, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "sm-work-claims-{}-{}",
        std::process::id(),
        OsRng.next_u64()
    ));
    fs::create_dir_all(&dir).unwrap();
    let db = dir.join("message_queue.db");
    let store = WorkClaimStore::new(db.clone());
    store.ensure_schema().unwrap();
    (store, db)
}

fn session(id: &str, parent: Option<&str>, state: HolderState) -> SessionInfo {
    SessionInfo {
        id: id.into(),
        name: format!("{id}-name"),
        parent_session_id: parent.map(Into::into),
        state,
        stopped_at: (state == HolderState::Retired).then(|| "2026-09-01T00:00:00Z".into()),
    }
}

/// lead → (eng1, eng2) siblings; eng1 → child1; other unrelated; asleep
/// stopped; gone retired.
fn directory() -> SessionDirectory {
    SessionDirectory::new([
        session("lead", None, HolderState::Idle),
        session("eng1", Some("lead"), HolderState::Working),
        session("eng2", Some("lead"), HolderState::Working),
        session("child1", Some("eng1"), HolderState::Working),
        session("other", None, HolderState::Working),
        session("asleep", None, HolderState::Stopped),
        session("gone", None, HolderState::Retired),
    ])
}

fn ticket(state: &str) -> ItemFetch {
    ItemFetch::Found(Box::new(GhItem {
        kind: WorkKind::Ticket,
        title: "Agent work claims".into(),
        state: state.into(),
        state_reason: None,
        url: "https://github.com/acme/widgets/issues/1".into(),
        head_ref: None,
        head_sha: None,
        closed_at: None,
        merged_at: None,
        closing_refs: None,
    }))
}

fn pr(state: &str, closes: &[i64]) -> ItemFetch {
    ItemFetch::Found(Box::new(GhItem {
        kind: WorkKind::Pr,
        title: "Claims core".into(),
        state: state.into(),
        state_reason: None,
        url: "https://github.com/acme/widgets/pull/9".into(),
        head_ref: Some("1452-claims".into()),
        head_sha: Some(format!("{state}-sha")),
        closed_at: None,
        merged_at: (state == "merged").then(|| "2026-09-24T00:00:00Z".into()),
        closing_refs: Some(closes.iter().map(|n| (REPO.to_owned(), *n)).collect()),
    }))
}

fn fetch(items: &[(i64, ItemFetch)]) -> Result<BatchFetch, String> {
    Ok(items.iter().cloned().collect())
}

fn request(kind: WorkKind, number: i64, claimant: &str, dir: &SessionDirectory) -> ClaimRequest {
    ClaimRequest {
        repo: REPO.into(),
        number,
        kind,
        claimant: dir.get(claimant).unwrap().clone(),
        source: ClaimSource::Explicit,
        take: false,
        worktree_path: Some("/wt".into()),
        branch: Some("1-x".into()),
        tickets: Vec::new(),
        reserve: false,
    }
}

fn claim_ticket(store: &WorkClaimStore, who: &str, dir: &SessionDirectory) -> ClaimResult {
    store
        .claim_explicit(
            &request(WorkKind::Ticket, 1, who, dir),
            fetch(&[(1, ticket("open"))]),
            dir,
        )
        .unwrap()
}

fn claim_pr(
    store: &WorkClaimStore,
    request: &ClaimRequest,
    items: &[(i64, ItemFetch)],
) -> ClaimOutcome {
    store
        .claim_explicit(request, fetch(items), &directory())
        .unwrap()
        .outcome
}

fn active(store: &WorkClaimStore, number: i64) -> Vec<WorkClaim> {
    store
        .claims_for_item(REPO, number)
        .unwrap()
        .into_iter()
        .filter(|claim| claim.ended_at.is_none())
        .collect()
}

fn ended(store: &WorkClaimStore, number: i64) -> Option<String> {
    store.claims_for_item(REPO, number).unwrap()[0]
        .end_reason
        .clone()
}

fn kinds(store: &WorkClaimStore) -> Vec<String> {
    store
        .events()
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect()
}

fn queued(db: &PathBuf, target: &str) -> Vec<String> {
    let conn = Connection::open(db).unwrap();
    if !table_exists(&conn, "message_queue").unwrap() {
        return Vec::new();
    }
    let mut statement = conn
        .prepare("SELECT text FROM message_queue WHERE target_session_id = ?1 ORDER BY queued_at")
        .unwrap();
    statement
        .query_map(params![target], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap()
}

#[test]
fn claim_then_already_held() {
    let (store, _) = new_store();
    let dir = directory();
    let first = claim_ticket(&store, "eng1", &dir);
    let ClaimOutcome::Claimed { claim, taken, .. } = first.outcome else {
        panic!("{first:?}");
    };
    assert!(!taken);
    assert_eq!(claim.source, "explicit");
    assert_eq!(claim.worktree_path.as_deref(), Some("/wt"));
    assert_eq!(claim.branch.as_deref(), Some("1-x"));
    assert_eq!(claim.session_name.as_deref(), Some("eng1-name"));
    assert_eq!(claim.parent_session_id.as_deref(), Some("lead"));
    let again = claim_ticket(&store, "eng1", &dir);
    assert!(matches!(again.outcome, ClaimOutcome::AlreadyHeld { .. }));
    assert_eq!(active(&store, 1).len(), 1);
    assert_eq!(kinds(&store), vec!["claim.taken"]);
    let item = store.item(REPO, 1).unwrap().unwrap();
    assert_eq!(item.title, "Agent work claims");
    assert!(item.synced_at.is_some());
    let views = store.claims_for_session("eng1", true).unwrap();
    assert_eq!(views[0].history_path, "/t/widgets/1");
    assert_eq!(views[0].title, "Agent work claims");
}

#[test]
fn wrong_kind_closed_merged_and_missing_are_rejected() {
    let (store, _) = new_store();
    let dir = directory();
    let reject = |kind, number, item: ItemFetch| {
        let result = store
            .claim_explicit(
                &request(kind, number, "eng1", &dir),
                fetch(&[(number, item)]),
                &dir,
            )
            .unwrap();
        match result.outcome {
            ClaimOutcome::Rejected(detail) => detail,
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(
        reject(WorkKind::Ticket, 7, pr("open", &[])),
        "#7 is a pull request; use sm pr 7."
    );
    assert_eq!(
        reject(WorkKind::Pr, 8, ticket("open")),
        "#8 is a ticket; use sm ticket 8."
    );
    assert_eq!(
        reject(WorkKind::Ticket, 9, ticket("closed")),
        "Ticket #9 is closed."
    );
    assert_eq!(
        reject(WorkKind::Pr, 10, pr("merged", &[])),
        "PR #10 is merged."
    );
    assert_eq!(
        reject(WorkKind::Pr, 11, pr("closed", &[])),
        "PR #11 is closed."
    );
    assert_eq!(
        reject(WorkKind::Ticket, 12, ItemFetch::NotFound),
        "No ticket or PR #12 in acme/widgets."
    );
    let unreachable = store
        .claim_explicit(
            &request(WorkKind::Ticket, 13, "eng1", &dir),
            Err("timed out".into()),
            &dir,
        )
        .unwrap();
    assert_eq!(
        unreachable.outcome,
        ClaimOutcome::Unreachable(
            "Could not reach GitHub to check #13: timed out. Nothing was recorded.".into()
        )
    );
    assert!(store.item(REPO, 13).unwrap().is_none());
    assert!(store.events().unwrap().is_empty());
    assert!(store.claims_for_session("eng1", false).unwrap().is_empty());
}

#[test]
fn unrelated_live_holder_refuses_and_writes_only_the_event() {
    let (store, _) = new_store();
    let dir = directory();
    claim_ticket(&store, "eng1", &dir);
    let refused = claim_ticket(&store, "other", &dir);
    let ClaimOutcome::Collision { holders } = refused.outcome else {
        panic!("{refused:?}");
    };
    assert_eq!(holders.len(), 1);
    assert_eq!(holders[0].session_id, "eng1");
    assert_eq!(holders[0].name, "eng1-name");
    assert_eq!(holders[0].state, "working");
    assert_eq!(holders[0].worktree_path.as_deref(), Some("/wt"));
    assert_eq!(active(&store, 1).len(), 1);
    assert_eq!(kinds(&store), vec!["claim.taken", "claim.refused"]);
    let refused_event = store.events().unwrap().pop().unwrap();
    assert_eq!(refused_event.session_id.as_deref(), Some("other"));
    assert_eq!(refused_event.ticket, Some(1));
    assert_eq!(refused_event.payload["holder_session_ids"], json!(["eng1"]));
}

#[test]
fn siblings_collide_but_parent_and_child_share() {
    let (store, _) = new_store();
    let dir = directory();
    claim_ticket(&store, "eng1", &dir);
    assert!(matches!(
        claim_ticket(&store, "eng2", &dir).outcome,
        ClaimOutcome::Collision { .. }
    ));
    let child = claim_ticket(&store, "child1", &dir);
    let ClaimOutcome::Claimed { notes, .. } = child.outcome else {
        panic!("{child:?}");
    };
    assert_eq!(notes, vec!["Also held by your parent eng1-name (eng1)."]);
    let lead = claim_ticket(&store, "lead", &dir);
    let ClaimOutcome::Claimed { notes, .. } = lead.outcome else {
        panic!("{lead:?}");
    };
    assert_eq!(
        notes,
        vec![
            "Also held by your child eng1-name (eng1).",
            "Also held by your descendant child1-name (child1)."
        ]
    );
    assert_eq!(active(&store, 1).len(), 3);
}

#[test]
fn take_ends_the_holder_and_queues_the_exact_text() {
    let (store, db) = new_store();
    let dir = directory();
    claim_ticket(&store, "eng1", &dir);
    let mut take = request(WorkKind::Ticket, 1, "other", &dir);
    take.take = true;
    let result = store
        .claim_explicit(&take, fetch(&[(1, ticket("open"))]), &dir)
        .unwrap();
    assert_eq!(result.notified, vec!["eng1"]);
    let ClaimOutcome::Claimed { taken, notes, .. } = result.outcome else {
        panic!("{result:?}");
    };
    assert!(taken);
    assert_eq!(notes, vec!["Took ticket #1 from eng1-name (eng1)."]);
    let claims = store.claims_for_item(REPO, 1).unwrap();
    assert_eq!(claims[0].end_reason.as_deref(), Some("taken"));
    assert_eq!(claims[0].ended_by_session_id.as_deref(), Some("other"));
    assert_eq!(
        queued(&db, "eng1"),
        vec!["[sm claim] other-name (other) claimed ticket #1. Your claim on it ended."]
    );
    assert_eq!(
        kinds(&store),
        vec!["claim.taken", "claim.released", "claim.taken"]
    );
}

#[test]
fn take_by_a_caller_who_already_holds_ends_the_other_holders() {
    let (store, _) = new_store();
    let dir = directory();
    claim_pr(
        &store,
        &request(WorkKind::Pr, 9, "eng1", &dir),
        &[(9, pr("open", &[]))],
    );
    // An implicit claim never blocks: it records the collision and warns.
    let warnings = store
        .claim_implicit(
            REPO,
            9,
            dir.get("other").unwrap(),
            ClaimSource::CodexReview,
            &dir,
        )
        .unwrap();
    assert_eq!(
        warnings,
        Some(vec![
            "Warning: PR #9 is also held by eng1-name (eng1), working.".into()
        ])
    );
    assert_eq!(kinds(&store).last().unwrap(), "claim.collision");
    assert_eq!(active(&store, 9).len(), 2);
    let mut take = request(WorkKind::Pr, 9, "other", &dir);
    take.take = true;
    let ClaimOutcome::AlreadyHeld { notes, .. } = claim_pr(&store, &take, &[(9, pr("open", &[]))])
    else {
        panic!();
    };
    assert!(notes.contains(&"Took PR #9 from eng1-name (eng1).".to_owned()));
    let held: Vec<_> = active(&store, 9)
        .into_iter()
        .map(|c| c.session_id)
        .collect();
    assert_eq!(held, vec!["other"]);
    // The implicit claim, now explicit, gets the worktree it lacked.
    assert_eq!(active(&store, 9)[0].worktree_path.as_deref(), Some("/wt"));
}

#[test]
fn dormant_holder_is_superseded_with_a_queued_message() {
    let (store, db) = new_store();
    let dir = directory();
    claim_ticket(&store, "asleep", &dir);
    let result = claim_ticket(&store, "other", &dir);
    let ClaimOutcome::Claimed { notes, .. } = result.outcome else {
        panic!("{result:?}");
    };
    assert_eq!(
        notes,
        vec!["Previous holder asleep-name (asleep) is stopped; its claim ended."]
    );
    assert_eq!(
        queued(&db, "asleep"),
        vec!["[sm claim] While you were stopped, other-name (other) claimed ticket #1. Your claim on it ended."]
    );
    assert_eq!(ended(&store, 1).as_deref(), Some("superseded"));
}

#[test]
fn a_retired_holder_never_blocks() {
    let (store, db) = new_store();
    claim_ticket(
        &store,
        "gone",
        &SessionDirectory::new([session("gone", None, HolderState::Working)]),
    );
    assert!(matches!(
        claim_ticket(&store, "other", &directory()).outcome,
        ClaimOutcome::Claimed { .. }
    ));
    assert_eq!(ended(&store, 1).as_deref(), Some("retired"));
    assert!(queued(&db, "gone").is_empty());
}

#[test]
fn release_and_the_unique_active_index() {
    let (store, db) = new_store();
    let dir = directory();
    claim_ticket(&store, "eng1", &dir);
    assert!(store
        .release("eng2", REPO, 1, WorkKind::Ticket)
        .unwrap()
        .is_none());
    assert!(store
        .release("eng1", REPO, 1, WorkKind::Pr)
        .unwrap()
        .is_none());
    let released = store
        .release("eng1", REPO, 1, WorkKind::Ticket)
        .unwrap()
        .unwrap();
    assert_eq!(released.end_reason.as_deref(), Some("released"));
    assert!(store
        .release("eng1", REPO, 1, WorkKind::Ticket)
        .unwrap()
        .is_none());
    // Claiming again after release is a new row.
    claim_ticket(&store, "eng1", &dir);
    assert_eq!(store.claims_for_item(REPO, 1).unwrap().len(), 2);
    let conn = Connection::open(&db).unwrap();
    let duplicate = conn.execute(
        "INSERT INTO work_claims (id, repo, number, kind, session_id, source, claimed_at)
         VALUES ('dupe0001', ?1, 1, 'ticket', 'eng1', 'explicit', 'now')",
        params![REPO],
    );
    assert!(
        duplicate.is_err(),
        "two active rows for one session and item"
    );
}

#[test]
fn a_bad_ticket_flag_aborts_the_claim_and_writes_no_link() {
    let (store, _) = new_store();
    let dir = directory();
    for (bad, detail) in [
        (ticket("closed"), "Ticket #2 is closed."),
        (pr("open", &[]), "#2 is a pull request; use sm pr 2."),
        (ItemFetch::NotFound, "No ticket or PR #2 in acme/widgets."),
    ] {
        let mut claim = request(WorkKind::Pr, 9, "eng1", &dir);
        claim.tickets = vec![2];
        assert_eq!(
            claim_pr(&store, &claim, &[(9, pr("open", &[])), (2, bad)]),
            ClaimOutcome::Rejected(detail.into())
        );
        assert!(active(&store, 9).is_empty());
        assert!(store.links_for_pr(REPO, 9).unwrap().is_empty());
    }
}

#[test]
fn spawn_reservation_confirms_or_is_deleted_and_recovers_after_a_crash() {
    let (store, db) = new_store();
    let mut dir = directory();
    // Two children spawned onto one ticket are siblings and collide, so each
    // reservation below after the first takes its own ticket.
    let reserve = |id: &str, number: i64, dir: &SessionDirectory| {
        let mut claim = request(WorkKind::Ticket, number, "eng1", dir);
        claim.claimant = session(id, Some("eng1"), HolderState::Working);
        claim.source = ClaimSource::Spawn;
        claim.reserve = true;
        match store
            .claim_explicit(&claim, fetch(&[(number, ticket("open"))]), dir)
            .unwrap()
            .outcome
        {
            ClaimOutcome::Claimed { claim, .. } => claim,
            other => panic!("{other:?}"),
        }
    };
    // The spawner holds the ticket: its child shares it.
    claim_ticket(&store, "eng1", &dir);
    let claim = reserve("newkid01", 1, &dir);
    assert_eq!(claim.source, "spawn");
    assert_eq!(claim.parent_session_id.as_deref(), Some("eng1"));
    assert!(claim.reserved_at.is_some());
    // A reservation blocks an unrelated claimant but is not listed.
    let ClaimOutcome::Collision { holders } = claim_ticket(&store, "other", &dir).outcome else {
        panic!()
    };
    assert_eq!(holders.len(), 2);
    assert!(store
        .claims_for_session("newkid01", true)
        .unwrap()
        .is_empty());
    assert_eq!(
        kinds(&store).iter().filter(|k| *k == "claim.taken").count(),
        1
    );
    // A second child on the same ticket is its sibling: refused.
    let mut sibling = request(WorkKind::Ticket, 1, "eng1", &dir);
    sibling.claimant = session("newkid09", Some("eng1"), HolderState::Working);
    sibling.reserve = true;
    assert!(matches!(
        store
            .claim_explicit(&sibling, fetch(&[(1, ticket("open"))]), &dir)
            .unwrap()
            .outcome,
        ClaimOutcome::Collision { .. }
    ));
    store.confirm_reservation(&claim.id).unwrap();
    dir.insert(session("newkid01", Some("eng1"), HolderState::Working));
    assert_eq!(store.claims_for_session("newkid01", true).unwrap().len(), 1);
    assert_eq!(
        kinds(&store).iter().filter(|k| *k == "claim.taken").count(),
        2
    );

    let failed = reserve("newkid02", 2, &dir);
    store.delete_reservation(&failed.id).unwrap();
    assert!(store.claim(&failed.id).unwrap().is_none());

    // A crash between reservation and confirm: settled at the next start.
    let made = reserve("newkid03", 3, &dir);
    let lost = reserve("newkid04", 4, &dir);
    assert_eq!(
        store.recover_reservations(|_| true).unwrap(),
        (0, 0),
        "too young"
    );
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE work_claims SET reserved_at = '2026-01-01T00:00:00Z' WHERE reserved_at IS NOT NULL",
            [],
        )
        .unwrap();
    assert_eq!(
        store.recover_reservations(|id| id == "newkid03").unwrap(),
        (1, 1)
    );
    assert!(store
        .claim(&made.id)
        .unwrap()
        .unwrap()
        .reserved_at
        .is_none());
    assert!(store.claim(&lost.id).unwrap().is_none());
}

#[test]
fn sync_ends_claims_on_close_and_merge_and_freezes_the_head() {
    let (store, _) = new_store();
    let dir = directory();
    claim_ticket(&store, "eng1", &dir);
    claim_pr(
        &store,
        &request(WorkKind::Pr, 9, "eng1", &dir),
        &[(9, pr("open", &[]))],
    );
    store
        .record_fetch(
            REPO,
            &[1, 9],
            &fetch(&[(1, ticket("closed")), (9, pr("merged", &[]))]),
        )
        .unwrap();
    assert_eq!(ended(&store, 1).as_deref(), Some("closed"));
    assert_eq!(ended(&store, 9).as_deref(), Some("merged"));
    let merged = store.item(REPO, 9).unwrap().unwrap();
    assert_eq!(merged.head_sha.as_deref(), Some("merged-sha"));
    assert_eq!(merged.merge_check.as_deref(), Some("pending"));
    // A later fetch never moves a merged head.
    let mut moved = pr("merged", &[]);
    if let ItemFetch::Found(item) = &mut moved {
        item.head_sha = Some("later".into());
    }
    store
        .record_fetch(REPO, &[9], &fetch(&[(9, moved)]))
        .unwrap();
    assert_eq!(
        store.item(REPO, 9).unwrap().unwrap().head_sha.as_deref(),
        Some("merged-sha")
    );
    let changes: Vec<_> = store
        .events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "github.state_changed")
        .map(|e| (e.ticket, e.pr, e.payload["to"].clone()))
        .collect();
    // The PR links to exactly one ticket, so its event carries it.
    assert_eq!(
        changes,
        vec![
            (Some(1), None, json!("closed")),
            (Some(1), Some(9), json!("merged"))
        ]
    );
    // Closed and merged items are not re-polled.
    assert!(store.tracked_items().unwrap().is_empty());
    // Reopening does not revive the claim.
    store
        .record_fetch(REPO, &[1], &fetch(&[(1, ticket("open"))]))
        .unwrap();
    assert!(active(&store, 1).is_empty());
}

#[test]
fn retire_ends_claims_and_the_sweep_catches_a_missed_hook() {
    let (store, _) = new_store();
    let dir = directory();
    claim_ticket(&store, "eng1", &dir);
    assert_eq!(store.end_claims_for_session("eng1").unwrap(), 1);
    assert_eq!(ended(&store, 1).as_deref(), Some("retired"));
    claim_ticket(&store, "eng2", &dir);
    let mut retired = directory();
    retired.insert(session("eng2", Some("lead"), HolderState::Retired));
    assert_eq!(store.end_claims_of_retired_sessions(&retired).unwrap(), 1);
    // A stopped, restorable holder keeps its (dormant) claim.
    claim_ticket(&store, "asleep", &dir);
    assert_eq!(store.end_claims_of_retired_sessions(&dir).unwrap(), 0);
    assert_eq!(active(&store, 1).len(), 1);
}

#[test]
fn first_sight_is_not_a_transition_and_errors_mark_items() {
    let (store, _) = new_store();
    let dir = directory();
    store
        .claim_implicit(
            REPO,
            9,
            dir.get("eng1").unwrap(),
            ClaimSource::DocPublish,
            &dir,
        )
        .unwrap();
    let stub = store.item(REPO, 9).unwrap().unwrap();
    assert!(stub.synced_at.is_none());
    assert_eq!(
        store.tracked_items().unwrap(),
        BTreeMap::from([(REPO.to_owned(), vec![9])])
    );
    // Stub first fetched merged: the claim ends, but no transition.
    store
        .record_fetch(REPO, &[9], &fetch(&[(9, pr("merged", &[]))]))
        .unwrap();
    assert_eq!(ended(&store, 9).as_deref(), Some("merged"));
    assert_eq!(store.item(REPO, 9).unwrap().unwrap().merge_check, None);
    assert!(!kinds(&store).contains(&"github.state_changed".to_owned()));

    claim_ticket(&store, "eng1", &dir);
    store
        .record_fetch(REPO, &[1], &fetch(&[(1, ItemFetch::NotFound)]))
        .unwrap();
    let item = store.item(REPO, 1).unwrap().unwrap();
    assert_eq!(item.sync_error.as_deref(), Some("not found"));
    assert_eq!(item.state, "open");
    store
        .record_fetch(REPO, &[1], &Err("bad JSON".into()))
        .unwrap();
    assert_eq!(
        store.item(REPO, 1).unwrap().unwrap().sync_error.as_deref(),
        Some("bad JSON")
    );
    store
        .record_fetch(REPO, &[1], &fetch(&[(1, ticket("open"))]))
        .unwrap();
    assert_eq!(store.item(REPO, 1).unwrap().unwrap().sync_error, None);
}

#[test]
fn parsing_handles_partial_failures_pages_and_other_repos() {
    let numbers = [1449, 1460, 99999];
    let stdout = json!({
        "data": {"repository": {
            "i1449": {"__typename": "Issue", "title": "Owner docs", "state": "OPEN",
                      "stateReason": null, "closedAt": null, "url": "u1449"},
            "i1460": {"__typename": "PullRequest", "title": "Docs", "state": "MERGED",
                      "mergedAt": "2026-09-24T00:00:00Z", "closedAt": "2026-09-24T00:00:00Z",
                      "url": "u1460", "headRefName": "1449-docs", "headRefOid": "abc",
                      "closingIssuesReferences": {"totalCount": 22,
                        "pageInfo": {"hasNextPage": true, "endCursor": "C1"},
                        "nodes": [{"number": 1449, "repository": {"nameWithOwner": "acme/widgets"}},
                                  {"number": 5, "repository": {"nameWithOwner": "acme/other"}}]}},
            "i99999": null}},
        "errors": [{"type": "NOT_FOUND", "path": ["repository", "i99999"],
                    "message": "Could not resolve"}]
    })
    .to_string();
    let (batch, more) = parse_items_response(stdout.as_bytes(), &numbers).unwrap();
    assert_eq!(batch[&99999], ItemFetch::NotFound);
    let ItemFetch::Found(issue) = &batch[&1449] else {
        panic!()
    };
    assert_eq!(
        (issue.kind, issue.state.as_str()),
        (WorkKind::Ticket, "open")
    );
    let ItemFetch::Found(merged) = &batch[&1460] else {
        panic!()
    };
    assert_eq!(merged.state, "merged");
    assert_eq!(merged.head_sha.as_deref(), Some("abc"));
    assert_eq!(more, vec![(1460, "C1".to_owned())]);
    assert!(parse_items_response(b"gh: not json", &numbers).is_err());

    let page = json!({"data": {"repository": {"pullRequest": {"closingIssuesReferences": {
        "pageInfo": {"hasNextPage": false, "endCursor": "C2"},
        "nodes": [{"number": 7, "repository": {"nameWithOwner": "acme/widgets"}}]}}}}});
    let (refs, next) = parse_closing_refs_page(page.to_string().as_bytes()).unwrap();
    assert_eq!((refs, next), (vec![("acme/widgets".to_owned(), 7)], None));

    let query = items_query("acme/widgets", &[1, 2]);
    assert!(query.contains("i1: issueOrPullRequest(number: 1) { ...F }"));
    assert!(query.contains("repository(owner: \"acme\", name: \"widgets\")"));

    // Closing references from another repo never become links.
    let (store, _) = new_store();
    let mut item = pr("open", &[]);
    if let ItemFetch::Found(item) = &mut item {
        item.closing_refs = Some(vec![("acme/other".into(), 5), (REPO.into(), 3)]);
    }
    store
        .record_fetch(REPO, &[9], &fetch(&[(9, item)]))
        .unwrap();
    assert_eq!(
        store.links_for_pr(REPO, 9).unwrap(),
        vec![(3, "closing_ref".to_owned())]
    );
    assert!(store.item(REPO, 5).unwrap().is_none());
    assert!(store.item(REPO, 3).unwrap().unwrap().synced_at.is_none());
}

#[test]
fn closing_ref_links_follow_the_body_and_claim_links_survive() {
    let (store, _) = new_store();
    let dir = directory();
    let sync = |closes: &[i64]| {
        store
            .record_fetch(REPO, &[9], &fetch(&[(9, pr("open", closes))]))
            .unwrap()
    };
    sync(&[1, 2]);
    // #2 is also linked by claim.
    let mut claim = request(WorkKind::Pr, 9, "eng1", &dir);
    claim.tickets = vec![2];
    claim_pr(
        &store,
        &claim,
        &[(9, pr("open", &[1, 2])), (2, ticket("open"))],
    );
    // "Closes #1" and "Closes #2" removed before merge.
    sync(&[]);
    assert_eq!(
        store.links_for_pr(REPO, 9).unwrap(),
        vec![(2, "claim".to_owned())]
    );
    let link_events: Vec<_> = store
        .events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind.starts_with("link."))
        .map(|e| (e.kind, e.ticket))
        .collect();
    assert_eq!(
        link_events,
        vec![
            ("link.added".to_owned(), Some(1)),
            ("link.added".to_owned(), Some(2)),
            ("link.removed".to_owned(), Some(1)),
        ]
    );
    // An incomplete or failed fetch leaves links as they were.
    sync(&[1]);
    let mut incomplete = pr("open", &[]);
    if let ItemFetch::Found(item) = &mut incomplete {
        item.closing_refs = None;
    }
    store
        .record_fetch(REPO, &[9], &fetch(&[(9, incomplete)]))
        .unwrap();
    store.record_fetch(REPO, &[9], &Err("down".into())).unwrap();
    assert_eq!(store.links_for_pr(REPO, 9).unwrap().len(), 2);
}

#[test]
fn pr_claims_link_held_tickets_and_note_missing_closing_refs() {
    let (store, _) = new_store();
    let dir = directory();
    claim_ticket(&store, "eng1", &dir);
    // One held ticket, not in the PR's closing references: linked, noted.
    let ClaimOutcome::Claimed { notes, .. } = claim_pr(
        &store,
        &request(WorkKind::Pr, 9, "eng1", &dir),
        &[(9, pr("open", &[]))],
    ) else {
        panic!()
    };
    assert_eq!(
        notes,
        vec!["Note: PR #9's closing references don't include ticket #1."]
    );
    assert_eq!(
        store.links_for_pr(REPO, 9).unwrap(),
        vec![(1, "claim".to_owned())]
    );
    let taken = store
        .events()
        .unwrap()
        .into_iter()
        .rfind(|e| e.kind == "claim.taken")
        .unwrap();
    assert_eq!((taken.ticket, taken.pr), (Some(1), Some(9)));

    // Closed by the PR: no note.
    let (store, _) = new_store();
    claim_ticket(&store, "eng1", &dir);
    let ClaimOutcome::Claimed { notes, .. } = claim_pr(
        &store,
        &request(WorkKind::Pr, 9, "eng1", &dir),
        &[(9, pr("open", &[1]))],
    ) else {
        panic!()
    };
    assert!(notes.is_empty(), "{notes:?}");

    // Several held and none named: no link, a note. --ticket on a rerun
    // links exactly the one named and fills a NULL worktree.
    let (store, _) = new_store();
    claim_ticket(&store, "eng1", &dir);
    store
        .claim_explicit(
            &request(WorkKind::Ticket, 2, "eng1", &dir),
            fetch(&[(2, ticket("open"))]),
            &dir,
        )
        .unwrap();
    let mut first = request(WorkKind::Pr, 9, "eng1", &dir);
    first.worktree_path = None;
    first.branch = None;
    let ClaimOutcome::Claimed { notes, .. } = claim_pr(&store, &first, &[(9, pr("open", &[]))])
    else {
        panic!()
    };
    assert_eq!(
        notes,
        vec!["You hold tickets #1, #2; PR #9 is linked to neither (--ticket links one)."]
    );
    assert!(store.links_for_pr(REPO, 9).unwrap().is_empty());
    let mut rerun = request(WorkKind::Pr, 9, "eng1", &dir);
    rerun.tickets = vec![2];
    let ClaimOutcome::AlreadyHeld { notes, claim } = claim_pr(
        &store,
        &rerun,
        &[(9, pr("open", &[2])), (2, ticket("open"))],
    ) else {
        panic!()
    };
    assert_eq!(notes, vec!["Linked PR #9 to ticket #2."]);
    assert_eq!(claim.worktree_path.as_deref(), Some("/wt"));
    assert_eq!(
        store.links_for_pr(REPO, 9).unwrap(),
        vec![(2, "claim".to_owned()), (2, "closing_ref".to_owned())]
    );
}

#[test]
fn implicit_claims_never_link_skip_closed_prs_and_are_idempotent() {
    let (store, _) = new_store();
    let dir = directory();
    claim_ticket(&store, "eng1", &dir);
    let eng1 = dir.get("eng1").unwrap();
    assert_eq!(
        store
            .claim_implicit(REPO, 9, eng1, ClaimSource::CodexReview, &dir)
            .unwrap(),
        Some(Vec::new())
    );
    assert!(store.links_for_pr(REPO, 9).unwrap().is_empty());
    assert_eq!(active(&store, 9)[0].source, "codex_review");
    assert_eq!(active(&store, 9)[0].worktree_path, None);
    assert_eq!(
        store
            .claim_implicit(REPO, 9, eng1, ClaimSource::DocPublish, &dir)
            .unwrap(),
        None
    );
    store
        .record_fetch(REPO, &[10], &fetch(&[(10, pr("closed", &[]))]))
        .unwrap();
    assert_eq!(
        store
            .claim_implicit(REPO, 10, eng1, ClaimSource::CodexReview, &dir)
            .unwrap(),
        None
    );
    assert!(store.claims_for_item(REPO, 10).unwrap().is_empty());
}

fn seed_history(db: &PathBuf) {
    let conn = Connection::open(db).unwrap();
    crate::owner_docs::init_owner_docs_schema(&conn).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS codex_review_request_registrations (
            id TEXT PRIMARY KEY, repo TEXT NOT NULL, pr_number INTEGER NOT NULL,
            requester_session_id TEXT, requested_at TIMESTAMP NOT NULL);
        INSERT INTO codex_review_request_registrations VALUES
            ('r1', 'acme/widgets', 9, 'eng1', '2026-09-02T00:00:00Z'),
            ('r2', 'acme/widgets', 9, 'eng1', '2026-09-01T00:00:00Z'),
            ('r3', 'acme/widgets', 9, 'gone', '2026-09-03T00:00:00Z'),
            ('r4', 'acme/widgets', 11, NULL, '2026-09-03T00:00:00Z');
        INSERT INTO owner_docs (id, repo, path, pr_number, author_session_id, title,
                                created_at, updated_at)
            VALUES ('d0000001', 'acme/widgets', 'specs/m.html', 12, 'eng1', 'M', 't', 't');
        INSERT INTO owner_doc_publishes (doc_id, commit_sha, blob_sha, session_id, published_at)
            VALUES ('d0000001', 'c', 'b', 'eng1', '2026-09-04T00:00:00Z'),
                   ('d0000001', 'c', 'b', 'eng2', '2026-09-05T00:00:00Z');
        "#,
    )
    .unwrap();
}

#[test]
fn backfill_runs_once_ends_retired_claims_and_is_fetched_by_the_first_sync() {
    let (store, db) = new_store();
    let dir = directory();
    seed_history(&db);
    assert!(store.backfill(&dir).unwrap());
    assert!(!store.backfill(&dir).unwrap(), "second start does nothing");
    let pr9 = store.claims_for_item(REPO, 9).unwrap();
    assert_eq!(pr9.len(), 2);
    let eng1 = pr9.iter().find(|c| c.session_id == "eng1").unwrap();
    assert_eq!(eng1.claimed_at, "2026-09-01T00:00:00Z", "earliest request");
    assert_eq!(eng1.source, "backfill");
    assert!(eng1.ended_at.is_none());
    let gone = pr9.iter().find(|c| c.session_id == "gone").unwrap();
    assert_eq!(gone.end_reason.as_deref(), Some("retired"));
    assert_eq!(gone.ended_at.as_deref(), Some("2026-09-01T00:00:00Z"));
    // Every publisher of a republished doc gets a claim.
    let pr12: Vec<_> = store
        .claims_for_item(REPO, 12)
        .unwrap()
        .into_iter()
        .map(|c| c.session_id)
        .collect();
    assert_eq!(pr12, vec!["eng1", "eng2"]);
    assert!(store.claims_for_item(REPO, 11).unwrap().is_empty());
    // No messages, no events.
    assert!(store.events().unwrap().is_empty());
    assert!(queued(&db, "eng1").is_empty());
    // Stubs are fetched whatever their claims; the first fetch sees #9
    // merged (no transition), and its closing ticket is fetched next pass.
    assert_eq!(store.tracked_items().unwrap()[REPO], vec![9, 12]);
    store
        .record_fetch(
            REPO,
            &[9, 12],
            &fetch(&[(9, pr("merged", &[4])), (12, pr("open", &[]))]),
        )
        .unwrap();
    assert_eq!(store.item(REPO, 9).unwrap().unwrap().merge_check, None);
    assert!(store.tracked_items().unwrap()[REPO].contains(&4));
    // The watermarks moved past the backfilled rows.
    assert_eq!(store.reconcile_implicit(&dir).unwrap(), 0);
}

#[test]
fn reconciliation_records_implicit_claims_whose_hook_failed() {
    let (store, db) = new_store();
    let dir = directory();
    store.backfill(&dir).unwrap();
    seed_history(&db);
    // eng1 and gone on #9, eng1 and eng2 on #12.
    assert_eq!(store.reconcile_implicit(&dir).unwrap(), 4);
    let pr9 = store.claims_for_item(REPO, 9).unwrap();
    assert_eq!(pr9.iter().filter(|c| c.ended_at.is_none()).count(), 1);
    assert_eq!(
        pr9.iter()
            .find(|c| c.session_id == "gone")
            .unwrap()
            .end_reason
            .as_deref(),
        Some("retired")
    );
    assert_eq!(store.reconcile_implicit(&dir).unwrap(), 0, "idempotent");
}
