#!/bin/zsh
# #1954: check the wall (K.1) and the egress proxy (K.2) from inside sandbox-exec.
# usage: wall_check.sh <checkout> <agent temp folder> <run dir> <sm port>
# Needs the egress proxy on 8432. Each line: PASS/FAIL, expectation, check.
WT=${1:A} T=${2:A} R=${3:A} SMPORT=$4; K=${0:A:h}
mkdir -p $T $R; $K/wall_profile.sh $WT $T $R $SMPORT > $R/profile.sb
P="http://wallcheck:x@127.0.0.1:8432"
run() { perl -e "alarm 30; exec @ARGV" sandbox-exec -f $R/profile.sb /usr/bin/env -u TMUX /bin/zsh -c "export TMUX_TMPDIR=$T HTTPS_PROXY=$P https_proxy=$P GIT_TERMINAL_PROMPT=0 GIT_CONFIG_COUNT=2 GIT_CONFIG_KEY_0=credential.helper GIT_CONFIG_VALUE_0= GIT_CONFIG_KEY_1=credential.helper GIT_CONFIG_VALUE_1=\"!gh auth git-credential\"; $1" >/dev/null 2>&1 }
pass=0 fail=0
expect() {  # expect ok|blocked <label> <command>
  run "$3"; rc=$?
  if [[ ($1 == ok && $rc == 0) || ($1 == blocked && $rc != 0) ]]; then print -r "PASS  $1  $2"; ((pass++))
  else print "FAIL  $1  $2  (rc=$rc)"; ((fail++)); fi
}
expect ok      "write in checkout"            "touch $WT/.wallcheck && rm $WT/.wallcheck"
expect ok      "write in temp folder"         "touch $T/x && rm $T/x"
expect ok      "write ~/.cargo"               "touch ~/.cargo/.wallcheck && rm ~/.cargo/.wallcheck"
expect ok      "Claude Code cwd file"         "echo /x > /tmp/claude-ab12-cwd && rm /tmp/claude-ab12-cwd"
expect blocked "write /tmp root"              "touch /tmp/l1954-other"
expect blocked "write home"                   "touch ~/.wallcheck"
expect blocked "write main repo"              "touch $HOME/projects/session-manager/.wallcheck"
expect blocked "write sm state"               "touch ~/.local/share/claude-sessions/.wallcheck"
expect blocked "read ~/.ssh"                  "ls ~/.ssh"
expect blocked "read ~/.claude"               "ls ~/.claude"
expect blocked "read ~/.claude.json"          "cat ~/.claude.json"
expect blocked "read sm config"               "cat ~/.config/session-manager/config.yaml"
expect blocked "read keychains"               "ls ~/Library/Keychains"
expect ok      "read sm logs"                 "ls ~/.local/share/claude-sessions"
expect ok      "read gh config"               "test -r ~/.config/gh/hosts.yml && head -c1 ~/.config/gh/hosts.yml"
expect blocked "live sm 8420"                 "curl -s --max-time 3 http://127.0.0.1:8420/health"
expect blocked "model server direct 8000"     "nc -z -w 2 127.0.0.1 8000"
expect blocked "live sm LAN listener 8443"    "nc -z -w 2 127.0.0.1 8443"
expect blocked "another agent proxy 1236"     "nc -z -w 2 127.0.0.1 1236"
expect blocked "example.com direct"           "curl -s --noproxy '*' --max-time 5 https://example.com"
expect blocked "example.com via proxy"        "curl -s --max-time 5 https://example.com"
expect blocked "1.1.1.1 direct"               "curl -s --noproxy '*' --max-time 5 https://1.1.1.1"
expect blocked "DNS lookup"                   "python3 -c 'import socket; socket.getaddrinfo(\"example.com\", 443)'"
expect ok      "github via proxy"             "curl -s -f --max-time 10 https://api.github.com/zen"
expect ok      "gh api via proxy"             "gh api user --jq .login"
expect ok      "git ls-remote via proxy"      "git ls-remote https://github.com/rajeshgoli/session-manager.git refs/heads/main"
expect blocked "sm tmux socket"               "tmux -L session-manager ls"
expect blocked "default tmux socket"          "tmux -S /private/tmp/tmux-501/default ls"
expect blocked "driver tmux socket"           "tmux -L l1954 ls"
expect ok      "private tmux socket"          "tmux new-session -d -s wc 'sleep 5' && tmux ls && tmux kill-server"
expect ok      "loopback listen on 18441"      "python3 -c 'import socket,threading; s=socket.socket(); s.bind((\"127.0.0.1\",18441)); s.listen(1); c=socket.create_connection((\"127.0.0.1\",18441)); print(1)'"
print "$pass passed, $fail failed"
