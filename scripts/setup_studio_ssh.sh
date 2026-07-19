#!/usr/bin/env bash
#
# setup_studio_ssh.sh — Idempotent installer for the Studio SSH toggle (Workstream B).
#
# Provisions everything needed to expose `ssh studio-away` (-> studio-ssh.rajeshgo.li),
# key-only, over a dedicated cloudflared tunnel, and leaves both LaunchAgents installed
# but DISABLED so the default state is OFF. The Rust server (Workstream A) flips them
# on/off via launchctl at runtime.
#
# Steps (all idempotent):
#   1. Create the `studio-ssh` cloudflared tunnel (if absent) + write its config.yml.
#   2. Route DNS studio-ssh.rajeshgo.li -> the tunnel.
#   3. Generate a dedicated ed25519 host key + write the key-only, loopback sshd_config.
#   4. Write both LaunchAgent plists (sshd + tunnel).
#   5. Bootout (if loaded) + disable both agents  => default OFF.
#
# Usage:
#   scripts/setup_studio_ssh.sh            # real run (orchestrator runs this)
#   scripts/setup_studio_ssh.sh --dry-run  # print every action, touch nothing outward-facing
#   scripts/setup_studio_ssh.sh --sshd-assets-only   # only step 3 (local sshd assets); used for verification
#
# Test/override env vars (all default to the frozen spec constants; the orchestrator's
# plain run uses the defaults and produces spec-exact output):
#   STUDIO_SSH_ASSETS_DIR   sshd assets dir      (default ~/.local/share/session-manager/studio-ssh)
#   STUDIO_SSH_AUTHORIZED_KEYS  AuthorizedKeysFile (default /Users/rajesh/.ssh/authorized_keys)
#   STUDIO_SSH_PORT         loopback sshd port   (default 22222)
#   STUDIO_SSH_LA_DIR       LaunchAgents dir     (default ~/Library/LaunchAgents)
#
set -euo pipefail

# ---------------------------------------------------------------------------
# Frozen shared constants (see specs/900_studio_ssh_toggle.md)
# ---------------------------------------------------------------------------
TUNNEL_NAME="studio-ssh"
PUBLIC_HOSTNAME="studio-ssh.rajeshgo.li"
SSHD_LABEL="com.rajesh.sm-studio-ssh-sshd"
TUNNEL_LABEL="com.rajesh.sm-studio-ssh-tunnel"
CLOUDFLARED_BIN="/opt/homebrew/bin/cloudflared"
SSHD_BIN="/usr/sbin/sshd"

# ---------------------------------------------------------------------------
# Derived / overridable paths
# ---------------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

UID_NUM="$(/usr/bin/id -u)"
LAUNCHD_DOMAIN="gui/${UID_NUM}"

SSHD_PORT="${STUDIO_SSH_PORT:-22222}"
ASSETS_DIR="${STUDIO_SSH_ASSETS_DIR:-${HOME}/.local/share/session-manager/studio-ssh}"
AUTHORIZED_KEYS="${STUDIO_SSH_AUTHORIZED_KEYS:-/Users/rajesh/.ssh/authorized_keys}"
LA_DIR="${STUDIO_SSH_LA_DIR:-${HOME}/Library/LaunchAgents}"

CF_DIR="${REPO_DIR}/.local/studio-ssh/cloudflared"
CF_CONFIG="${CF_DIR}/config.yml"

HOST_KEY="${ASSETS_DIR}/ssh_host_ed25519_key"
SSHD_CONFIG="${ASSETS_DIR}/sshd_config"
SSHD_PID="${ASSETS_DIR}/sshd.pid"

SSHD_PLIST="${LA_DIR}/${SSHD_LABEL}.plist"
TUNNEL_PLIST="${LA_DIR}/${TUNNEL_LABEL}.plist"

DRY_RUN=0
SSHD_ASSETS_ONLY=0

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
log()  { printf '  %s\n' "$*"; }
step() { printf '\n==> %s\n' "$*"; }
warn() { printf 'WARN: %s\n' "$*" >&2; }

# Run a command, or just print it under --dry-run. Use for anything OUTWARD-FACING
# (tunnel create, DNS route, launchctl). Local file writes are gated separately.
run() {
  if [[ "${DRY_RUN}" -eq 1 ]]; then
    printf '  [dry-run] %s\n' "$*"
    return 0
  fi
  "$@"
}

# Write a file (heredoc via stdin), or print intent under --dry-run.
write_file() {
  local dest="$1"
  if [[ "${DRY_RUN}" -eq 1 ]]; then
    printf '  [dry-run] write %s:\n' "${dest}"
    sed 's/^/      | /'
    return 0
  fi
  cat > "${dest}"
}

mkdirp() {
  if [[ "${DRY_RUN}" -eq 1 ]]; then
    printf '  [dry-run] mkdir -p %s\n' "$1"
  else
    mkdir -p "$1"
  fi
}

# Print the UUID of the `studio-ssh` tunnel, or empty if it does not exist.
# Read-only; safe in dry-run. cloudflared returns JSON `null` when nothing matches.
tunnel_uuid() {
  command -v "${CLOUDFLARED_BIN}" >/dev/null 2>&1 || return 0
  "${CLOUDFLARED_BIN}" tunnel list --name "${TUNNEL_NAME}" --output json 2>/dev/null \
    | jq -r 'if type=="array" then (.[0].id // "") else "" end' 2>/dev/null
}

# ---------------------------------------------------------------------------
# Step 1 — cloudflared tunnel + config.yml
# ---------------------------------------------------------------------------
setup_tunnel() {
  step "Step 1: cloudflared tunnel '${TUNNEL_NAME}' + config.yml"
  mkdirp "${CF_DIR}"

  local uuid=""
  # Look up an existing tunnel by name (works in both real + dry-run). cloudflared
  # returns `null` (not `[]`) when no tunnel matches, hence the array-type guard.
  uuid="$(tunnel_uuid || true)"

  if [[ -n "${uuid}" ]]; then
    log "Tunnel '${TUNNEL_NAME}' already exists: ${uuid}"
  else
    if [[ "${DRY_RUN}" -eq 1 ]]; then
      log "[dry-run] ${CLOUDFLARED_BIN} tunnel create ${TUNNEL_NAME}"
      uuid="<uuid>"
    else
      log "Creating tunnel '${TUNNEL_NAME}'..."
      "${CLOUDFLARED_BIN}" tunnel create "${TUNNEL_NAME}"
      uuid="$(tunnel_uuid || true)"
      if [[ -z "${uuid}" ]]; then
        warn "Could not determine tunnel UUID after create"; exit 1
      fi
    fi
  fi

  # cloudflared writes creds to ~/.cloudflared/<uuid>.json on create. Mirror it into the
  # repo-local cloudflared dir so the tunnel plist is self-contained.
  local src_creds="${HOME}/.cloudflared/${uuid}.json"
  local dst_creds="${CF_DIR}/${uuid}.json"
  if [[ "${DRY_RUN}" -eq 1 ]]; then
    log "[dry-run] copy creds ${src_creds} -> ${dst_creds}"
  else
    if [[ -f "${src_creds}" ]]; then
      cp -f "${src_creds}" "${dst_creds}"
      chmod 600 "${dst_creds}"
      log "Credentials: ${dst_creds}"
    elif [[ -f "${dst_creds}" ]]; then
      log "Credentials already present: ${dst_creds}"
    else
      warn "Tunnel credentials ${src_creds} not found (expected after create)"
    fi
  fi

  write_file "${CF_CONFIG}" <<EOF
tunnel: ${uuid}
credentials-file: ${CF_DIR}/${uuid}.json
ingress:
  - hostname: ${PUBLIC_HOSTNAME}
    service: ssh://127.0.0.1:${SSHD_PORT}
  - service: http_status:404
EOF
  log "Wrote ${CF_CONFIG}"
}

# ---------------------------------------------------------------------------
# Step 2 — DNS route
# ---------------------------------------------------------------------------
setup_dns() {
  step "Step 2: DNS route ${PUBLIC_HOSTNAME} -> ${TUNNEL_NAME}"
  if [[ "${DRY_RUN}" -eq 1 ]]; then
    log "[dry-run] ${CLOUDFLARED_BIN} tunnel route dns ${TUNNEL_NAME} ${PUBLIC_HOSTNAME}"
    return 0
  fi
  # Idempotent: cloudflared errors if the record already exists — treat that as success.
  local out
  if out="$("${CLOUDFLARED_BIN}" tunnel route dns "${TUNNEL_NAME}" "${PUBLIC_HOSTNAME}" 2>&1)"; then
    log "DNS route created."
  else
    if grep -qiE 'already|exists|record with that host' <<<"${out}"; then
      log "DNS route already exists (ok)."
    else
      warn "DNS route failed: ${out}"; exit 1
    fi
  fi
}

# ---------------------------------------------------------------------------
# Step 3 — sshd assets (host key + sshd_config). Purely local, no outward-facing calls.
# ---------------------------------------------------------------------------
setup_sshd_assets() {
  step "Step 3: dedicated sshd assets in ${ASSETS_DIR}"
  mkdirp "${ASSETS_DIR}"

  if [[ "${DRY_RUN}" -eq 1 ]]; then
    log "[dry-run] ssh-keygen -t ed25519 -f ${HOST_KEY} -N '' (if absent)"
  else
    if [[ -f "${HOST_KEY}" ]]; then
      log "Host key already exists: ${HOST_KEY}"
    else
      /usr/bin/ssh-keygen -t ed25519 -f "${HOST_KEY}" -N "" -C "studio-ssh-host" >/dev/null
      log "Generated host key: ${HOST_KEY}"
    fi
    chmod 600 "${HOST_KEY}" 2>/dev/null || true
  fi

  write_file "${SSHD_CONFIG}" <<EOF
Port ${SSHD_PORT}
ListenAddress 127.0.0.1
HostKey ${HOST_KEY}
PidFile ${SSHD_PID}
PasswordAuthentication no
KbdInteractiveAuthentication no
ChallengeResponseAuthentication no
PubkeyAuthentication yes
AuthorizedKeysFile ${AUTHORIZED_KEYS}
AllowUsers rajesh
PermitRootLogin no
UsePAM no
LogLevel VERBOSE
EOF
  log "Wrote ${SSHD_CONFIG}"
}

# ---------------------------------------------------------------------------
# Step 4 — LaunchAgent plists
# ---------------------------------------------------------------------------
setup_plists() {
  step "Step 4: LaunchAgent plists in ${LA_DIR}"
  mkdirp "${LA_DIR}"

  write_file "${SSHD_PLIST}" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${SSHD_LABEL}</string>

    <key>ProgramArguments</key>
    <array>
        <string>${SSHD_BIN}</string>
        <string>-D</string>
        <string>-f</string>
        <string>${SSHD_CONFIG}</string>
    </array>

    <key>RunAtLoad</key>
    <true/>

    <key>KeepAlive</key>
    <true/>

    <key>StandardOutPath</key>
    <string>${ASSETS_DIR}/sshd.out.log</string>

    <key>StandardErrorPath</key>
    <string>${ASSETS_DIR}/sshd.err.log</string>

    <key>ThrottleInterval</key>
    <integer>10</integer>
</dict>
</plist>
EOF
  log "Wrote ${SSHD_PLIST}"

  write_file "${TUNNEL_PLIST}" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${TUNNEL_LABEL}</string>

    <key>ProgramArguments</key>
    <array>
        <string>${CLOUDFLARED_BIN}</string>
        <string>tunnel</string>
        <string>--config</string>
        <string>${CF_CONFIG}</string>
        <string>run</string>
        <string>${TUNNEL_NAME}</string>
    </array>

    <key>RunAtLoad</key>
    <true/>

    <key>KeepAlive</key>
    <true/>

    <key>StandardOutPath</key>
    <string>${CF_DIR}/tunnel.out.log</string>

    <key>StandardErrorPath</key>
    <string>${CF_DIR}/tunnel.err.log</string>

    <key>ThrottleInterval</key>
    <integer>10</integer>

    <key>EnvironmentVariables</key>
    <dict>
        <key>HOME</key>
        <string>${HOME}</string>
        <key>PATH</key>
        <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
    </dict>
</dict>
</plist>
EOF
  log "Wrote ${TUNNEL_PLIST}"
}

# ---------------------------------------------------------------------------
# Step 5 — leave both agents installed but DISABLED (default OFF)
# ---------------------------------------------------------------------------
disable_agents() {
  step "Step 5: leave both LaunchAgents installed but DISABLED (default OFF)"
  local label
  for label in "${SSHD_LABEL}" "${TUNNEL_LABEL}"; do
    # bootout if currently loaded (ignore "not loaded"); then disable.
    run launchctl bootout "${LAUNCHD_DOMAIN}/${label}" || true
    run launchctl disable "${LAUNCHD_DOMAIN}/${label}"
    log "${label}: booted out (if loaded) + disabled"
  done
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
summary() {
  step "Summary"
  cat <<EOF
  Tunnel name        : ${TUNNEL_NAME}
  Public hostname    : ${PUBLIC_HOSTNAME}
  cloudflared config : ${CF_CONFIG}
  sshd config        : ${SSHD_CONFIG}
  sshd host key      : ${HOST_KEY}
  authorized_keys    : ${AUTHORIZED_KEYS}
  loopback bind      : 127.0.0.1:${SSHD_PORT}
  sshd plist         : ${SSHD_PLIST}  (${SSHD_LABEL})
  tunnel plist       : ${TUNNEL_PLIST}  (${TUNNEL_LABEL})
  launchd domain     : ${LAUNCHD_DOMAIN}
  default state      : OFF (both agents disabled)

  The Rust server enables/disables these agents at runtime. To toggle manually:
    launchctl enable ${LAUNCHD_DOMAIN}/${SSHD_LABEL}    && launchctl bootstrap ${LAUNCHD_DOMAIN} ${SSHD_PLIST}
    launchctl enable ${LAUNCHD_DOMAIN}/${TUNNEL_LABEL}  && launchctl bootstrap ${LAUNCHD_DOMAIN} ${TUNNEL_PLIST}
EOF
}

# ---------------------------------------------------------------------------
# Arg parsing + main
# ---------------------------------------------------------------------------
usage() {
  sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

main() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --dry-run)          DRY_RUN=1 ;;
      --sshd-assets-only) SSHD_ASSETS_ONLY=1 ;;
      -h|--help)          usage; exit 0 ;;
      *) warn "Unknown argument: $1"; usage; exit 2 ;;
    esac
    shift
  done

  if [[ "${DRY_RUN}" -eq 1 ]]; then
    printf '### DRY RUN — no outward-facing actions will be taken ###\n'
  fi

  if [[ "${SSHD_ASSETS_ONLY}" -eq 1 ]]; then
    setup_sshd_assets
    step "Done (sshd assets only)."
    exit 0
  fi

  setup_tunnel
  setup_dns
  setup_sshd_assets
  setup_plists
  disable_agents
  summary
  step "Done. Default state is OFF; the server toggles it on demand."
}

main "$@"
