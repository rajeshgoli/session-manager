#!/bin/zsh
# #1971 evidence: grade_evidence.sh's A-D steps on each blind checkout. usage: evidence.sh
set -u
S=${0:A:h}; REPO=/Users/rajesh/projects/session-manager
export CARGO_NET_OFFLINE=true  # each checkout builds its own target/: a shared one reuses the other checkout's binaries
filt() { grep -E '^test |test result|panicked|error(\[|:)|^error' | grep -v ' 0 passed; 0 failed; 0 ignored; 0 measured' | head -40 }
step() { print; print "===== $1 =====" }
typeset -A AT=(
  1855-prototype "--test cli_review_alias"
  1855-1954 "request_codex_review_name_is_a_hidden_deprecated_alias"
  1913-prototype "queue_job_child_does_not_inherit_server_descriptors"
  1913-1954 "queue_job_child_does_not_inherit_the_servers_listeners;lsof_scan_flags_inherited_listeners_and_production_state"
)
typeset -A MOD=(1855 "--bin sm" 1913 "queue::tests")
typeset -A MERGED=(1855 2100e916ec 1913 14ea14c936)
while read T a b; do
  for X in A B; do
    if [[ $X == A ]]; then src=${a#A=}; else src=${b#B=}; fi
    W=/private/tmp/regrade-$T-$X; P=/private/tmp/regrade-packet-$T; mkdir -p $P; cd $W; H=$(git rev-parse HEAD); M=$MERGED[$T]
    {
      print "ticket #$T  change $X"
      step "A. the change's own new tests: ${AT[$T-$src]//;/ and }"
      for t in ${(s:;:)AT[$T-$src]}; do scripts/test-rust-isolated.sh ${=t} 2>&1 | filt; done
      step "B. module tests with the change: ${MOD[$T]}"
      scripts/test-rust-isolated.sh ${=MOD[$T]} 2>&1 | grep -E 'FAILED|failed|test result|panicked' | grep -v ' 0 passed; 0 failed' | head -30
      step "C. merged fix's own test applied onto the change"
      case $T in
        1855) git -C $REPO show ${M}:crates/sm-server/tests/retired_review_name.rs > crates/sm-server/tests/retired_review_name.rs
              scripts/test-rust-isolated.sh --test retired_review_name 2>&1 | filt ;;
        1913) git -C $REPO show $M -- crates/sm-server/src/queue.rs | git apply --3way 2>&1 | tail -3
              scripts/test-rust-isolated.sh queue_job_does_not_inherit_handed_over_listeners 2>&1 | filt ;;
      esac
      git reset -q --hard $H; git clean -qfd -- crates
      step "D. fmt and clippy with the change"
      cargo fmt -p sm-server --check >/dev/null 2>&1 && print "fmt: clean" || print "fmt: NOT clean"
      cargo clippy -p sm-server --all-targets -- -D warnings 2>&1 | grep -E '^(error|warning)' | sort | uniq -c | head -10
      print "clippy exit ${pipestatus[1]}"
    } > $P/test_evidence_$X.log 2>&1
    print "#$T $X done: $(grep -c 'test result: ok' $P/test_evidence_$X.log) ok, $(grep -c 'FAILED' $P/test_evidence_$X.log) FAILED lines"
  done
done < $S/assignment.txt
