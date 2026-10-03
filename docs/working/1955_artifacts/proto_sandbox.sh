#!/bin/zsh
# usage: proto_sandbox.sh <run-name> > profile.sb
# macOS sandbox profile for one #1926 prototype run (Claude Code or opencode, whole process).
# Writes: the run's clone, its temp dirs, ~/.cargo, the run's own config dir.
# Network: loopback only, minus the live sm server; no outbound beyond this Mac.
# Blocks: live sm/tmux/handover sockets and credential reads.
set -eu
RUN=$1
S=${0:A:h}
WT=$HOME/worktrees/sm-1784-proto-$RUN
TMP=${TMPDIR%/}
ESC=${WT//\//-}
cat <<EOF
(version 1)
(allow default)

(deny file-write*)
(allow file-write*
  (subpath "$WT")
  (subpath "$S/runs/$RUN")
  (subpath "$TMP")
  (subpath "/private$TMP")
  (subpath "/private/tmp/claude-501/$ESC")
  (subpath "$HOME/.cargo")
  (subpath "$HOME/.local/share/opencode") (subpath "$HOME/.cache/opencode") (subpath "$HOME/.config/opencode")
  (subpath "/dev"))

(deny file-read*
  (subpath "$HOME/.ssh") (subpath "$HOME/.claude") (subpath "$HOME/.codex") (subpath "$HOME/.config/gh")
  (subpath "$HOME/.config/session-manager") (subpath "$HOME/Library/Keychains") (subpath "$HOME/.aws")
  (literal "$HOME/.claude.json") (literal "$HOME/.netrc"))

(deny network-outbound)
(allow network-outbound (remote ip "localhost:*") (remote unix-socket))
(deny network-outbound (remote ip "localhost:8420"))
(deny network-outbound
  (remote unix-socket (path-literal "/private/tmp/tmux-501/session-manager"))
  (remote unix-socket (path-literal "/private/tmp/tmux-501/default"))
  (remote unix-socket (subpath "$HOME/.local/share/claude-sessions")))
EOF
