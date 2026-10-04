#!/bin/zsh
# #1954 sandbox wall (memo K.1): macOS sandbox-exec profile around one whole Claude Code process.
# usage: wall_profile.sh <checkout> <agent temp folder> <run dir> <sm port> ["<own ports>"] > profile.sb
# #1966: the fifth argument replaces #1954's fixed own-service ports (18236 8431 8432).
# Writes: checkout, agent temp folder, run dir (its Claude config, hooks log), ~/.cargo, /dev,
#   Claude Code's per-project temp, and the macOS per-user temp (toolchain caches write there).
# Reads: everything but credentials. Network: loopback only, and no DNS. Allowed: the logging proxy
#   in front of the model (18236), the judge service (8431), the egress proxy (8432), the agent's sm
#   port and any port nothing was listening on at launch (tests open throwaway servers). Unix sockets: no tmux server but the agent's own (TMUX_TMPDIR in its temp folder).
set -eu
WT=${1:A} T=${2:A} R=${3:A} SMPORT=$4
ESC=${WT//\//-}
UTMP=$(getconf DARWIN_USER_TEMP_DIR); UTMP=${UTMP%/}
cat <<P
(version 1)
(allow default)

(deny file-write*)
(allow file-write*
  (subpath "$WT") (subpath "/private$WT")
  (subpath "$T") (subpath "/private$T")
  (subpath "$R")
  (subpath "/private/tmp/claude-501/$ESC") (subpath "/private/tmp/claude-501/-private$ESC")
  (subpath "$UTMP") (subpath "/private$UTMP")
  (subpath "$HOME/.cargo")
  (regex #"^/private/tmp/claude-[0-9a-f]+-cwd$")
  (subpath "/dev"))

(deny file-read*
  (subpath "$HOME/.ssh") (subpath "$HOME/.claude") (subpath "$HOME/.codex")
  (subpath "$HOME/.config/session-manager") (subpath "$HOME/Library/Keychains") (subpath "$HOME/.aws")
  (literal "$HOME/.claude.json") (literal "$HOME/.netrc"))

(deny network-outbound)
(allow network-outbound (remote ip "localhost:*") (remote unix-socket))
P
# Every loopback port something else already listens on is denied, except the agent's own services:
# the live sm (8420, 8443), the model (8000), LM Studio, other agents' proxies, anything else running.
# Ports opened later (a test's throwaway server, the agent's own scratch sm) stay reachable.
OWN=" ${5:-18236 8431 8432} $SMPORT "
lsof -nP -iTCP -sTCP:LISTEN 2>/dev/null | awk 'NR>1 {n=split($9,a,":"); print a[n]}' | sort -un | while read p; do
  [[ $OWN == *" $p "* ]] || print "(deny network-outbound (remote ip \"localhost:$p\"))"
done
for p in 8420 8443 8000 1234 1235 1236; do print "(deny network-outbound (remote ip \"localhost:$p\"))"; done
cat <<P
(deny network-outbound
  (remote unix-socket (path-literal "/private/var/run/mDNSResponder"))
  (remote unix-socket (subpath "/private/tmp/tmux-501"))
  (remote unix-socket (subpath "$HOME/.local/share/claude-sessions")))
P
