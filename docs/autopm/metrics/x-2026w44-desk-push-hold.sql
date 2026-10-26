-- Pushes that reached Rajesh's phone while he was at the desk, one row per push.
-- push_desk_log is written by the experiment's build (plan: docs/autopm/plans/x-2026w44-desk-push-hold.html).
SELECT sent_at_ms AS at_ms, 1.0 AS value
FROM q.push_desk_log
WHERE desk_at_send = 1
  AND sent_at_ms >= :from_ms AND sent_at_ms < :until_ms
