#!/bin/bash
# Keep this Mac's browsers off HTTP/3 for the sm browser host.
#
# Chrome on macOS does not send a client certificate over HTTP/3, so device
# certificate sign-in to Cloudflare Access silently falls back to email. Chrome
# learns HTTP/3 from the zone's DNS HTTPS record, which no per-host Cloudflare
# rule removes. This blocks outbound UDP 443 to exactly the host's current
# addresses, so Chrome's HTTP/3 attempt fails and it uses HTTP/2 instead.
# Other hostnames that share those Cloudflare addresses also use HTTP/2 here.
#
#   sudo scripts/browser-http2-pin.sh install sm.rajeshgo.li
#   sudo scripts/browser-http2-pin.sh uninstall
#   scripts/browser-http2-pin.sh status
#
# install copies this script to a root-owned path and registers a LaunchDaemon
# that runs `refresh` at boot and every 5 minutes, following address changes.
set -euo pipefail

LABEL=li.rajeshgo.sm.browser-http2-pin
ANCHOR=com.apple/sm-browser-http2-pin
INSTALLED=/usr/local/libexec/sm-browser-http2-pin
PLIST=/Library/LaunchDaemons/$LABEL.plist
STATE=/var/db/sm-browser-http2-pin.addresses

need_root() { [ "$(id -u)" -eq 0 ] || { echo "run with sudo" >&2; exit 1; }; }

resolve() {
    { /usr/bin/dig +short A "$1"; /usr/bin/dig +short AAAA "$1"; } |
        grep -E '^([0-9]+\.){3}[0-9]+$|^[0-9a-fA-F:]+:[0-9a-fA-F:]*$' | sort -u || true
}

refresh() {
    need_root
    local host=$1 addresses
    addresses=$(resolve "$host")
    # A failed lookup keeps the last rules: dropping them would silently
    # bring back email sign-in until the next successful refresh.
    if [ -z "$addresses" ]; then
        echo "no addresses for $host; keeping current rules" >&2
        return 0
    fi
    /sbin/pfctl -s info 2>/dev/null | grep -q 'Status: Enabled' || /sbin/pfctl -e 2>/dev/null || true
    if [ "$addresses" = "$(cat "$STATE" 2>/dev/null)" ] &&
        /sbin/pfctl -a "$ANCHOR" -s rules 2>/dev/null | grep -q 'port = 443'; then
        return 0
    fi
    printf 'table <sm_browser_host> { %s }\nblock return out quick proto udp from any to <sm_browser_host> port 443\n' \
        "$(echo $addresses | tr ' ' ',')" | /sbin/pfctl -a "$ANCHOR" -f - 2>/dev/null
    # End HTTP/3 connections already open to these addresses so the browser
    # reconnects over HTTP/2 now, not when the old connection idles out.
    for address in $addresses; do
        case $address in *:*) any=::/0 ;; *) any=0.0.0.0/0 ;; esac
        /sbin/pfctl -k "$any" -k "$address" >/dev/null 2>&1 || true
    done
    echo "$addresses" >"$STATE"
    echo "$(date '+%F %T') blocking HTTP/3 to $host: $(echo $addresses)"
}

install() {
    need_root
    local host=$1
    mkdir -p "$(dirname "$INSTALLED")"
    cp "$0" "$INSTALLED" && chown root:wheel "$INSTALLED" && chmod 755 "$INSTALLED"
    cat >"$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>$LABEL</string>
    <key>ProgramArguments</key>
    <array><string>$INSTALLED</string><string>refresh</string><string>$host</string></array>
    <key>RunAtLoad</key><true/>
    <key>StartInterval</key><integer>300</integer>
    <key>StandardErrorPath</key><string>/var/log/sm-browser-http2-pin.log</string>
    <key>StandardOutPath</key><string>/var/log/sm-browser-http2-pin.log</string>
</dict>
</plist>
EOF
    chown root:wheel "$PLIST" && chmod 644 "$PLIST"
    /bin/launchctl bootout system "$PLIST" 2>/dev/null || true
    rm -f "$STATE"
    /bin/launchctl bootstrap system "$PLIST"
    sleep 2
    status
    echo "Installed. Quit Chrome completely and reopen it."
}

uninstall() {
    need_root
    /bin/launchctl bootout system "$PLIST" 2>/dev/null || true
    rm -f "$PLIST" "$INSTALLED" "$STATE"
    /sbin/pfctl -a "$ANCHOR" -F all 2>/dev/null || true
    echo "Removed. pf stays enabled with only the macOS default rules."
}

status() {
    /bin/launchctl print "system/$LABEL" 2>/dev/null | grep -E '^\s*(state|last exit code)' || echo "LaunchDaemon not loaded"
    if [ "$(id -u)" -eq 0 ]; then /sbin/pfctl -a "$ANCHOR" -s rules 2>/dev/null; fi
    echo "addresses: $(tr "\n" " " 2>/dev/null <"$STATE")"
}

case "${1:-}" in
    install) install "${2:?usage: install <browser hostname>}" ;;
    refresh) refresh "${2:?usage: refresh <browser hostname>}" ;;
    uninstall) uninstall ;;
    status) status ;;
    *) echo "usage: $0 install <host> | refresh <host> | uninstall | status" >&2; exit 2 ;;
esac
