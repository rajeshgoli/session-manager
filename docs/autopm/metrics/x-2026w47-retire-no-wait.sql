SELECT at_ms, CAST(duration_ms AS REAL) AS value
FROM q.owner_events
WHERE action = 'session.retire'
  AND surface IN ('web', 'android')
  AND status = 200
  AND at_ms >= :from_ms
  AND at_ms < :until_ms
