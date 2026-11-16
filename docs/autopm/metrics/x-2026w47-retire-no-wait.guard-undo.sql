SELECT r.at_ms, 1.0 AS value
FROM q.owner_events r
WHERE r.action = 'session.retire'
  AND r.surface IN ('web', 'android')
  AND r.at_ms >= :from_ms
  AND r.at_ms < :until_ms
  AND EXISTS (
    SELECT 1 FROM q.owner_events s
    WHERE s.action = 'session.restore'
      AND s.target_id = r.target_id
      AND s.at_ms > r.at_ms
      AND s.at_ms < r.at_ms + 120000
  )
