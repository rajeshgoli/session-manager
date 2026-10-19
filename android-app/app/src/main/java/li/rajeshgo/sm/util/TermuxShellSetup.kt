package li.rajeshgo.sm.util

import li.rajeshgo.sm.data.model.PlainTerminalRoute

/** Only the public half leaves Termux. A missing .pub is recovered from the existing private key. */
fun termuxShellSetupScript(route: PlainTerminalRoute): String {
    require(route.sshUser.matches(Regex("[A-Za-z_][A-Za-z0-9_.-]*"))) { "Invalid SSH user" }
    require(route.sshPort in 1..65535) { "Invalid SSH port" }
    return """
        set -eu
        command -v ssh-keygen >/dev/null && command -v cloudflared >/dev/null || {
          echo 'In Termux, run: pkg install openssh cloudflared' >&2; exit 1;
        }
        umask 077
        mkdir -p ~/.ssh
        chmod 700 ~/.ssh
        if [ ! -f ~/.ssh/id_ed25519 ]; then
          ssh-keygen -q -t ed25519 -N '' -f ~/.ssh/id_ed25519 -C sm-termux
        fi
        ssh-keygen -y -P '' -f ~/.ssh/id_ed25519 > ~/.ssh/id_ed25519.pub
        tmp=${'$'}(mktemp ~/.ssh/sm-config.XXXXXX)
        trap 'rm -f "${'$'}tmp"' EXIT
        cat > "${'$'}tmp" <<'SM_HOSTS'
        # BEGIN SM SHELLS
        Host studio-lan studio-lan.rajeshgo.li
          HostName studio-lan.rajeshgo.li
          Port ${route.sshPort}
          User ${route.sshUser}
          IdentityFile ~/.ssh/id_ed25519
          IdentitiesOnly yes
        Host studio-away
          HostName studio-ssh.rajeshgo.li
          User ${route.sshUser}
          IdentityFile ~/.ssh/id_ed25519
          IdentitiesOnly yes
          ProxyCommand /data/data/com.termux/files/usr/bin/cloudflared access ssh --hostname %h
        Host *
        # END SM SHELLS
        SM_HOSTS
        if [ -f ~/.ssh/config ]; then
          awk '/^# BEGIN SM SHELLS${'$'}/ {skip=1; next} /^# END SM SHELLS${'$'}/ {skip=0; next} !skip' ~/.ssh/config >> "${'$'}tmp"
        fi
        chmod 600 "${'$'}tmp"
        mv "${'$'}tmp" ~/.ssh/config
        cat ~/.ssh/id_ed25519.pub
    """.trimIndent()
}

fun termuxPublicKey(stdout: String): String {
    val key = stdout.trim()
    require(key.matches(Regex("ssh-ed25519 [A-Za-z0-9+/]+={0,2}( [^\\r\\n]*)?"))) { "Termux did not return an Ed25519 public key" }
    return key
}
