#!/bin/zsh
# #1966: validate the opencode #1913 run as #1955 did (M.3) on a fresh clone of the agent commit:
# own test, queue module, the merged fix's test (applied from 14ea14c936), each without the fix too; fmt, clippy.
# usage: sm queue run --type tests --label 1966-validate-1913 -- validate_1913.sh <full agent commit> <log dir>
set -u
C=$1 L=$2 REPO=/Users/rajesh/projects/session-manager
V=$HOME/worktrees/sm-1966-validate-1913
mkdir -p $L; rm -f $L/summary.txt
[[ -d $V ]] || git clone -q --shared $REPO $V
git -C $V fetch -q $HOME/worktrees/sm-1966-oc-d 1966-oc-1913 || { echo "fetch failed"; exit 2; }
git -C $V checkout -q -f $C || { echo "checkout failed"; exit 2; }
[[ $(git -C $V rev-parse HEAD) == $C ]] || { echo "not on $C"; exit 2; }
cd $V
OWN=queue_job_child_inherits_no_descriptors_above_stdio MERGED=queue_job_does_not_inherit_handed_over_listeners
FIX='        crate::runtime::close_inherited_descriptors_before_exec(&mut command);'
run() { local name=$1; shift; print "== $name: $*" >> $L/summary.txt
  "$@" > $L/$name.log 2>&1; local rc=$?; print "   exit $rc; $(grep -E 'test result:|^error' $L/$name.log | tr '\n' ' ')" >> $L/summary.txt; }
run F-fmt cargo fmt -p sm-server --check
run G-clippy cargo clippy -p sm-server --all-targets -- -D warnings
run A-own-test scripts/test-rust-isolated.sh -p sm-server --lib $OWN
run B-queue-module scripts/test-rust-isolated.sh -p sm-server --lib queue::tests
git -C $REPO show --format= 14ea14c936 -- crates/sm-server/src/queue.rs | git apply --whitespace=nowarn || { echo "merged test patch failed" >> $L/summary.txt; }
run C-merged-test scripts/test-rust-isolated.sh -p sm-server --lib $MERGED
[[ $(grep -cF -- "$FIX" crates/sm-server/src/queue.rs) == 1 ]] || { echo "fix line not found once" >> $L/summary.txt; exit 3; }
grep -vF -- "$FIX" crates/sm-server/src/queue.rs > /tmp/l1966-q.rs && mv /tmp/l1966-q.rs crates/sm-server/src/queue.rs
run D-own-test-without-fix scripts/test-rust-isolated.sh -p sm-server --lib $OWN
run E-merged-test-without-fix scripts/test-rust-isolated.sh -p sm-server --lib $MERGED
git -C $V checkout -q -f $C
cat $L/summary.txt
