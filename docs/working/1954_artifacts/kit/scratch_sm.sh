#!/bin/zsh
# #1954 scratch sm (memo H.8): the installed sm-server on fresh, empty state, port 18440, runtime off,
# so an agent's allowed sm commands (send to rajesh, request-review, status) reach nothing live.
# usage: sm queue run --type service --timeout 12h --label 1954-scratch-sm -- scratch_sm.sh
S=/private/tmp/l1954-sm
rm -rf $S; mkdir -p $S/state $S/iso $S/q
cat > $S/config.yaml <<Y
server: {host: 127.0.0.1, port: 18440}
paths:
  log_dir: $S/logs
  app_artifacts_dir: $S/state/apps
  bug_reports_db: $S/state/bug_reports.db
  state_file: $S/state/sessions.json
  notes_db: $S/state/notes.db
tmux: {socket_name: l1954-scratch}
usage: {enabled: false, db_path: $S/state/usage.db}
activity: {db_path: $S/state/activity.db}
queue_runner: {state_dir: $S/q}
sm_send: {db_path: $S/state/message_queue.db}
codex_events: {db_path: $S/state/codex_events.db}
codex_requests: {db_path: $S/state/codex_requests.db}
codex_observability: {db_path: $S/state/codex_observability.db}
rust_core: {runtime_enabled: false, fixture_writes_enabled: true}
Y
python3 ${0:A:h}/seed_scratch.py ${0:A:h:h}/runs-1954/agents.json $S/state/sessions.json
export SM_TEST_ISOLATION_ROOT=$S/iso
# It keeps the owner's GitHub login: request-review posts a real @codex review on the agent's draft PR
# (owner's call, 3 Oct). Runtime is off, so the driver delivers the review to the agent by hand.
exec /Users/rajesh/projects/session-manager-prod/.local/bin/sm-server --config $S/config.yaml --host 127.0.0.1 --port 18440
