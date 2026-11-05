#!/bin/bash
# The till as a macOS launchd service (docs/OPS-PLAN.md O5, "Till").
#
#   till-service.sh install    copy the built binary aside, write the LaunchAgent, start it
#   till-service.sh restart    copy the binary again (after a rebuild) and restart
#   till-service.sh stop       unload; start with `install` again
#   till-service.sh status     launchd's view, /v1/status, the log's tail
#   till-service.sh pair       make the POS token if needed and print the pairing QR (POS-LINK.md)
#   till-service.sh claim NAME ask the registry for a till token; approve it in the provisioner app
#   till-service.sh run        what launchd runs; not for a terminal
#
# Settings live in ~/.config/fedimintcard/till.env (written with defaults on first install):
# OPS_URL, CARD_BACKEND, CARD_BRIDGE_URL, DATA_DIR, TILL_LISTEN. TILL_LISTEN defaults to
# 127.0.0.1:9877 (USB phones through adb reverse); set it to 0.0.0.0:9877 to let paired POS phones
# reach the till over the LAN. The till refuses a non-loopback address until `pair` has made the
# POS token in DATA_DIR (docs/POS-LINK.md); off loopback every request must carry it. The till token is
# ~/.config/fedimintcard/till-token if that file exists (made with registry/scripts/new-till.sh),
# read at every start so it is never in the plist or the env file. Without it the till uses the
# token a `claim` left in DATA_DIR/till-ops.json, or waits for one: `claim` shows a QR code an
# operator approves in the provisioner app, and the running till picks the token up
# (docs/TILL-CLAIM.md). No CARD_PIN: on ops every charge carries the
# holder's PIN (`pin_required`), and the till never opens a card on its own.
#
# launchd restarts the till if it exits (KeepAlive, at most every 10 s). The binary is a copy in
# ~/.local/libexec/fedimintcard, so a `cargo build` does not swap code under a running till.
set -euo pipefail

LABEL=org.fedimintcard.till
CONF=~/.config/fedimintcard
ENV_FILE=$CONF/till.env
TOKEN_FILE=$CONF/till-token
LIBEXEC=~/.local/libexec/fedimintcard
BIN=$LIBEXEC/card-sidecar
PLIST=~/Library/LaunchAgents/$LABEL.plist
LOG_DIR=~/Library/Logs/fedimintcard
LOG=$LOG_DIR/till.log
REPO=$(cd "$(dirname "$0")/../.." && pwd)
BUILT=$REPO/host/target/debug/card-sidecar
SELF=$LIBEXEC/till-service.sh

copy_binary() {
  [ -x "$BUILT" ] || { echo "no till binary at $BUILT: cargo build -p fedimint-card-sidecar first" >&2; exit 1; }
  mkdir -p "$LIBEXEC"
  cp "$BUILT" "$BIN.new" && mv "$BIN.new" "$BIN"
  cp "$0" "$SELF.new" && chmod +x "$SELF.new" && mv "$SELF.new" "$SELF"
}

write_env() {
  [ -f "$ENV_FILE" ] && return
  mkdir -p "$CONF"
  cat > "$ENV_FILE" <<CONF
# Read by till-service.sh at every start.
OPS_URL=https://registry.fedimintcard.org
CARD_BACKEND=bridge
CARD_BRIDGE_URL=http://127.0.0.1:9876
DATA_DIR=$HOME/.local/share/fedimint-card-demo/wallet
# 0.0.0.0:9877 for POS phones on the LAN (run \`till-service.sh pair\` first).
TILL_LISTEN=127.0.0.1:9877
CONF
  echo "wrote $ENV_FILE"
}

write_plist() {
  mkdir -p "$(dirname "$PLIST")" "$LOG_DIR"
  cat > "$PLIST" <<PL
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key><array><string>/bin/bash</string><string>$SELF</string><string>run</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>$LOG</string>
  <key>StandardErrorPath</key><string>$LOG</string>
</dict>
</plist>
PL
}

domain() { echo "gui/$(id -u)"; }

case "${1:-}" in
  run)
    set -a
    # shellcheck disable=SC1090
    . "$ENV_FILE"
    set +a
    if [ -s "$TOKEN_FILE" ]; then
      OPS_TILL_TOKEN=$(tr -d '\n' < "$TOKEN_FILE")
      export OPS_TILL_TOKEN
    fi
    unset CARD_PIN
    echo "$(date -u +%FT%TZ) starting till on $OPS_URL"
    exec "$BIN" --data-dir "$DATA_DIR" till serve --listen "${TILL_LISTEN:-127.0.0.1:9877}"
    ;;
  pair)
    shift
    # shellcheck disable=SC1090
    . "$ENV_FILE"
    [ -x "$BIN" ] || BIN=$BUILT
    exec "$BIN" --data-dir "$DATA_DIR" till pair "$@"
    ;;
  claim)
    shift
    [ -n "${1:-}" ] || { echo "usage: till-service.sh claim NAME" >&2; exit 2; }
    # shellcheck disable=SC1090
    . "$ENV_FILE"
    [ -x "$BIN" ] || BIN=$BUILT
    exec "$BIN" --data-dir "$DATA_DIR" till claim --ops-url "$OPS_URL" --name "$1"
    ;;
  install)
    copy_binary; write_env; write_plist
    launchctl bootout "$(domain)/$LABEL" 2>/dev/null || true
    launchctl bootstrap "$(domain)" "$PLIST"
    echo "installed $LABEL; log $LOG"
    ;;
  restart)
    copy_binary
    launchctl kickstart -k "$(domain)/$LABEL"
    echo "restarted $LABEL"
    ;;
  stop)
    launchctl bootout "$(domain)/$LABEL" && echo "stopped $LABEL"
    ;;
  status)
    launchctl print "$(domain)/$LABEL" 2>/dev/null | grep -E "state =|pid =|last exit code" || echo "$LABEL not loaded"
    curl -s --max-time 3 http://127.0.0.1:9877/v1/status || echo "(till not answering on 9877)"
    echo; tail -n 5 "$LOG" 2>/dev/null || true
    ;;
  *)
    sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
