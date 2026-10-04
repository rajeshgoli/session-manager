#!/bin/zsh
# #1966 scratch sm: the installed sm-server on fresh, empty state at port 18450, runtime ON so it
# delivers messages, fires reminders and runs its own queue jobs. Nothing is shared with the live
# server: own state, queue dir, tmux socket (l1966-scratch), usage db; SM_TEST_ISOLATION_ROOT
# isolates every default path and drops the FCM push key; cwd has no email bridge config.
# Seeds two sessions: the parent sm-1966 (p1966000) and the opencode agent sm-1966-oc (a1966oc0),
# provider "claude" so sm's existing hook routes and tmux delivery apply (oc_pane.py is its pane).
# usage: scratch_sm.sh <checkout>      (run through sm queue as a service job)
set -eu
S=/private/tmp/l1966-sm WT=${1:A}
mkdir -p $S/state $S/iso $S/q $S/logs
cat > $S/config.yaml <<Y
server: {host: 127.0.0.1, port: 18450}
paths:
  log_dir: $S/logs
  app_artifacts_dir: $S/state/apps
  bug_reports_db: $S/state/bug_reports.db
  state_file: $S/state/sessions.json
  notes_db: $S/state/notes.db
tmux: {socket_name: l1966-scratch}
usage: {enabled: true, db_path: $S/state/usage.db, scan_interval_secs: 10}
activity: {db_path: $S/state/activity.db}
queue_runner: {state_dir: $S/q}
sm_send: {db_path: $S/state/message_queue.db}
codex_events: {db_path: $S/state/codex_events.db}
codex_requests: {db_path: $S/state/codex_requests.db}
codex_observability: {db_path: $S/state/codex_observability.db}
rust_core: {runtime_enabled: true, fixture_writes_enabled: true}
Y
[[ -f $S/state/sessions.json ]] || python3 - $S $WT <<'EOF'
import datetime, hashlib, json, sys
s, wt = sys.argv[1:]
now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")
def rec(sid, name, wd, parent, cred):
    return {"id": sid, "name": name, "friendly_name": name, "friendly_name_is_explicit": True,
            "provider": "claude", "status": "idle", "working_dir": wd, "parent_session_id": parent,
            "created_at": now, "spawned_at": now, "last_activity": now, "started_by_sm": True,
            "model": "local-flash-next", "node": "primary", "is_em": False, "turns_completed": 0,
            "tmux_session": f"l1966-{sid}", "tmux_socket_name": "l1966-scratch",
            "log_file": f"{s}/logs/{sid}.log", "completion_status": None,
            "session_credential_sha256": hashlib.sha256(cred.encode()).hexdigest()}
creds = {"p1966000": "parent-" + "0" * 26, "a1966oc0": "agent-" + "1" * 26}
json.dump({"sessions": [rec("p1966000", "sm-1966", "/Users/rajesh/projects/session-manager", None, creds["p1966000"]),
                        rec("a1966oc0", "sm-1966-oc", wt, "p1966000", creds["a1966oc0"])]},
          open(f"{s}/state/sessions.json", "w"), indent=1)
json.dump(creds, open(f"{s}/creds.json", "w"))
print("seeded p1966000, a1966oc0")
EOF
cd $S
export SM_TEST_ISOLATION_ROOT=$S/iso
exec /Users/rajesh/projects/session-manager-prod/.local/bin/sm-server --config $S/config.yaml --host 127.0.0.1 --port 18450
