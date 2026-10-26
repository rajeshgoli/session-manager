WITH asks AS (
  SELECT CAST((julianday(created_at) - 2440587.5) * 86400000 AS INTEGER) AS at_ms,
         CAST((julianday(first_viewed_at) - 2440587.5) * 86400000 AS INTEGER) AS open_ms
  FROM q.owner_messages
  WHERE blocking = 0 AND lower(human) = 'rajesh'
  UNION ALL
  SELECT CAST((julianday(p.published_at) - 2440587.5) * 86400000 AS INTEGER),
         (SELECT MIN(e.at_ms) FROM q.owner_events e
            WHERE e.action IN ('doc.open', 'doc.review')
              AND (e.target_id = p.doc_id OR e.target_id = substr(d.repo, instr(d.repo, '/') + 1) || '/' || d.path)
              AND e.at_ms >= CAST((julianday(p.published_at) - 2440587.5) * 86400000 AS INTEGER))
  FROM q.owner_doc_publishes p JOIN q.owner_docs d ON d.id = p.doc_id
  WHERE p.review_requested = 1
)
SELECT at_ms, MIN((COALESCE(open_ms, :until_ms) - at_ms) / 60000.0, 1440.0) AS value
FROM asks a
WHERE at_ms >= :from_ms AND at_ms < :until_ms
  AND EXISTS (SELECT 1 FROM q.owner_events e WHERE e.surface = 'web' AND e.at_ms >= a.at_ms - 300000
              AND e.at_ms < a.at_ms AND e.action NOT IN (
              'watch.state','restart.latest','session.models','model.status','board.badge','board.seen',
              'doc.head','queue.stats','utilization.series','host.status','terminal.ticket','app.meta',
              'push.register','follow.list','artifact.activity','artifact.list','session.obligations',
              'review.status','handoff.defaults','doc.drafts','claim.list','notice.ack','app.bootstrap',
              'review.policies'))
  AND NOT EXISTS (SELECT 1 FROM q.owner_events e WHERE e.surface = 'android'
                  AND e.at_ms >= a.at_ms - 300000 AND e.at_ms < a.at_ms)
