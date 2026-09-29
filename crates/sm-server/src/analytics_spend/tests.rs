use super::*;
use std::path::PathBuf;

const CLAUDE: &str = "claude:acct";
const CODEX_A: &str = "codex:a";
const CODEX_B: &str = "codex:b";
const REPO: &str = "acme/widgets";
const PROJECT: &str = "/work/widgets/.git";
const OPUS: &str = "claude-opus-5-5";
const FABLE: &str = "claude-fable-5-1";

fn at(value: &str) -> i128 {
    nanos(value).unwrap()
}

fn text(at: i128) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(at)
        .unwrap()
        .format(&Rfc3339)
        .unwrap()
}

fn now() -> OffsetDateTime {
    OffsetDateTime::parse("2026-09-29T19:00:00Z", &Rfc3339).unwrap()
}

const DAY: i128 = 86_400 * NANOS_PER_SECOND;
const HOUR: i128 = 3_600 * NANOS_PER_SECOND;

/// Minimal usage and queue DBs with the columns Spend reads.
struct Fixture {
    usage_path: PathBuf,
    queue_path: PathBuf,
    usage: Connection,
    queue: Connection,
    live: BTreeSet<String>,
}

fn fixture() -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "sm-analytics-spend-{}-{}",
        std::process::id(),
        rand_core::RngCore::next_u64(&mut rand_core::OsRng)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let usage_path = dir.join("usage.db");
    let queue_path = dir.join("message_queue.db");
    let usage = Connection::open(&usage_path).unwrap();
    usage
        .execute_batch(
            "CREATE TABLE accounts (account_key TEXT PRIMARY KEY, provider TEXT NOT NULL,
                 label TEXT);
             CREATE TABLE account_timeline (provider TEXT NOT NULL, account_key TEXT NOT NULL,
                 from_ts TEXT NOT NULL, to_ts TEXT);
             CREATE TABLE burn_samples (id INTEGER PRIMARY KEY AUTOINCREMENT,
                 account_key TEXT NOT NULL, window_kind TEXT NOT NULL, window_scope TEXT,
                 window_start TEXT NOT NULL, percent REAL NOT NULL, resets_at TEXT NOT NULL,
                 observed_at TEXT NOT NULL);
             CREATE TABLE message_ledger (msg_id INTEGER PRIMARY KEY AUTOINCREMENT,
                 seat_id TEXT NOT NULL, account_key TEXT NOT NULL, project_key TEXT NOT NULL,
                 bucket_ts TEXT NOT NULL, model TEXT NOT NULL, effort TEXT,
                 input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
                 cache_write_5m INTEGER NOT NULL DEFAULT 0,
                 cache_write_1h INTEGER NOT NULL DEFAULT 0,
                 cache_read_tokens INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE message_window (msg_id INTEGER NOT NULL, window_kind TEXT NOT NULL,
                 window_start TEXT NOT NULL, PRIMARY KEY (msg_id, window_kind));
             CREATE TABLE seat_meta (seat_id TEXT NOT NULL, observed_at TEXT NOT NULL,
                 friendly_name TEXT, parent_seat_id TEXT);
             INSERT INTO accounts VALUES ('claude:acct', 'claude', 'me@example.com'),
                 ('codex:a', 'codex', 'a@example.com'), ('codex:b', 'codex', NULL);",
        )
        .unwrap();
    let queue = Connection::open(&queue_path).unwrap();
    queue
        .execute_batch(
            "CREATE TABLE work_claims (id TEXT PRIMARY KEY, repo TEXT NOT NULL,
                 number INTEGER NOT NULL, kind TEXT NOT NULL, session_id TEXT NOT NULL,
                 claimed_at TEXT NOT NULL, ended_at TEXT, reserved_at TEXT);
             CREATE TABLE work_links (repo TEXT NOT NULL, pr_number INTEGER NOT NULL,
                 ticket_number INTEGER NOT NULL);
             CREATE TABLE work_items (repo TEXT NOT NULL, number INTEGER NOT NULL,
                 kind TEXT NOT NULL, title TEXT NOT NULL, state TEXT NOT NULL);
             CREATE TABLE codex_review_request_registrations (id TEXT PRIMARY KEY,
                 repo TEXT NOT NULL, pr_number INTEGER NOT NULL, requester_session_id TEXT,
                 review_landed_at TEXT);",
        )
        .unwrap();
    Fixture {
        usage_path,
        queue_path,
        usage,
        queue,
        live: BTreeSet::new(),
    }
}

fn kind_of(account: &str) -> &'static str {
    if account.starts_with("codex") {
        "codex_10080"
    } else {
        "weekly_all"
    }
}

impl Fixture {
    fn sample(&self, account: &str, start: i128, percent: f64, observed: i128) {
        self.usage
            .execute(
                "INSERT INTO burn_samples (account_key, window_kind, window_start, percent,
                     resets_at, observed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    account,
                    kind_of(account),
                    text(start),
                    percent,
                    text(start + 7 * DAY),
                    text(observed)
                ],
            )
            .unwrap();
    }

    /// One ledger turn; `window` is its `message_window` start, if mapped.
    #[allow(clippy::too_many_arguments)]
    fn turn(
        &self,
        seat: &str,
        account: &str,
        minute: i128,
        model: &str,
        input: i64,
        output: i64,
        cache_read: i64,
        window: Option<i128>,
    ) {
        self.usage
            .execute(
                "INSERT INTO message_ledger (seat_id, account_key, project_key, bucket_ts, model,
                     effort, input_tokens, output_tokens, cache_read_tokens)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'high', ?6, ?7, ?8)",
                params![
                    seat,
                    account,
                    PROJECT,
                    text(minute),
                    model,
                    input,
                    output,
                    cache_read
                ],
            )
            .unwrap();
        if let Some(window) = window {
            self.usage
                .execute(
                    "INSERT INTO message_window VALUES (last_insert_rowid(), ?1, ?2)",
                    params![kind_of(account), text(window)],
                )
                .unwrap();
        }
    }

    fn seat(&self, seat: &str, name: &str, parent: Option<&str>) {
        self.usage
            .execute(
                "INSERT INTO seat_meta VALUES (?1, '2026-09-01T00:00:00Z', ?2, ?3)",
                params![seat, name, parent],
            )
            .unwrap();
    }

    fn claim(&self, id: &str, seat: &str, number: i64, kind: &str, claimed: i128) {
        self.queue
            .execute(
                "INSERT INTO work_claims (id, repo, number, kind, session_id, claimed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, REPO, number, kind, seat, text(claimed)],
            )
            .unwrap();
    }

    fn item(&self, number: i64, kind: &str, title: &str, state: &str) {
        self.queue
            .execute(
                "INSERT INTO work_items VALUES (?1, ?2, ?3, ?4, ?5)",
                params![REPO, number, kind, title, state],
            )
            .unwrap();
    }

    fn report(&self, provider: &'static str, range: SpendRange) -> SpendReport {
        let labels = BTreeMap::from([(CODEX_B.to_owned(), "b@example.com".to_owned())]);
        let repo_of = |_: &str| REPO.to_owned();
        let sources = SpendSources {
            usage_db: &self.usage_path,
            queue_db: &self.queue_path,
            account_labels: &labels,
            live_sessions: &self.live,
            repo_of: &repo_of,
        };
        spend_report(&sources, provider, range, now()).unwrap()
    }
}

fn fitted(model: &str, input: i64, output: i64, cache_read: i64) -> f64 {
    let provider = if model.starts_with("claude") {
        "claude"
    } else {
        "codex"
    };
    quota_rates::fitted_percent(
        provider,
        model,
        &Tokens {
            input,
            output,
            cache_read,
            ..Tokens::default()
        },
    )
}

fn close(left: f64, right: f64) -> bool {
    (left - right).abs() < 1e-3
}

fn child<'a>(node: &'a SpendNode, id: &str) -> &'a SpendNode {
    node.children
        .iter()
        .find(|child| child.id == id)
        .unwrap_or_else(|| panic!("no {id} under {}", node.id))
}

/// Every node's parts add to its percent, its children add to it too, and
/// children come largest first.
fn assert_consistent(node: &SpendNode) {
    if node.kind != "gap" && node.kind != "root" {
        let parts: f64 = node.parts.values().sum();
        assert!(
            close(parts, node.percent),
            "{}: {parts} vs {}",
            node.id,
            node.percent
        );
    }
    if !node.children.is_empty() {
        let children: f64 = node.children.iter().map(|child| child.percent).sum();
        assert!(
            close(children, node.percent),
            "{}: {children} vs {}",
            node.id,
            node.percent
        );
    }
    for pair in node.children.windows(2) {
        assert!(pair[0].percent >= pair[1].percent, "{} unsorted", node.id);
    }
    for child in &node.children {
        assert_consistent(child);
    }
}

#[test]
fn scale_clamps_and_the_remainder_shows_as_a_gap_or_a_note() {
    let start = at("2026-09-27T16:00:00Z");
    let observed = at("2026-09-29T18:00:00Z");
    let f = fitted(OPUS, 12_000_000, 0, 0);
    for (ratio, expected_scale, gap, excess) in [
        (1.07, 1.07, None, None),
        (30.0, 1.25, Some(30.0 * f - 1.25 * f), None),
        (0.5, 0.8, None, Some(0.3 * f)),
    ] {
        let fixture = fixture();
        fixture.sample(CLAUDE, start, ratio * f, observed);
        fixture.turn(
            "s1",
            CLAUDE,
            start + DAY,
            OPUS,
            12_000_000,
            0,
            0,
            Some(start),
        );
        let report = fixture.report("claude", SpendRange::Week);
        assert_consistent(&report.root);
        let meter = &report.meters[0];
        assert!(
            close(meter.scale, expected_scale),
            "{ratio}: {}",
            meter.scale
        );
        assert!(close(meter.fitted_percent, f), "{ratio}");
        let gap_node = report.root.children.iter().find(|node| node.kind == "gap");
        match gap {
            Some(gap) => {
                let node = gap_node.expect("gap row");
                assert_eq!(node.label, "Not in the ledger");
                assert!(close(node.percent, gap), "{ratio}");
                // The rows add up to the meter.
                assert!(close(report.total.percent, ratio * f));
            }
            None => assert!(gap_node.is_none(), "{ratio}"),
        }
        match excess {
            Some(points) => assert_eq!(
                report.notes,
                vec![format!("Estimates exceed the meter by {points:.1} points")]
            ),
            None => assert!(report.notes.is_empty(), "{ratio}: {:?}", report.notes),
        }
        let repo = child(&report.root, &format!("r:{REPO}"));
        assert!(close(repo.percent, expected_scale * f), "{ratio}");
    }
    // Nothing fitted: scale 1, the whole meter is the gap.
    assert_eq!(scale(12.0, 0.0), 1.0);
}

#[test]
fn cloud_reviews_count_against_the_account_active_when_they_land() {
    let fixture = fixture();
    let start_a = at("2026-09-26T08:00:00Z");
    let start_b = at("2026-09-27T10:00:00Z");
    let switch = text(at("2026-09-28T00:00:00Z"));
    fixture
        .usage
        .execute_batch(&format!(
            "INSERT INTO account_timeline VALUES
                 ('codex', '{CODEX_A}', '2026-09-01T00:00:00Z', '{switch}'),
                 ('codex', '{CODEX_B}', '{switch}', NULL);"
        ))
        .unwrap();
    fixture.sample(CODEX_A, start_a, 0.05, at("2026-09-28T01:00:00Z"));
    fixture.sample(CODEX_B, start_b, 0.05, at("2026-09-29T18:00:00Z"));
    // The requester holds #77, but a review follows its PR: #50 → ticket #49.
    fixture.claim("c1", "req", 77, "ticket", at("2026-09-27T00:00:00Z"));
    fixture
        .queue
        .execute_batch(&format!(
            "INSERT INTO work_links VALUES ('{REPO}', 50, 49);
             INSERT INTO codex_review_request_registrations VALUES
                 ('r1', '{REPO}', 50, 'req', '2026-09-27T12:00:00Z'),
                 ('r2', '{REPO}', 60, NULL, '2026-09-28T12:00:00Z'),
                 ('r3', '{REPO}', 60, 'req', NULL);"
        ))
        .unwrap();
    fixture.seat("req", "50-reviewer", None);
    let report = fixture.report("codex", SpendRange::Week);
    assert_consistent(&report.root);
    assert_eq!(report.meters.len(), 2);
    for meter in &report.meters {
        assert!(
            close(meter.fitted_percent, CLOUD_REVIEW_PERCENT),
            "{meter:?}"
        );
        assert!(close(meter.scale, 1.0));
    }
    assert!(close(report.total.percent, 0.1));
    assert_eq!(report.meters[0].label.as_deref(), Some("a@example.com"));
    assert_eq!(report.meters[1].label.as_deref(), Some("b@example.com"));
    let repo = child(&report.root, &format!("r:{REPO}"));
    let requested = child(child(repo, &format!("t:{REPO}#49")), "a:req");
    assert_eq!(requested.label, "50-reviewer");
    assert_eq!(requested.parts.keys().collect::<Vec<_>>(), vec!["review"]);
    let models = requested.models.as_ref().unwrap();
    assert_eq!(models[0].model, "codex-cloud-review");
    assert_eq!(models[0].turns, 1);
    assert_eq!(requested.tokens, 0);
    let owner = child(child(repo, &format!("t:{REPO}#60")), "a:owner");
    assert_eq!(owner.label, "You");
    assert_eq!(owner.session_id, None);
    assert_eq!(report.parts_legend[0].key, "review");
    assert_eq!(report.parts_legend[0].label, "Cloud reviews");
    // Claude has no cloud reviews.
    assert!(fixture
        .report("claude", SpendRange::Week)
        .root
        .children
        .is_empty());
}

#[test]
fn ranges_pick_windows_and_scale_each_turn_by_its_own_window() {
    let fixture = fixture();
    let w3 = at("2026-09-27T16:00:00Z");
    let w2 = w3 - 7 * DAY;
    let w1 = w2 - 7 * DAY;
    let f1 = fitted(OPUS, 6_000_000, 0, 0);
    let f2 = fitted(OPUS, 3_000_000, 0, 0);
    let f3 = fitted(OPUS, 1_000_000, 0, 0);
    fixture.sample(CLAUDE, w1, 1.1 * f1, w1 + 6 * DAY);
    fixture.sample(CLAUDE, w2, 1.2 * f2, w2 + 6 * DAY);
    fixture.sample(CLAUDE, w3, 0.9 * f3, at("2026-09-29T18:00:00Z"));
    fixture.turn("s1", CLAUDE, w1 + 5 * DAY, OPUS, 6_000_000, 0, 0, Some(w1));
    // An unmapped turn falls into the window containing its minute.
    fixture.turn("s1", CLAUDE, w2 + DAY, OPUS, 2_000_000, 0, 0, None);
    fixture.turn("s1", CLAUDE, w2 + 2 * DAY, OPUS, 1_000_000, 0, 0, Some(w2));
    fixture.turn("s1", CLAUDE, w3 + HOUR, OPUS, 1_000_000, 0, 0, Some(w3));

    let week = fixture.report("claude", SpendRange::Week);
    assert!(close(week.total.percent, 0.9 * f3));
    assert_eq!(week.start, "2026-09-27T16:00:00Z");
    assert_eq!(week.end, "2026-10-04T16:00:00Z");
    assert_eq!(week.total.tokens, 1_000_000);

    let last = fixture.report("claude", SpendRange::LastWeek);
    assert!(close(last.total.percent, 1.2 * f2));
    assert_eq!(last.start, "2026-09-20T16:00:00Z");
    assert_eq!(last.meters.len(), 1);
    assert!(last.meters[0].pace.is_none());
    assert_eq!(last.total.tokens, 3_000_000);

    // Four weeks: every turn since Sep 1, each at its own window's scale.
    let four = fixture.report("claude", SpendRange::FourWeeks);
    assert_consistent(&four.root);
    assert!(close(four.total.percent, 1.1 * f1 + 1.2 * f2 + 0.9 * f3));
    assert_eq!(four.meters.len(), 3);
    assert_eq!(four.start, "2026-09-01T19:00:00Z");
    assert_eq!(four.total.tokens, 10_000_000);
}

#[test]
fn codex_window_starts_a_second_apart_are_one_window() {
    let fixture = fixture();
    let start = at("2026-09-26T18:11:20Z");
    let jittered = start + NANOS_PER_SECOND;
    fixture.sample(CODEX_A, jittered, 4.0, at("2026-09-28T07:00:00Z"));
    fixture.sample(CODEX_A, start, 22.0, at("2026-09-29T18:00:00Z"));
    fixture.turn(
        "s1",
        CODEX_A,
        start + DAY,
        "gpt-6-astra",
        1_000_000,
        0,
        0,
        Some(jittered),
    );
    let report = fixture.report("codex", SpendRange::Week);
    assert_eq!(report.meters.len(), 1);
    assert_eq!(report.meters[0].percent, 22.0);
    assert!(close(report.meters[0].fitted_percent, 0.683));
}

#[test]
fn tree_sorts_children_rolls_parts_up_and_names_every_level() {
    let mut fixture = fixture();
    let start = at("2026-09-27T16:00:00Z");
    let minute = start + DAY;
    fixture.item(12, "ticket", "Pivot detector", "open");
    fixture.claim("c1", "author", 12, "ticket", start);
    fixture.seat("author", "12-spec-author", None);
    fixture.seat("helper", "scout", Some("author"));
    fixture.seat("adhoc", "comfy-ui", None);
    fixture.live.insert("author".to_owned());
    fixture.turn(
        "author",
        CLAUDE,
        minute,
        FABLE,
        0,
        100_000,
        10_000_000,
        Some(start),
    );
    fixture.turn("author", CLAUDE, minute, OPUS, 0, 50_000, 0, Some(start));
    fixture.turn("helper", CLAUDE, minute, OPUS, 0, 20_000, 0, Some(start));
    fixture.turn("adhoc", CLAUDE, minute, OPUS, 0, 300_000, 0, Some(start));
    fixture.turn("unassigned", CLAUDE, minute, OPUS, 0, 1_000, 0, Some(start));
    let f = fitted(FABLE, 0, 100_000, 10_000_000) + fitted(OPUS, 0, 371_000, 0);
    fixture.sample(CLAUDE, start, f, at("2026-09-29T16:00:00Z"));

    let report = fixture.report("claude", SpendRange::Week);
    assert_consistent(&report.root);
    assert_eq!(report.root.kind, "root");
    assert!(close(report.total.percent, f));
    let repo = child(&report.root, &format!("r:{REPO}"));
    assert_eq!(repo.kind, "repo");
    assert_eq!(repo.label, "widgets");
    assert_eq!(
        repo.children
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        vec![format!("t:{REPO}#12"), format!("t:{REPO}#none")]
    );
    let thread = child(repo, &format!("t:{REPO}#12"));
    assert_eq!(thread.label, "#12 Pivot detector");
    assert_eq!(thread.state.as_deref(), Some("open"));
    assert_eq!(thread.history_path.as_deref(), Some("/t/widgets/12"));
    let author = child(thread, "a:author");
    assert_eq!(author.kind, "agent");
    assert_eq!(author.session_id.as_deref(), Some("author"));
    assert_eq!(author.session_status, Some("running"));
    assert!(author.children.is_empty());
    let models = author.models.as_ref().unwrap();
    assert_eq!(models[0].model, FABLE);
    assert_eq!(models[0].effort.as_deref(), Some("high"));
    assert_eq!(models[0].tokens.cache_read, 10_000_000);
    assert_eq!(author.parts.len(), 2);
    assert_eq!(child(thread, "a:helper").session_status, Some("stopped"));
    let none = child(repo, &format!("t:{REPO}#none"));
    assert_eq!(none.label, "No ticket");
    assert_eq!(none.history_path, None);
    assert_eq!(child(none, "a:adhoc").label, "comfy-ui");
    let unassigned = child(none, "a:unassigned");
    assert_eq!(unassigned.label, "Unassigned");
    assert_eq!(unassigned.session_id, None);
    // Author by claim, helper by parent, the rest by nothing.
    assert!(close(report.basis["parent"], fitted(OPUS, 0, 20_000, 0)));
    assert!(close(report.basis["none"], fitted(OPUS, 0, 301_000, 0)));
    let basis: f64 = report.basis.values().sum();
    assert!(close(basis, report.total.percent));
    assert_eq!(
        report
            .parts_legend
            .iter()
            .map(|entry| entry.key.as_str())
            .collect::<Vec<_>>(),
        vec!["fable", "opus"]
    );
    assert!(report.meters[0].pace.is_some());
}

#[test]
fn pace_runs_out_projects_or_stays_silent_in_the_first_hour() {
    let start = at("2026-09-27T16:00:00Z");
    let window = |percent: f64, observed: i128| Window {
        account_key: CLAUDE.to_owned(),
        starts: BTreeSet::new(),
        start,
        resets_at: start + 7 * DAY,
        start_text: String::new(),
        resets_text: String::new(),
        percent,
        observed_at: observed,
        observed_text: String::new(),
    };
    // 30% in 2 days reaches 100% 4⅔ days later, before the reset.
    assert_eq!(
        pace(&window(30.0, start + 2 * DAY)),
        Some(Pace::RunsOut {
            at: text(start + 2 * DAY + 14 * DAY / 3)
        })
    );
    // 10% in 2 days: 35% at the reset.
    assert_eq!(
        pace(&window(10.0, start + 2 * DAY)),
        Some(Pace::OnPace { percent: 35.0 })
    );
    assert_eq!(pace(&window(3.0, start + HOUR / 2)), None);
}

#[test]
fn default_provider_is_the_meter_closer_to_its_limit() {
    let fixture = fixture();
    let observed = at("2026-09-29T18:00:00Z");
    assert_eq!(
        default_provider(&fixture.usage_path, now()).unwrap(),
        "claude"
    );
    fixture.sample(CLAUDE, at("2026-09-27T16:00:00Z"), 20.0, observed);
    fixture.sample(CODEX_A, at("2026-09-26T18:00:00Z"), 25.0, observed);
    assert_eq!(
        default_provider(&fixture.usage_path, now()).unwrap(),
        "codex"
    );
    // A closed window does not count.
    fixture.sample(
        CLAUDE,
        at("2026-09-13T16:00:00Z"),
        90.0,
        observed - 10 * DAY,
    );
    assert_eq!(
        default_provider(&fixture.usage_path, now()).unwrap(),
        "codex"
    );
}

#[test]
fn default_provider_sums_the_codex_accounts() {
    let fixture = fixture();
    let observed = at("2026-09-29T18:00:00Z");
    fixture.sample(CLAUDE, at("2026-09-27T16:00:00Z"), 22.0, observed);
    fixture.sample(CODEX_A, at("2026-09-26T18:00:00Z"), 15.0, observed);
    fixture.sample(CODEX_B, at("2026-09-27T10:00:00Z"), 10.0, observed);
    assert_eq!(
        default_provider(&fixture.usage_path, now()).unwrap(),
        "codex"
    );
}

#[test]
fn missing_databases_read_as_an_empty_report() {
    let missing = std::env::temp_dir().join("sm-analytics-spend-missing/usage.db");
    let labels = BTreeMap::new();
    let live = BTreeSet::new();
    let sources = SpendSources {
        usage_db: &missing,
        queue_db: &missing,
        account_labels: &labels,
        live_sessions: &live,
        repo_of: &|key: &str| key.to_owned(),
    };
    let report = spend_report(&sources, "claude", SpendRange::Week, now()).unwrap();
    assert_eq!(report.total.percent, 0.0);
    assert!(report.root.children.is_empty());
    assert_eq!(default_provider(&missing, now()).unwrap(), "claude");
}

#[test]
fn unknown_models_add_a_note() {
    let fixture = fixture();
    let start = at("2026-09-26T18:00:00Z");
    fixture.sample(CODEX_A, start, 1.0, at("2026-09-29T18:00:00Z"));
    fixture.turn(
        "s1",
        CODEX_A,
        start + DAY,
        "gpt-7-x",
        1_000_000,
        0,
        0,
        Some(start),
    );
    fixture.turn(
        "s1",
        CODEX_A,
        start + DAY,
        "unknown",
        1_000_000,
        0,
        0,
        Some(start),
    );
    let report = fixture.report("codex", SpendRange::Week);
    assert_eq!(
        report.notes,
        vec!["gpt-7-x has no fitted rate; priced as Sol"]
    );
    assert_eq!(report.parts_legend[0].key, "other");
}
