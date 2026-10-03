#!/bin/zsh
# usage: make_brief.sh <run> <ticket> <branch> > runs-1954/<run>/brief.md
RUN=$1 TICKET=$2 BRANCH=$3; K=${0:A:h}
gh issue view $TICKET -R rajeshgoli/session-manager --json title,body --jq '"# \(.title)\n\n\(.body)"'
echo; cat $K/local_agent_addendum.md; echo
cat <<B
This run, specifically:
- You are \`sm-$TICKET-local\`; your parent is \`sm-1954\`. Your checkout is this directory, on branch \`$BRANCH\`. Your temp folder is \`/private/tmp/l1954-$RUN\`.
- This ticket was fixed before; this is a rerun to measure local agents. Your PR must never be merged. Open it as a draft: \`gh pr create --draft\`, title starting "[local-agent rerun, do not merge]", body "Rerun of #$TICKET for #1954. Do not merge." Do not write "Closes", "Fixes" or "Resolves" in it.
- After \`sm request-review\`, end your turn. A review arrives later as a message starting \`[sm review]\`; address its findings that are correct, push, and end your turn again.
- \`git\` and \`gh\` reach GitHub; nothing else on the internet is reachable. Builds use the local crate cache (offline).
- Known sandbox-only test failures, on a clean checkout too: \`sm\` CLI test \`tests::resolve_api_url_uses_existing_default\` reads \`~/.config/session-manager\`, which the sandbox hides; its panic poisons a shared lock, so eight more \`sm\` CLI tests fail with \`PoisonError\`. You do not need to investigate them.
B
if [[ $TICKET == 1892 ]]; then cat <<B
- Live reproduction is available inside your sandbox. Your own tmux server (plain \`tmux\`; its socket is in your temp folder) works. The \`claude\` binary is on PATH; give any Claude Code you start a config dir in your temp folder (\`CLAUDE_CONFIG_DIR\`) and \`ANTHROPIC_BASE_URL=http://127.0.0.1:18449\` (nothing listens there; it has no model, but Claude Code starts and writes a transcript turn when a prompt is submitted). You may run an sm-server you build yourself on ports 18441-18448 with a config and state under your temp folder. The live sm at port 8420 and its tmux server are not reachable.
B
fi
