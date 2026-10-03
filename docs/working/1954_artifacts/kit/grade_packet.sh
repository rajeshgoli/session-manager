#!/bin/zsh
# usage: grade_packet.sh <ticket> <start commit> <merged commit> <original minutes>
# Builds runs-1954/<ticket>/grade/ and prints the brief path for `sm spawn --prompt-file`.
set -eu
T=$1 START=$2 M=$3 ORIG=$4
K=${0:A:h}; R=${K:h}/runs-1954; W=$HOME/worktrees/sm-1954-$T; G=$R/$T/grade
REPO=/Users/rajesh/projects/session-manager
mkdir -p $G
gh issue view $T -R rajeshgoli/session-manager --json title,body --jq '"# #'$T' \(.title)\n\n\(.body)"' > $G/issue.md
git -C $REPO show $M > $G/merged_fix.diff
git -C $W diff $START HEAD > $G/local_fix.diff
cp $R/grades/$T.log $G/test_evidence.log
gh pr view $(gh pr list --head 1954-rerun-$T --state all --json number --jq '.[0].number') --comments > $G/pr_and_review.txt
cat > $G/brief.md <<EOF
You are grading one run of a local-model prototype (sm ticket #1954). A local model (Qwen3.8-Flash-Next on MTPLX, running in an unmodified Claude Code inside a macOS sandbox, with no human at the terminal) was given ticket #$T and worked alone in its own checkout. You are not reviewing a PR. Do not edit code, commit, push, or comment on GitHub.

The local agent's checkout is \`$W\` (HEAD = its final change). Files in \`$G/\`:
- \`issue.md\`: the ticket text the local agent was given.
- \`merged_fix.diff\`: the fix that actually merged, written by a top-tier agent in about $ORIG minutes.
- \`local_fix.diff\`: the local agent's change, start commit to its head.
- \`test_evidence.log\`: the driver's test run on the local change. A = the agent's own tests at its head; B = the changed module's tests; C = the merged fix's own test applied onto the local change (it may not apply or compile if the designs differ; say whether that matters); D = fmt and clippy. Tests that need /tmp, the live sm config or a real tmux server were blocked inside the agent's sandbox, not in this evidence run.
- \`pr_and_review.txt\`: the agent's draft PR, the Codex reviews it received and its replies.

Read the code around both changes in that checkout as needed. Then answer:

1. Does the local change fix the reported bug? Yes, partly, or no, with the reason.
2. Does it add a test that fails before the fix and passes after?
3. Does it stay in scope?
4. How does its quality compare with the merged fix? Name concrete differences.
5. Verdict: mergeable as-is, small changes, large changes, or wrong.
6. Rescue effort: the fraction (0–100%) of the original agent's work still needed to bring the local change to the merged fix's standard, with one line on what that work is.
7. How did the agent handle its Codex review findings, if any: right calls, wrong calls?

Send your answers to the driver as one message, then stop:
sm send sm-1954 <<'GRADE'
#$T grade
<answers 1–7, numbered, one short paragraph each>
GRADE
EOF
echo $G/brief.md
