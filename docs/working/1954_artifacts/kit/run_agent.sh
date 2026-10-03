#!/bin/zsh
# #1954: launch one local agent inside the wall with the judge hook (memo K.1-K.3, K.6).
# usage: run_agent.sh <run> <ticket> <start commit> <branch>
#   checkout  ~/worktrees/sm-1954-<run>  (shared clone; origin = GitHub over https, through the proxy)
#   temp      /private/tmp/l1954-<run>    (short: tmux and sm sockets live here)
#   run dir   runs-1954/<run>             (Claude config, settings, env, wall profile, hooks log)
# Needs, outside the wall: logging proxy :18236 -> MTPLX :8000, judge service :8431,
# egress proxy :8432, scratch sm :18440. The agent runs in Claude Code's default permission mode:
# no permission-skipping flag; the judge hook answers every tool call allow or deny.
set -euo pipefail
RUN=$1 TICKET=$2 COMMIT=$3 BRANCH=$4
K=${0:A:h}
R=${K:h}/runs-1954/$RUN
WT=$HOME/worktrees/sm-1954-$RUN
T=/private/tmp/l1954-$RUN
REPO=/Users/rajesh/projects/session-manager
AGENT=local-$RUN
mkdir -p $R/claude-config $R/hooks $T/tmp

[[ -d $WT ]] || { git clone -q --shared --no-checkout $REPO $WT && git -C $WT checkout -q -b $BRANCH $COMMIT; }
git -C $WT remote set-url origin https://github.com/rajeshgoli/session-manager.git
git -C $WT config user.name "$(git -C $REPO config user.name)"
git -C $WT config user.email "$(git -C $REPO config user.email)"

# The judge service learns who this agent is from agents.json (re-read on every call).
python3 - "$K" "$AGENT" "$RUN" "$TICKET" "$BRANCH" "$WT" "$T" <<'EOF'
import json, subprocess, sys
k, agent, run, ticket, branch, wt, t = sys.argv[1:]
path = f"{k[:-len('kit-1954')]}runs-1954/agents.json"
title = subprocess.run(["gh", "issue", "view", ticket, "-R", "rajeshgoli/session-manager", "--json", "title",
                        "--jq", ".title"], capture_output=True, text=True).stdout.strip()
d = json.load(open(path))
d[agent] = {"name": f"sm-{ticket}-local", "ticket": int(ticket), "title": title, "branch": branch,
            "checkout": wt, "tmp": t, "parent": "sm-1954", "sm_url": "http://127.0.0.1:18440"}
json.dump(d, open(path, "w"), indent=1)
EOF

# Fresh config dir: no Anthropic credential, transcripts stay out of sm's usage scanner.
cat > $R/claude-config/.claude.json <<EOF
{"hasCompletedOnboarding": true, "theme": "dark", "autoUpdates": false, "lspRecommendationDisabled": true,
 "projects": {"$WT": {"hasTrustDialogAccepted": true, "hasCompletedProjectOnboarding": true}}}
EOF
# The wall makes ~/.claude unreadable, so the agent gets its own copy of sm's hooks.
cp -p ~/.claude/hooks/* $R/hooks/
H=$R/hooks
cat > $R/settings.json <<EOF
{
  "permissions": {"defaultMode": "default"},
  "sandbox": {"enabled": false},
  "hooks": {
    "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "$H/notify_server.sh"}]}],
    "Stop": [{"hooks": [{"type": "command", "command": "$H/notify_server.sh"}]}],
    "PreCompact": [{"hooks": [{"type": "command", "command": "$H/precompact_notify.sh"}]}],
    "SessionStart": [
      {"matcher": "clear", "hooks": [{"type": "command", "command": "$H/session_clear_notify.sh"}]},
      {"matcher": "compact", "hooks": [{"type": "command", "command": "$H/post_compact_recovery.sh"}]}
    ],
    "PreToolUse": [{"hooks": [
      {"type": "command", "command": "$K/judge_hook.sh", "timeout": 60},
      {"type": "command", "command": "$REPO/hooks/log_tool_use.sh"}
    ]}]
  },
  "statusLine": {"type": "command", "command": "$H/context_monitor.sh"}
}
EOF

$K/wall_profile.sh $WT $T $R 18440 > $R/profile.sb

P="http://$AGENT:x@127.0.0.1:8432"
cat > $R/env.sh <<EOF
unset TMUX TMUX_PANE ANTHROPIC_API_KEY
export CLAUDE_CONFIG_DIR=$R/claude-config
export ANTHROPIC_BASE_URL=http://127.0.0.1:18236
export ANTHROPIC_AUTH_TOKEN=local
export ANTHROPIC_MODEL=proto
export ANTHROPIC_SMALL_FAST_MODEL=proto
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
export CLAUDE_CODE_ATTRIBUTION_HEADER=0
export CLAUDE_CODE_TOTAL_TOKENS_REMINDER=off
export API_TIMEOUT_MS=3000000
export CLAUDE_CODE_AUTO_COMPACT_WINDOW=200000
export CLAUDE_CODE_EFFORT_LEVEL=medium
export ENABLE_TOOL_SEARCH=false
export SM_HOST=local
export SM_HOOK_BASE_URL=http://127.0.0.1:18236
export SM_API_URL=http://127.0.0.1:18440
export CLAUDE_SESSION_MANAGER_ID=\$(cat $R/sm_id 2>/dev/null) SESSION_MANAGER_ID=\$(cat $R/sm_id 2>/dev/null)
export SM_SESSION_CREDENTIAL=\$(cat $R/sm_credential 2>/dev/null)
export CLAUDE_HOOK_LOG_PATH=$R/hooks.log
export LOCAL_AGENT_ID=$AGENT
export LOCAL_JUDGE_PORT=8431
export HTTPS_PROXY=$P https_proxy=$P NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost
export GIT_TERMINAL_PROMPT=0
export GIT_CONFIG_COUNT=2 GIT_CONFIG_KEY_0=credential.helper GIT_CONFIG_VALUE_0= GIT_CONFIG_KEY_1=credential.helper GIT_CONFIG_VALUE_1='!gh auth git-credential'
export TMUX_TMPDIR=$T
export TMPDIR=$T/tmp/
export CARGO_NET_OFFLINE=true
EOF

cat > $R/launch.sh <<EOF
#!/bin/zsh
source $R/env.sh
sandbox-exec -f $R/profile.sb claude --model proto --strict-mcp-config --mcp-config '{"mcpServers":{}}' \\
  --disable-slash-commands --tools Bash,Read,Edit,Write,Glob,Grep,TodoWrite --settings $R/settings.json
echo CLAUDE-EXITED; sleep 86400
EOF
# A clean environment: nothing from the driver's own Claude Code session leaks in.
tmux -L l1954 new-session -d -s $RUN -x 220 -y 60 -c $WT \
  env -i HOME=$HOME USER=$USER LOGNAME=$USER SHELL=/bin/zsh TERM=xterm-256color LANG=en_US.UTF-8 \
  PATH=/Users/rajesh/projects/session-manager-prod/.local/bin:$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin \
  /bin/zsh $R/launch.sh
echo "agent $AGENT: tmux -L l1954 attach -t $RUN; checkout $WT on $BRANCH; temp $T"
