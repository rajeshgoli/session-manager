SELECT sent_at_ms AS at_ms, 1.0 AS value
FROM q.push_desk_log
WHERE desk_at_send = 1
  AND sent_at_ms >= :from_ms AND sent_at_ms < :until_ms
