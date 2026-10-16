CREATE TABLE IF NOT EXISTS artifacts (
  root TEXT NOT NULL, name TEXT NOT NULL COLLATE NOCASE,
  state TEXT NOT NULL,            -- unrecorded|building|live|frozen|deleted
  op TEXT,                        -- NULL|freezing|thawing|moving|deleting
  location TEXT NOT NULL,         -- root|cold (meaningful for live/building)
  set_id TEXT NOT NULL,
  origin TEXT NOT NULL,           -- queue|hand|adopted|unrecorded
  owner_session_id TEXT, owner_session_name TEXT,
  repo TEXT NOT NULL, ticket INTEGER, epic TEXT,
  note TEXT,                      -- plain English, 1..note_max_chars once recorded
  description TEXT,               -- optional markdown, <= 8000 chars
  project INTEGER NOT NULL DEFAULT 0,
  keep_until TEXT,                -- YYYY-MM-DD or NULL
  keep_live_until TEXT,           -- owner "Keep live"; blocks freeze/delete by sm
  recipe_complete INTEGER NOT NULL DEFAULT 0,
  gpu_hours REAL, gpu_cost_usd REAL,
  disk_bytes INTEGER,             -- live: st_blocks*512 on its filesystem
  data_bytes INTEGER,             -- live: sum of st_size
  frozen_bytes INTEGER,           -- frozen: archive parts on the cold store
  ratio_estimate REAL, ratio_sampled_at TEXT, -- J.2
  content_stamp TEXT,             -- H.1 stamp of the live tree
  size_measured_at TEXT,
  last_used_at TEXT,              -- latest queue use (E.5) or thaw
  created_at TEXT NOT NULL, first_seen_at TEXT, grace_until TEXT,
  recorded_at TEXT, frozen_at TEXT, thawed_at TEXT,
  deleted_at TEXT, deleted_by TEXT, delete_reason TEXT, rebuildable INTEGER,
  backed_up_at TEXT, backup_stamp TEXT,
  PRIMARY KEY (root, name));
CREATE TABLE IF NOT EXISTS artifact_consumers (root TEXT, name TEXT COLLATE NOCASE, repo TEXT, number INTEGER,
  added_by TEXT, added_at TEXT, last_state TEXT, checked_at TEXT,   -- OPEN|CLOSED|UNKNOWN
  PRIMARY KEY (root, name, repo, number));
CREATE TABLE IF NOT EXISTS artifact_signals (root TEXT, name TEXT COLLATE NOCASE,
  kind TEXT,                      -- named|input_of|in_use
  evidence TEXT, evaluated_at TEXT);  -- replaced wholesale per artifact by F
CREATE TABLE IF NOT EXISTS artifact_jobs (root TEXT, name TEXT COLLATE NOCASE, job_id TEXT, seq INTEGER,
  type TEXT, label TEXT, argv_json TEXT, script_text TEXT, env_json TEXT, timeout_seconds INTEGER,
  cwd TEXT, cwd_rel TEXT, commit_sha TEXT, dirty INTEGER, tag TEXT, state TEXT,
  exit_code INTEGER, wall_seconds INTEGER, bytes_written INTEGER,
  PRIMARY KEY (root, name, job_id));
CREATE TABLE IF NOT EXISTS artifact_inputs (root TEXT, name TEXT COLLATE NOCASE, input TEXT COLLATE NOCASE,
  source TEXT, job_id TEXT, PRIMARY KEY (root, name, input, job_id));  -- declared|detected
CREATE TABLE IF NOT EXISTS artifact_aliases (root TEXT, alias TEXT COLLATE NOCASE, target_name TEXT COLLATE NOCASE,
  target_rel TEXT, PRIMARY KEY (root, alias));
CREATE TABLE IF NOT EXISTS artifact_job_reservations (job_id TEXT PRIMARY KEY, root TEXT,
  outputs_json TEXT, inputs_json TEXT, disk_bytes INTEGER NOT NULL,
  placed TEXT,                    -- root|cold, decided at admission (E.3)
  pending_record_json TEXT,       -- record flags given on sm queue run (E.1), applied at E.5
  start_bytes INTEGER, first_disk_hold_at TEXT, overrun_warned INTEGER DEFAULT 0);
CREATE TABLE IF NOT EXISTS artifact_events (id INTEGER PRIMARY KEY AUTOINCREMENT, root TEXT, name TEXT, at TEXT,
  kind TEXT,  -- created|recorded|request_sent|frozen|thawed|moved|deleted|would_*|rebuild_*|
              -- consumer_*|owner_*|overrun|tag_*|op_failed|backup_included
  actor TEXT, detail_json TEXT);
CREATE TABLE IF NOT EXISTS backup_runs (id INTEGER PRIMARY KEY AUTOINCREMENT, queue_job_id TEXT,
  started_at TEXT, finished_at TEXT, state TEXT, include_count INTEGER, include_bytes INTEGER,
  log_path TEXT);
CREATE TABLE IF NOT EXISTS backup_static (path TEXT PRIMARY KEY, content_stamp TEXT, backup_stamp TEXT, backed_up_at TEXT);

CREATE TABLE IF NOT EXISTS artifact_disk_holds (
 job_id TEXT PRIMARY KEY, root TEXT NOT NULL, role TEXT NOT NULL, label TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS artifact_completions (
 job_id TEXT PRIMARY KEY, root TEXT NOT NULL, state TEXT NOT NULL,
 finished_at TEXT NOT NULL, bytes_written INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS artifact_completion_window ON artifact_completions(root,finished_at);

CREATE TABLE IF NOT EXISTS artifact_meta (key TEXT PRIMARY KEY, value TEXT);

CREATE TABLE IF NOT EXISTS artifact_rebuilds (
 id TEXT PRIMARY KEY, root TEXT NOT NULL, set_id TEXT NOT NULL,
 plan_json TEXT NOT NULL, execution_json TEXT NOT NULL,
 state TEXT NOT NULL, started_at TEXT NOT NULL, finished_at TEXT, error TEXT, notified INTEGER NOT NULL DEFAULT 0);
CREATE UNIQUE INDEX IF NOT EXISTS artifact_rebuild_active
 ON artifact_rebuilds(root,set_id) WHERE state IN ('preparing','running');
CREATE TABLE IF NOT EXISTS artifact_rebuild_steps (
 run_id TEXT NOT NULL, position INTEGER NOT NULL, job_id TEXT UNIQUE, error TEXT,
 PRIMARY KEY(run_id,position));

-- The journal is retained after completion for status and recovery diagnostics.
CREATE TABLE IF NOT EXISTS artifact_operations (
 id INTEGER PRIMARY KEY AUTOINCREMENT, root TEXT NOT NULL, set_id TEXT NOT NULL,
 kind TEXT NOT NULL, phase TEXT NOT NULL, members_json TEXT NOT NULL,
 actor TEXT NOT NULL, created_at TEXT NOT NULL, finished_at TEXT, error TEXT);
CREATE UNIQUE INDEX IF NOT EXISTS artifact_operation_active
 ON artifact_operations(root,set_id) WHERE phase NOT IN ('done','failed');
