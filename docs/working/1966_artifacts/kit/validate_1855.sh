#!/bin/zsh
# #1966: validate the opencode #1855 run as #1955 did (M.3): own test and the merged fix's test, with
# and without the agent's fix, plus fmt and clippy. Works on a fresh clone, never the agent's checkout.
# usage: sm queue run --type tests --label 1966-validate-1855 -- validate_1855.sh <agent commit> <log dir>
set -u
C=$1 L=$2 REPO=/Users/rajesh/projects/session-manager
V=$HOME/worktrees/sm-1966-validate-1855
mkdir -p $L
[[ -d $V ]] || git clone -q --shared $REPO $V
git -C $V fetch -q $HOME/worktrees/sm-1966-oc-c 1966-oc-1855 || { echo "fetch failed"; exit 2; }
git -C $V checkout -q -f $C || { echo "checkout $C failed"; exit 2; }
[[ $(git -C $V rev-parse HEAD) == $(git -C $V rev-parse $C) ]] || { echo "not on $C"; exit 2; }
cd $V
rm -f $L/summary.txt
OWN=request_codex_review_parses_as_hidden_alias
run() { local name=$1; shift; print "== $name: $*" >> $L/summary.txt
  "$@" > $L/$name.log 2>&1; local rc=$?; print "   exit $rc; $(grep -E 'test result:|^error' $L/$name.log | tr '\n' ' ')" >> $L/summary.txt; }
git show 2100e916:crates/sm-server/tests/retired_review_name.rs > crates/sm-server/tests/retired_review_name.rs
grep -q $OWN crates/sm-server/src/bin/sm.rs || { echo "agent test missing"; exit 2; }
run A-own-test scripts/test-rust-isolated.sh -p sm-server --bin sm $OWN
run B-merged-test scripts/test-rust-isolated.sh -p sm-server --test retired_review_name
run C-fmt cargo fmt -p sm-server --check
run D-clippy cargo clippy -p sm-server --all-targets -- -D warnings
# Without the fix: restore the original sm.rs but keep the agent's test, then rerun both tests.
git show $C~1:crates/sm-server/src/bin/sm.rs > /tmp/l1966-orig-sm.rs
python3 - <<'EOF'
import re
fixed = open("crates/sm-server/src/bin/sm.rs").read()
orig = open("/tmp/l1966-orig-sm.rs").read()
# Keep the agent's test module, revert everything else: the test module starts at the last "#[cfg(test)]".
i, j = fixed.rindex("#[cfg(test)]"), orig.rindex("#[cfg(test)]")
open("crates/sm-server/src/bin/sm.rs", "w").write(orig[:j] + fixed[i:])
EOF
run E-own-test-without-fix scripts/test-rust-isolated.sh -p sm-server --bin sm $OWN
run F-merged-test-without-fix scripts/test-rust-isolated.sh -p sm-server --test retired_review_name
git -C $V checkout -q -f $C; rm -f crates/sm-server/tests/retired_review_name.rs /tmp/l1966-orig-sm.rs
cat $L/summary.txt
