use rand_core::{OsRng, RngCore};

use super::*;

fn new_store() -> GuestbookStore {
    let dir = std::env::temp_dir().join(format!(
        "sm-guestbook-{}-{}",
        std::process::id(),
        OsRng.next_u64()
    ));
    fs::create_dir_all(&dir).unwrap();
    GuestbookStore::new(dir.join("message_queue.db"))
}

fn entry(session_id: &str, repos: &[&str], text: &str) -> NewEntry {
    NewEntry {
        session_id: session_id.into(),
        session_name: format!("{session_id}-name"),
        provider: "claude".into(),
        model: Some("opus".into()),
        working_dir: "/tmp/wt".into(),
        repos: repos.iter().map(|r| (*r).to_owned()).collect(),
        claims: vec![ClaimedWork {
            repo: "acme/widgets".into(),
            number: 7,
            kind: "ticket".into(),
            title: "Fix the thing".into(),
        }],
        signed_at: "2026-09-28T12:00:00Z".into(),
        text: text.into(),
    }
}

#[test]
fn validate_entry_refuses_empty_and_oversize() {
    assert_eq!(
        validate_entry(" \n\t").unwrap_err(),
        "Guestbook entry is empty."
    );
    assert!(validate_entry(&"x".repeat(MAX_ENTRY_BYTES)).is_ok());
    let error = validate_entry(&"x".repeat(MAX_ENTRY_BYTES + 1)).unwrap_err();
    assert!(error.contains("16 KB"), "{error}");
}

#[test]
fn a_missing_db_lists_empty_and_is_not_created() {
    let store = new_store();
    let page = store
        .list(&GuestbookQuery {
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert!(page.entries.is_empty());
    assert!(!store.db_path.exists());
}

#[test]
fn sign_stores_every_field_and_lists_newest_first() {
    let store = new_store();
    let first = store
        .sign(&entry("s1", &["acme/widgets"], "first"))
        .unwrap();
    // The same agent may sign again; both entries stay.
    let second = store
        .sign(&entry("s1", &["acme/widgets"], "second"))
        .unwrap();
    assert!(second > first);
    let page = store
        .list(&GuestbookQuery {
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    let texts: Vec<_> = page.entries.iter().map(|e| e.text.as_str()).collect();
    assert_eq!(texts, ["second", "first"]);
    let stored = &page.entries[1];
    assert_eq!(stored.session_id, "s1");
    assert_eq!(stored.session_name, "s1-name");
    assert_eq!(stored.provider, "claude");
    assert_eq!(stored.model.as_deref(), Some("opus"));
    assert_eq!(stored.working_dir, "/tmp/wt");
    assert_eq!(stored.repos, ["acme/widgets"]);
    assert_eq!(stored.claims[0].number, 7);
    assert_eq!(stored.signed_at, "2026-09-28T12:00:00Z");
    assert_eq!(page.next_before, None);
}

#[test]
fn sign_refuses_an_empty_entry() {
    let store = new_store();
    assert!(store.sign(&entry("s1", &[], "  ")).is_err());
    let page = store
        .list(&GuestbookQuery {
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert!(page.entries.is_empty());
}

#[test]
fn repo_filter_matches_slug_or_name_and_pages() {
    let store = new_store();
    for n in 0..3 {
        store
            .sign(&entry(&format!("w{n}"), &["acme/widgets"], "w"))
            .unwrap();
        store
            .sign(&entry(&format!("g{n}"), &["acme/gadgets"], "g"))
            .unwrap();
    }
    for filter in ["widgets", "Acme/Widgets"] {
        let page = store
            .list(&GuestbookQuery {
                repo: Some(filter.into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.entries.len(), 3, "{filter}");
        assert!(page.entries.iter().all(|e| e.text == "w"));
    }
    let first = store
        .list(&GuestbookQuery {
            repo: Some("gadgets".into()),
            limit: 2,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(first.entries.len(), 2);
    let rest = store
        .list(&GuestbookQuery {
            repo: Some("gadgets".into()),
            before: first.next_before,
            limit: 2,
        })
        .unwrap();
    assert_eq!(rest.entries.len(), 1);
    assert_eq!(rest.next_before, None);
    assert_eq!(rest.entries[0].session_id, "g0");
}

#[test]
fn render_escapes_html_and_drops_unsafe_links_and_images() {
    let html = render_entry_html(
        "**great** job\n\n<script>alert(1)</script>\n\nsee <b>bold</b> \
         [ok](https://example.com) [bad](javascript:alert(1)) \
         ![pixel](https://tracker.example/p.png)",
    );
    assert!(html.contains("<strong>great</strong>"), "{html}");
    assert!(!html.contains("<script"), "{html}");
    assert!(html.contains("&lt;script&gt;"), "{html}");
    assert!(!html.contains("<b>"), "{html}");
    assert!(
        html.contains(r#"<a href="https://example.com">ok</a>"#),
        "{html}"
    );
    assert!(!html.contains("javascript:"), "{html}");
    assert!(html.contains("bad"), "{html}");
    assert!(!html.contains("<img"), "{html}");
    assert!(html.contains("pixel"), "{html}");
}

#[test]
fn observed_model_is_the_latest_ledger_model() {
    let store = new_store();
    let db = store.db_path.with_file_name("usage.db");
    assert_eq!(observed_model(&db, "s1"), None);
    let conn = Connection::open(&db).unwrap();
    assert_eq!(observed_model(&db, "s1"), None);
    conn.execute_batch(
        "CREATE TABLE seat_tokens (seat_id TEXT, bucket_ts TEXT, model TEXT, updated_at TEXT);
         INSERT INTO seat_tokens VALUES
           ('s1', '2026-09-28T10:00:00Z', 'claude-sonnet-5', '2026-09-28T10:00:00Z'),
           ('s1', '2026-09-28T11:00:00Z', 'claude-opus-5-5', '2026-09-28T11:00:00Z'),
           ('s2', '2026-09-28T12:00:00Z', 'gpt-x', '2026-09-28T12:00:00Z');",
    )
    .unwrap();
    assert_eq!(
        observed_model(&db, "s1").as_deref(),
        Some("claude-opus-5-5")
    );
    assert_eq!(observed_model(&db, "nobody"), None);
}
