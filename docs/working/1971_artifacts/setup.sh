#!/bin/zsh
# Build blind A/B checkouts for #1971. usage: setup.sh
set -eu
S=${0:A:h}; REPO=/Users/rajesh/projects/session-manager; G=$HOME/.local/share/local-agents-proto
export GIT_AUTHOR_NAME=author GIT_AUTHOR_EMAIL=author@example.invalid GIT_COMMITTER_NAME=author GIT_COMMITTER_EMAIL=author@example.invalid
export GIT_AUTHOR_DATE=2026-10-01T12:00:00+0000 GIT_COMMITTER_DATE=2026-10-01T12:00:00+0000
: > $S/assignment.txt
mk() {  # mk <ticket> <start> <diff file> <dest>
  rm -rf $4; git clone -q --shared $REPO $4
  git -C $4 checkout -q -b work $2; git -C $4 remote remove origin
  for b in $(git -C $4 for-each-ref --format='%(refname:short)' refs/heads | grep -vx work); do git -C $4 branch -q -D $b; done
  git -C $4 tag -l | xargs -r git -C $4 tag -d >/dev/null
  git -C $4 apply $3; git -C $4 add -A; git -C $4 commit -q -m "Change for #$1"
  git -C $4 reflog expire --expire=now --all
}
side() {  # side <ticket> <start> -> sets P (prototype diff) and N (1954 diff)
  P=$G/grades/sm-1784-proto-flash-claude-$1/local_fix.diff
  N=$S/n$1.diff; git -C $HOME/worktrees/sm-1954-$1 diff $2 HEAD > $N
}
for spec in "1855 4a3a060d6c" "1913 0d3211e3fd"; do
  T=${spec% *} START=${spec#* }; side $T $START
  if (( RANDOM % 2 )); then A=$P B=$N; echo "$T A=prototype B=1954" >> $S/assignment.txt
  else A=$N B=$P; echo "$T A=1954 B=prototype" >> $S/assignment.txt; fi
  mk $T $START $A /private/tmp/regrade-$T-A; mk $T $START $B /private/tmp/regrade-$T-B
done
