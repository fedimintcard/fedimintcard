#!/bin/bash
# The till as a macOS launchd service (docs/OPS-PLAN.md O5, "Till").
#
#   till-service.sh install    copy the built binary aside, write the LaunchAgent, start it
#   till-service.sh restart    copy the binary again (after a rebuild) and restart
#   till-service.sh stop       unload; start with `install` again
#   till-service.sh status     launchd's view, /v1/status, the log's tail
#   till-service.sh run        what launchd runs; not for a terminal
#
# Settings live in ~/.config/fedimintcard/till.env (written with defaults on first install):
# OPS_URL, CARD_BACKEND, CARD_BRIDGE_URL, DATA_DIR. The till token is read from
# ~/.config/fedimintcard/till-token at every start, so it is never in the plist or the env file;
# make it with registry/scripts/new-till.sh. No CARD_PIN: on ops every charge carries the
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
    [ -s "$TOKEN_FILE" ] || { echo "$(date -u +%FT%TZ) no till token at $TOKEN_FILE" >&2; sleep 60; exit 1; }
    OPS_TILL_TOKEN=$(tr -d '\n' < "$TOKEN_FILE")
    export OPS_TILL_TOKEN
    unset CARD_PIN
    echo "$(date -u +%FT%TZ) starting till on $OPS_URL"
    exec "$BIN" --data-dir "$DATA_DIR" till serve
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
    sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
