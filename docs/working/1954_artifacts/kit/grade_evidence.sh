#!/bin/zsh
# #1954 grading evidence (memo H.5 with H.7's evidence job), run outside the wall as one tests job.
# usage: grade_evidence.sh <out dir>
# Per run: A agent's own tests at its head; B the changed module's tests; C the merged fix's own test
# applied onto the agent's head; D fmt and clippy. Every checkout is reset to the agent's head afterwards.
set -u
OUT=$1; mkdir -p $OUT
REPO=/Users/rajesh/projects/session-manager
export CARGO_NET_OFFLINE=true
filt() { grep -E '^test |test result|panicked|error(\[|:)|^error' | head -40 }
step() { echo; echo "===== $1 =====" }

grade() {  # grade <ticket> <merged commit> <agent test filters> <module filter>
  local T=$1 M=$2 AT=$3 MOD=$4 W=$HOME/worktrees/sm-1954-$1
  cd $W; local H=$(git rev-parse HEAD)
  {
    echo "ticket #$T  agent head $H  merged $M"
    step "A. agent tests at agent head: $AT"
    for t in ${=AT}; do scripts/test-rust-isolated.sh $t 2>&1 | filt; done
    step "B. module tests at agent head: $MOD"
    scripts/test-rust-isolated.sh ${=MOD} 2>&1 | grep -E 'FAILED|failed|test result|panicked' | head -30
    step "C. merged fix's own test against the agent's change"
    case $T in
      1855) git -C $REPO show ${M}:crates/sm-server/tests/retired_review_name.rs > crates/sm-server/tests/retired_review_name.rs
            scripts/test-rust-isolated.sh --test retired_review_name 2>&1 | filt ;;
      1913) git -C $REPO show $M -- crates/sm-server/src/queue.rs | git apply --3way 2>&1 | tail -3
            scripts/test-rust-isolated.sh queue_job_does_not_inherit_handed_over_listeners 2>&1 | filt ;;
      1892) git -C $REPO show ${M}:crates/sm-server/tests/read_only_http.rs > crates/sm-server/tests/read_only_http.rs
            scripts/test-rust-isolated.sh --test read_only_http spawn 2>&1 | filt ;;
    esac
    git reset -q --hard $H; git clean -qfd -- crates
    step "D. fmt and clippy at agent head"
    cargo fmt -p sm-server --check >/dev/null 2>&1 && echo "fmt: clean" || echo "fmt: NOT clean"
    cargo clippy -p sm-server --all-targets -- -D warnings 2>&1 | grep -E '^(error|warning)' | sort | uniq -c | head -10
    echo "clippy exit ${pipestatus[1]}"
  } > $OUT/$T.log 2>&1
  echo "#$T done: $(grep -c 'test result: ok' $OUT/$T.log) ok results, $(grep -c 'FAILED' $OUT/$T.log) FAILED lines"
}

grade 1855 2100e916ec "request_codex_review_name_is_a_hidden_deprecated_alias" "--bin sm"
grade 1913 14ea14c936 "queue_job_child_does_not_inherit_the_servers_listeners lsof_scan_flags_inherited_listeners_and_production_state" "queue::tests"
grade 1892 b6ec8fd8ba "claude_spawn_brief claude_brief" "runtime::tests"
