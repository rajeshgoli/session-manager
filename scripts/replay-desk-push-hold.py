"""Read only shadow replay of eligible notices from SQLite snapshots."""

import datetime as dt
import json
import sqlite3
import sys

if len(sys.argv) != 2:
    raise SystemExit("usage: replay-desk-push-hold.py <snapshot-directory>")
ROOT = sys.argv[1]
UTC = dt.timezone.utc
END = dt.datetime.now(UTC)
START = END - dt.timedelta(days=14)
BACKGROUND = {
    "watch.state", "restart.latest", "session.models", "model.status",
    "board.badge", "board.seen", "doc.head", "queue.stats",
    "utilization.series", "host.status", "terminal.ticket", "app.meta",
    "push.register", "follow.list", "artifact.activity", "artifact.list",
    "session.obligations", "review.status", "handoff.defaults", "doc.drafts",
    "claim.list", "notice.ack", "app.bootstrap", "review.policies",
}


def ms(value):
    if value is None:
        return None
    if isinstance(value, (int, float)):
        return int(value)
    return int(dt.datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp() * 1000)


db = sqlite3.connect(f"{ROOT}/owner_push.db")
db.row_factory = sqlite3.Row
db.execute(f"ATTACH DATABASE '{ROOT}/message_queue.db' AS q")


def at_desk(at):
    rows = db.execute(
        "SELECT surface, action FROM q.owner_events WHERE at_ms >= ? AND at_ms < ? "
        "AND surface IN ('web', 'android')", (at - 300_000, at)
    ).fetchall()
    return any(r["surface"] == "web" and r["action"] not in BACKGROUND for r in rows) and not any(
        r["surface"] == "android" for r in rows
    )


def first_open(row):
    if row["kind"] == "message":
        found = db.execute(
            "SELECT first_viewed_at FROM q.owner_messages WHERE id = ?", (row["subject_id"],)
        ).fetchone()
        return ms(found[0]) if found else None
    found = db.execute(
        "SELECT p.published_at, d.id, d.repo, d.path FROM q.owner_doc_publishes p "
        "JOIN q.owner_docs d ON d.id=p.doc_id WHERE p.id=?", (row["subject_id"],)
    ).fetchone()
    if not found:
        return None
    readable = found["repo"].split("/")[-1] + "/" + found["path"]
    result = db.execute(
        "SELECT MIN(at_ms) FROM q.owner_events WHERE action IN ('doc.open','doc.review') "
        "AND target_id IN (?, ?) AND at_ms >= ?",
        (found["id"], readable, ms(found["published_at"])),
    ).fetchone()
    return result[0]


rows = db.execute(
    "SELECT id, kind, subject_id, created_at, notify_after, notified_at, notified_via "
    "FROM owner_notices WHERE (kind='review_requested' OR (kind='message' AND blocking=0)) "
    "AND created_at >= ? AND created_at < ? ORDER BY created_at, id",
    (START.isoformat().replace("+00:00", "Z"), END.isoformat().replace("+00:00", "Z")),
).fetchall()

off_pushes = []
historical_pushes = []
desk = held = resolved = late_pushes = other = 0
opened_before_historical_push = 0
history = []
for row in rows:
    created = ms(row["created_at"])
    opened = first_open(row)
    if row["notified_via"] == "push":
        historical_pushes.append(row["id"])
        if opened is not None and opened <= ms(row["notified_at"]):
            opened_before_historical_push += 1
    # The off recorder uses the existing delivery destination and due time.
    # It makes no external call; its recorded IDs must match the snapshot.
    if row["notified_via"] == "push" and ms(row["notify_after"]) == created:
        off_pushes.append(row["id"])
    active = at_desk(created)
    history.append({
        "kind": row["kind"], "subject_id": row["subject_id"],
        "created_ms": created, "notified_ms": ms(row["notified_at"]),
        "opened_ms": opened, "desk": active, "outcome": row["notified_via"],
    })
    desk += active
    if not active:
        continue
    held += 1
    due = created + 600_000
    if (opened is not None and opened <= due) or (
        row["notified_via"] == "resolved" and ms(row["notified_at"]) <= due
    ):
        resolved += 1
    elif row["notified_via"] == "push":
        late_pushes += 1
    else:
        other += 1

report = {
    "window_start_utc": START.isoformat(),
    "window_end_utc": END.isoformat(),
    "eligible": len(rows),
    "at_desk": desk,
    "held": held,
    "held_then_resolved": resolved,
    "held_then_pushed": late_pushes,
    "held_other_historical_channel": other,
    "switch_off_recorded_pushes": len(off_pushes),
    "historical_pushes": len(historical_pushes),
    "switch_off_push_ids_equal_history": off_pushes == historical_pushes,
    "opened_before_historical_push": opened_before_historical_push,
}
with open(f"{ROOT}/history.json", "w", encoding="utf-8") as fixture:
    json.dump(history, fixture)
print(json.dumps(report, indent=2))
