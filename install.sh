#!/bin/sh
# Installs the latest Parts Relay tunnel on Linux or macOS.
#   curl -fsSL https://raw.githubusercontent.com/PartsRelay/tunnel/main/install.sh | sudo sh -s -- --service
set -eu

RELEASES="${PARTSRELAY_TUNNEL_RELEASES:-https://github.com/PartsRelay/tunnel/releases}"
DEFAULT_PREFIX="/opt/partsrelay"
PREFIX="$DEFAULT_PREFIX"
CONFIG="${PARTSRELAY_TUNNEL_CONFIG:-/etc/partsrelay/tunnel.toml}"
SERVICE=0
PUBKEY=""

usage() {
  cat <<USAGE
Usage: install.sh [--service] [--prefix DIR] [--pubkey FILE]

  --service      also register the tunnel with systemd (Linux) or launchd (macOS)
  --prefix DIR   install under DIR instead of /opt/partsrelay, a directory
                 of the tunnel alone when used with --service
  --pubkey FILE  verify the release signature with this minisign.pub (needs minisign)
USAGE
}

fail() {
  echo "install.sh: $*" >&2
  exit 1
}

while [ $# -gt 0 ]; do
  case "$1" in
    --service) SERVICE=1 ;;
    --prefix)
      [ $# -ge 2 ] || fail "--prefix needs a directory"
      PREFIX="$2"
      shift
      ;;
    --pubkey)
      [ $# -ge 2 ] || fail "--pubkey needs a file"
      PUBKEY="$2"
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      fail "unknown option $1"
      ;;
  esac
  shift
done

case "$(uname -m)" in
  x86_64 | amd64) arch="x86_64" ;;
  arm64 | aarch64) arch="aarch64" ;;
  *) fail "unsupported processor $(uname -m)" ;;
esac

os="$(uname -s)"
case "$os" in
  Linux) target="$arch-unknown-linux-musl" ;;
  Darwin) target="$arch-apple-darwin" ;;
  *) fail "unsupported system $os, on Windows use install.ps1" ;;
esac

for tool in curl tar; do
  command -v "$tool" >/dev/null 2>&1 || fail "$tool is required"
done

# Written when this script creates the prefix
MARK="$PREFIX/.partsrelay-tunnel"

if [ "$SERVICE" -eq 1 ]; then
  [ "$(id -u)" -eq 0 ] || fail "--service needs root"
  if [ "$os" = "Linux" ]; then
    command -v systemctl >/dev/null 2>&1 || fail "--service needs systemd"
    # Its bin directory goes to the account of the service
    if [ "$PREFIX" != "$DEFAULT_PREFIX" ] && [ -e "$PREFIX" ] && [ ! -f "$MARK" ]; then
      fail "$PREFIX is used by something else: --service needs a directory of the tunnel alone"
    fi
  fi
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  fail "sha256sum or shasum is required"
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

archive="partsrelay-tunnel-$target.tar.gz"

echo "Reading the latest release"
curl -fsSL "$RELEASES/latest/download/manifest.json" -o "$work/manifest.json"

if [ -n "$PUBKEY" ]; then
  command -v minisign >/dev/null 2>&1 || fail "--pubkey needs minisign"
  curl -fsSL "$RELEASES/latest/download/manifest.json.minisig" -o "$work/manifest.json.minisig"
  minisign -V -p "$PUBKEY" -m "$work/manifest.json" >/dev/null || fail "the release signature is not valid"
fi

compact="$(tr -d ' \n\r\t' <"$work/manifest.json")"
tag="$(printf '%s' "$compact" | sed -n 's/.*"tag":"\([^"]*\)".*/\1/p')"
expected="$(printf '%s' "$compact" | sed -n "s/.*\"$target\":{[^}]*\"sha256\":\"\([0-9a-f]*\)\".*/\1/p")"

[ -n "$tag" ] || fail "the release manifest has no tag"
[ -n "$expected" ] || fail "release $tag has no build for $target"

echo "Downloading $archive of $tag"
curl -fsSL "$RELEASES/download/$tag/$archive" -o "$work/$archive"

[ "$(sha256 "$work/$archive")" = "$expected" ] || fail "$archive does not match its SHA-256"

tar -xzf "$work/$archive" -C "$work" partsrelay-tunnel
if [ ! -e "$PREFIX" ]; then
  mkdir -p "$PREFIX"
  : >"$MARK"
fi
mkdir -p "$PREFIX/bin"
install -m 0755 "$work/partsrelay-tunnel" "$PREFIX/bin/partsrelay-tunnel"

shortcut="/usr/local/bin/partsrelay-tunnel"
if [ "$SERVICE" -eq 1 ] && [ "$os" = "Linux" ]; then
  # Writable by the service: not in the PATH of root
  if [ "$(readlink "$shortcut" 2>/dev/null)" = "$PREFIX/bin/partsrelay-tunnel" ]; then
    rm -f "$shortcut"
  fi
elif [ "$PREFIX" = "$DEFAULT_PREFIX" ] && [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
  ln -sf "$PREFIX/bin/partsrelay-tunnel" "$shortcut"
fi

if [ ! -e "$CONFIG" ] && mkdir -p "$(dirname "$CONFIG")" 2>/dev/null; then
  cat >"$CONFIG" <<'CONFIG'
# Parts Relay tunnel. Every setting is described in the README.

# The token Parts Relay gave you for this tunnel
token = ""

# The only targets the tunnel may reach, as host:port
allow = []
CONFIG
  chmod 0640 "$CONFIG"
  echo "Created $CONFIG: set the token and the allow list"
fi

echo "Installed $("$PREFIX/bin/partsrelay-tunnel" --version) in $PREFIX/bin"

[ "$SERVICE" -eq 1 ] || exit 0

if [ "$os" = "Linux" ]; then
  if ! id partsrelay >/dev/null 2>&1; then
    useradd --system --no-create-home --shell /usr/sbin/nologin partsrelay
  fi
  # The tunnel replaces its own executable when it updates
  chown -R partsrelay "$PREFIX/bin"
  # Only root and the tunnel read the token
  chgrp partsrelay "$CONFIG"
  chmod 0640 "$CONFIG"

  cat >/etc/systemd/system/partsrelay-tunnel.service <<UNIT
[Unit]
Description=Parts Relay tunnel
Documentation=https://github.com/PartsRelay/tunnel
After=network-online.target
Wants=network-online.target

[Service]
User=partsrelay
ExecStart=$PREFIX/bin/partsrelay-tunnel run
Restart=always
RestartSec=5
# A broken configuration is not retried
RestartPreventExitStatus=2
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=$PREFIX/bin
# Far above what the tunnel buffers
MemoryMax=256M

[Install]
WantedBy=multi-user.target
UNIT

  systemctl daemon-reload
  systemctl enable partsrelay-tunnel >/dev/null
  echo "Service registered. Edit $CONFIG, then run: systemctl start partsrelay-tunnel"
  echo "To test it: sudo -u partsrelay $PREFIX/bin/partsrelay-tunnel check"
else
  plist="/Library/LaunchDaemons/com.partsrelay.tunnel.plist"

  cat >"$plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.partsrelay.tunnel</string>
  <key>ProgramArguments</key>
  <array>
    <string>$PREFIX/bin/partsrelay-tunnel</string>
    <string>run</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>StandardErrorPath</key>
  <string>/var/log/partsrelay-tunnel.log</string>
</dict>
</plist>
PLIST

  chmod 0644 "$plist"
  echo "Service registered. Edit $CONFIG, then run: launchctl bootstrap system $plist"
fi
