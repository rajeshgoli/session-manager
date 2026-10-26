SELECT CAST((julianday(created_at) - 2440587.5) * 86400000 AS INTEGER) AS at_ms, 1.0 AS value
FROM bugs.bug_reports
WHERE (lower(report_text) LIKE '%push%' OR lower(report_text) LIKE '%notif%')
  AND created_at >= strftime('%Y-%m-%dT%H:%M:%S', :from_ms / 1000, 'unixepoch')
  AND created_at <  strftime('%Y-%m-%dT%H:%M:%S', :until_ms / 1000, 'unixepoch')
