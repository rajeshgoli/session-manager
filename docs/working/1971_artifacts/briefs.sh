#!/bin/zsh
# Build per-grader packets and briefs for #1971 (g1 = labels as assigned, g2 = A and B swapped). Run after evidence.sh.
set -eu
REPO=/Users/rajesh/projects/session-manager; G=$HOME/.local/share/local-agents-proto
typeset -A MERGED=(1855 2100e916ec 1913 14ea14c936) ORIG=(1855 7 1913 14)
for T in 1855 1913; do
  for g in g1 g2; do
    D=/private/tmp/regrade-$T-$g; rm -rf $D; mkdir -p $D
    if [[ $g == g1 ]]; then map=(A A B B); else map=(A B B A); fi   # label -> checkout
    for lab src in $map; do
      ln -s /private/tmp/regrade-$T-$src $D/checkout-$lab
      git -C /private/tmp/regrade-$T-$src diff HEAD~1 HEAD > $D/change_$lab.diff
      sed "1s/change $src/change $lab/" /private/tmp/regrade-packet-$T/test_evidence_$src.log > $D/test_evidence_$lab.log
    done
    cp $G/runs-1954/$T/grade/issue.md $D/issue.md
    git -C $REPO show $MERGED[$T] > $D/merged_fix.diff
    cat > $D/brief.md <<BRIEF
You are grading two candidate fixes, A and B, for sm ticket #$T. Each was written by an agent working alone in its own checkout. You are not reviewing a PR. Do not edit code, commit, push, or comment on GitHub.

Grade only from the files below and the two checkouts. Do not read \`docs/working/\` in any checkout or in /Users/rajesh/projects/session-manager, \`~/.local/share/local-agents-proto/\`, GitHub pull requests or issues other than #$T, or any other directory under /private/tmp. The test evidence was run for you; do not build or run tests.

Files in \`$D/\`:
- \`issue.md\`: the ticket text both agents were given.
- \`merged_fix.diff\`: the fix that actually merged, written by a top-tier agent in about $ORIG[$T] minutes.
- \`change_A.diff\`, \`change_B.diff\`: each candidate's change against the same start commit.
- \`checkout-A/\`, \`checkout-B/\`: each candidate's checkout, HEAD = its change.
- \`test_evidence_A.log\`, \`test_evidence_B.log\`: the same test run on each change. A = the change's own new tests; B = the changed module's tests; C = the merged fix's own test applied onto the change (it may not apply or compile if the designs differ; say whether that matters); D = fmt and clippy. Both runs used the same machine and environment.

Read the code around the changes as needed. Then answer questions 1–6 for A, then for B:

1. Does the change fix the reported bug? Yes, partly, or no, with the reason.
2. Does it add a test that fails before the fix and passes after?
3. Does it stay in scope?
4. How does its quality compare with the merged fix? Name concrete differences.
5. Verdict: mergeable as-is, small changes, large changes, or wrong.
6. Rescue effort: the fraction (0–100%) of the original agent's work still needed to bring the change to the merged fix's standard, with one line on what that work is.

Then answer 7: Which is closer to mergeable, and by how much?

Send your answers to the driver as one message, then stop:
sm send sm-1971 <<'GRADE'
#$T $g grade
A: <answers 1–6, numbered, one short paragraph each>
B: <answers 1–6, numbered, one short paragraph each>
7. <which is closer to mergeable, and by how much>
GRADE
BRIEF
  done
done
