use super::*;

fn ticket(number: i64, state: &str, title: &str) -> Value {
    json!({
        "repo": "rajeshgoli/session-manager", "number": number, "title": title,
        "state": state, "done_reason": null, "needs_you": null, "waits_on": [],
        "holder": null, "prs": [], "warnings": [], "new": false, "also_in": [],
        "sub_issues_done": false,
    })
}

#[test]
fn cli_output_lines() {
    let mut blocked = ticket(1651, "blocked", "Context handoff");
    blocked["waits_on"] = json!([
        {"repo": "rajeshgoli/session-manager", "number": 1654, "state": "ready"},
        {"repo": "rajeshgoli/session-manager", "number": 1653, "state": "done"},
        {"repo": "rajeshgoli/fractal-algo-rust", "number": 1774, "state": "blocked"},
    ]);
    let mut held = ticket(1660, "in_progress", "Shared settings endpoint");
    held["holder"] = json!({"session_id": "a1", "name": "sm-1660-engineer", "state": "stopped"});
    held["warnings"] = json!(["holder_stopped"]);
    let mut done = ticket(1653, "done", "Stored default policy");
    done["done_reason"] = json!("not_planned");
    let mut waiting = ticket(1662, "needs_you", "Analytics");
    waiting["needs_you"] =
        json!({"kind": "review", "text": "PR #1668 waits for your review", "url": "/d"});
    waiting["new"] = json!(true);
    let payload = json!({
        "unseen": {"count": 1, "lane_ids": [7]},
        "repos": [{"repo": "rajeshgoli/session-manager", "last_ok_at": "2026-09-29T17:01:51Z", "stale": false}],
        "lanes": [{
            "id": 7, "rank": 2,
            "goal": {"repo": "rajeshgoli/session-manager", "number": 1651, "title": "Context handoff"},
            "counts": {"needs_you": 1, "close_ready": 0, "ready": 1, "in_progress": 1, "blocked": 1, "done": 1},
            "longest_chain": [
                {"repo": "rajeshgoli/session-manager", "number": 1654},
                {"repo": "rajeshgoli/session-manager", "number": 1651},
            ],
            "cycles": [],
            "tickets": [waiting, ticket(1654, "ready", "sm handoff, successor start, work transfer"), held, blocked, done],
        }],
        "other": [{"repo": "rajeshgoli/session-manager", "tickets": [ticket(1658, "ready", "Usage ledger")]}],
    });
    let now = OffsetDateTime::parse("2026-09-29T17:02:11Z", &Rfc3339).unwrap();
    assert_eq!(
        board_lines(&payload, now),
        vec![
            "Board: 1 unseen alert",
            "Lane 2  rajeshgoli/session-manager#1651  Context handoff   (read 20s ago)",
            "  1 needs you · 0 all parts done · 1 ready · 1 in progress · 1 blocked · 1 done · longest chain 2: #1654 → #1651",
            "  needs you    #1662  Analytics                                PR #1668 waits for your review  new",
            "  ready        #1654  sm handoff, successor start, work transfer",
            "  in progress  #1660  Shared settings endpoint                 sm-1660-engineer (stopped)  ! agent stopped",
            "  blocked      #1651  Context handoff                          waits on #1654 fractal-algo-rust#1774",
            "  done         #1653  Stored default policy                    not planned",
            "Not in any lane",
            "  rajeshgoli/session-manager: ready #1658 Usage ledger",
        ]
    );
}

#[test]
fn cli_output_without_lanes() {
    let payload = json!({"unseen": {"count": 0}, "repos": [], "lanes": [], "other": []});
    assert_eq!(
        board_lines(&payload, OffsetDateTime::now_utc()),
        vec![
            "Board: no unseen alerts",
            "No lanes. Add one with sm board lane add <goal>."
        ]
    );
}

#[test]
fn cli_ticket_refs() {
    assert_eq!(
        parse_ticket_ref("fractal-algo-rust#1774", "rajeshgoli/session-manager"),
        Some(("rajeshgoli/fractal-algo-rust".to_owned(), 1774))
    );
    assert_eq!(
        parse_ticket_ref("#1654", "rajeshgoli/session-manager"),
        Some(("rajeshgoli/session-manager".to_owned(), 1654))
    );
}
