#!/usr/bin/env bash
set -euo pipefail

if [[ "$(id -u)" -ne 0 ]]; then
  echo "error: run this installer as root (for example, sudo ./install.sh)" >&2
  exit 1
fi

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
binary="${script_dir}/harmost"
unit="${script_dir}/harmost.service"

if [[ ! -x "${binary}" || ! -f "${unit}" ]]; then
  echo "error: install.sh must stay beside the harmost binary and harmost.service" >&2
  exit 1
fi
if ! command -v systemctl >/dev/null 2>&1; then
  echo "error: this installer requires a Linux system using systemd" >&2
  exit 1
fi

if ! getent group harmost >/dev/null 2>&1; then
  groupadd --system harmost
fi
if ! id -u harmost >/dev/null 2>&1; then
  nologin_shell="$(command -v nologin || true)"
  if [[ -z "${nologin_shell}" ]]; then
    nologin_shell="/usr/sbin/nologin"
  fi
  useradd --system --gid harmost --home-dir /var/lib/harmost \
    --shell "${nologin_shell}" harmost
fi

install -m 0755 "${binary}" /usr/local/bin/harmost
install -d -m 0750 -o root -g harmost /etc/harmost
install -m 0644 "${unit}" /etc/systemd/system/harmost.service

config="/etc/harmost/harmost.yaml"
if [[ ! -e "${config}" ]]; then
  /usr/local/bin/harmost init --config "${config}"
fi
chown root:harmost "${config}"
chmod 0640 "${config}"

systemctl daemon-reload

cat <<'EOF'
Harmost is installed. Configuration is preserved on later upgrades.

Next:
  1. Review /etc/harmost/harmost.yaml and run: harmost check
  2. Start the application on 127.0.0.1:3000
  3. Run: systemctl enable --now harmost
  4. Point Caddy or another TLS edge at 127.0.0.1:8080
EOF
