#!/bin/zsh
# #1966: launch the opencode agent a1966oc0 inside #1954's wall, wired to the scratch sm.
# usage: [RUN=a TICKET=1966 TITLE=... COMMIT=origin/main BRANCH=1966-oc-probe] run_oc.sh <model base url> [start|restart]
#   start    fresh opencode session; writes its id to the bridge config
#   restart  same XDG data dir and session id: restore after an opencode process restart
# Layout (all outside live state):
#   checkout ~/worktrees/sm-1966-oc-<run>        temp /private/tmp/l1966-<run> (tmux, TMPDIR)
#   run dir  $RUNS/<run>: opencode XDG dirs, opencode.json, profile.sb, bridge.json, logs
# Run a is the lifecycle probe (checkout sm-1966-oc-probe); later runs are benchmark tickets.
# Needs, outside the wall: scratch sm :18450, judge service :8441, model at <model base url>.
# tmux: -L l1966 holds opencode serve (oc) and the bridge (bridge); -L l1966-scratch holds the pane sm types into.
set -eu
MODEL_URL=$1 MODE=${2:-start}
K=${0:A:h} RUNS=${L1966_RUNS:-$HOME/.local/share/local-agents-proto/runs-1966}
RUN=${RUN:-a} TICKET=${TICKET:-1966} TITLE=${TITLE:-opencode lifecycle probe}
COMMIT=${COMMIT:-origin/main} BRANCH=${BRANCH:-1966-oc-probe}
R=$RUNS/$RUN T=/private/tmp/l1966-$RUN WT=$HOME/worktrees/sm-1966-oc-$RUN
[[ $RUN == a ]] && WT=$HOME/worktrees/sm-1966-oc-probe
REPO=/Users/rajesh/projects/session-manager
OCPORT=18451 SMPORT=18450 JPORT=8441
MPORT=${MODEL_URL##*:}; MPORT=${MPORT%%/*}
X=$R/xdg
mkdir -p $X/config/opencode/plugins $X/data $X/cache $X/state $T/tmp $R

[[ -d $WT ]] || { git clone -q --shared $REPO $WT && git -C $WT checkout -q -b $BRANCH $COMMIT; }
git -C $WT remote set-url origin https://github.com/rajeshgoli/session-manager.git
git -C $WT config user.name "sm-1966-oc (local agent)"; git -C $WT config user.email "sm-1966-oc@local.invalid"

# Judge service agent record (#1954 K.4 fields).
python3 - $RUNS/agents.json $WT $T $TICKET "$TITLE" $BRANCH <<'EOF'
import json, os, sys
path, wt, t, ticket, title, branch = sys.argv[1:]
d = json.load(open(path)) if os.path.exists(path) else {}
d["a1966oc0"] = {"name": "sm-1966-oc", "ticket": int(ticket), "title": title,
                 "branch": branch, "checkout": wt, "tmp": t, "parent": "sm-1966",
                 "sm_url": "http://127.0.0.1:18450"}
json.dump(d, open(path, "w"), indent=1)
EOF

# opencode config: one local provider, permissions never "ask", the judge plugin.
cat > $X/config/opencode/opencode.json <<EOF
{"\$schema": "https://opencode.ai/config.json", "autoupdate": false, "share": "disabled", "snapshot": false,
 "lsp": false, "formatter": false,
 "provider": {"local": {"npm": "@ai-sdk/openai-compatible", "name": "Local Flash-Next",
   "options": {"baseURL": "$MODEL_URL", "apiKey": "local", "timeout": 3000000},
   "models": {"flash": {"name": "Qwen3.8-Flash-Next", "limit": {"context": 200000, "output": 32000}}}}},
 "model": "local/flash", "small_model": "local/flash", "enabled_providers": ["local"],
 "permission": {"*": "allow", "question": "deny", "task": "deny", "webfetch": "deny", "websearch": "deny",
   "doom_loop": "allow", "external_directory": "allow"}}
EOF
cp $K/sm_judge.js $X/config/opencode/plugins/sm_judge.js

# opencode installs its plugin SDK into the config dir from npm at start-up; the wall has no
# network, so prepare it here, outside the wall, once.
if [[ ! -d $X/config/opencode/node_modules/@opencode-ai ]]; then
  XDG_CONFIG_HOME=$X/config XDG_DATA_HOME=$X/data XDG_CACHE_HOME=$X/cache XDG_STATE_HOME=$X/state \
    opencode debug config > $R/prep.log 2>&1 || true
fi
ls -d $X/config/opencode/node_modules/@opencode-ai > /dev/null

$K/wall_profile.sh $WT $T $R $SMPORT "$MPORT $JPORT $OCPORT" > $R/profile.sb

cat > $R/env.sh <<EOF
unset TMUX TMUX_PANE ANTHROPIC_API_KEY OPENAI_API_KEY
export XDG_CONFIG_HOME=$X/config XDG_DATA_HOME=$X/data XDG_CACHE_HOME=$X/cache XDG_STATE_HOME=$X/state
export OPENCODE_DISABLE_AUTOUPDATE=1 OPENCODE_DISABLE_MODELS_FETCH=1 OPENCODE_DISABLE_LSP_DOWNLOAD=1
export PWD=$WT
export LOCAL_AGENT_ID=a1966oc0 LOCAL_JUDGE_PORT=$JPORT SM_JUDGE_PLUGIN_LOG=$R/plugin.jsonl
export SM_API_URL=http://127.0.0.1:$SMPORT CLAUDE_SESSION_MANAGER_ID=a1966oc0 SESSION_MANAGER_ID=a1966oc0
export SM_SESSION_CREDENTIAL=\$(python3 -c 'import json;print(json.load(open("/private/tmp/l1966-sm/creds.json"))["a1966oc0"])')
export NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost
export GIT_TERMINAL_PROMPT=0 TMUX_TMPDIR=$T TMPDIR=$T/tmp/ CARGO_NET_OFFLINE=true
EOF

cat > $R/launch.sh <<EOF
#!/bin/zsh
source $R/env.sh
cd $WT
sandbox-exec -f $R/profile.sb opencode serve --port $OCPORT --hostname 127.0.0.1 --print-logs --log-level INFO
echo OPENCODE-EXITED \$?; sleep 86400
EOF
tmux -L l1966 kill-session -t oc 2>/dev/null || true
tmux -L l1966 new-session -d -s oc -x 200 -y 50 -c $WT \
  env -i HOME=$HOME USER=$USER LOGNAME=$USER SHELL=/bin/zsh TERM=xterm-256color LANG=en_US.UTF-8 \
  PATH=/Users/rajesh/projects/session-manager-prod/.local/bin:$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin \
  /bin/zsh $R/launch.sh
tmux -L l1966 pipe-pane -t oc "cat >> $R/oc_serve.log"
for i in {1..60}; do curl -sf http://127.0.0.1:$OCPORT/global/health > /dev/null && break; sleep 0.5; done
curl -sf http://127.0.0.1:$OCPORT/global/health

if [[ $MODE == start ]]; then
  SID=$(curl -s -XPOST http://127.0.0.1:$OCPORT/session -H 'content-type: application/json' -d '{"title":"sm-1966-oc"}' \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
  python3 - $R/bridge.json $SID $WT $R <<'EOF'
import json, sys
path, sid, wt, r = sys.argv[1:]
json.dump({"oc_url": "http://127.0.0.1:18451", "oc_session": sid, "sm_url": "http://127.0.0.1:18450",
           "sm_id": "a1966oc0", "state_dir": r, "model_label": "local/flash-next", "context_window": 200000,
           "cwd": wt}, open(path, "w"), indent=1)
EOF
fi
echo "opencode session $(python3 -c "import json;print(json.load(open('$R/bridge.json'))['oc_session'])")"

# The bridge (event projection) and the pane sm types into run outside the wall. Both reconnect
# to opencode on their own, so a restart keeps whichever is still running; a start replaces both.
if [[ $MODE == start ]]; then
  tmux -L l1966 kill-session -t bridge 2>/dev/null || true
  tmux -L l1966-scratch kill-session -t l1966-a1966oc0 2>/dev/null || true
fi
tmux -L l1966 has-session -t bridge 2>/dev/null || \
  tmux -L l1966 new-session -d -s bridge "python3 $K/oc_bridge.py project $R/bridge.json 2>> $R/bridge.err"
tmux -L l1966-scratch has-session -t l1966-a1966oc0 2>/dev/null || \
  tmux -L l1966-scratch new-session -d -s l1966-a1966oc0 -x 160 -y 40 "python3 $K/oc_pane.py $R/bridge.json 2>> $R/pane.err"
echo "pane: tmux -L l1966-scratch attach -t l1966-a1966oc0;  opencode: tmux -L l1966 attach -t oc"
