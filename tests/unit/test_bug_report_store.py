from src.bug_report_store import BugReportStore


def test_bug_report_store_prunes_oldest_rows(tmp_path):
    store = BugReportStore(str(tmp_path / "bug_reports.db"), max_reports=3)

    created_ids = []
    for index in range(5):
        created = store.create_report(
            report_text=f"report {index}",
            reported_by="user@example.com",
            route="/watch/",
            include_debug_state=False,
        )
        created_ids.append(created["id"])

    assert store.count_reports() == 3
    assert store.list_report_ids() == created_ids[-3:]


def test_bug_report_store_updates_maintainer_delivery_result(tmp_path):
    store = BugReportStore(str(tmp_path / "bug_reports.db"), max_reports=30)
    created = store.create_report(
        report_text="attach goes blank",
        reported_by="user@example.com",
        selected_session_id="sess1234",
        route="/watch/",
        include_debug_state=True,
        client_state={"route": "/watch/"},
        server_state={"health": {"status": "healthy"}},
    )

    store.update_delivery_result(created["id"], "delivered")
    report = store.get_report(created["id"])

    assert report is not None
    assert report["maintainer_delivery_result"] == "delivered"
    assert report["status"] == "submitted"
