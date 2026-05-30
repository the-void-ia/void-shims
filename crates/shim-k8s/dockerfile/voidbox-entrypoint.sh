#!/usr/bin/env bash
set -euo pipefail

MODE="${1:-run}"

case "$MODE" in
  run)
    exec voidbox run --file /spec/run.yaml
    ;;
  serve-and-load)
    : "${VOIDBOX_DAEMON_TOKEN:?VOIDBOX_DAEMON_TOKEN must be set}"
    install -m 600 /dev/stdin /tmp/daemon-token <<<"$VOIDBOX_DAEMON_TOKEN"
    voidbox serve --listen tcp://0.0.0.0:43100 --token-file /tmp/daemon-token &
    DAEMON_PID=$!
    # Wait for the daemon to accept connections
    for i in $(seq 1 30); do
      if curl -sf -H "Authorization: Bearer ${VOIDBOX_DAEMON_TOKEN}" \
              http://127.0.0.1:43100/v1/runs >/dev/null 2>&1; then
        break
      fi
      sleep 0.5
    done
    # POST the spec; daemon reads from /spec/run.yaml via filesystem path
    curl -fsS -X POST \
      -H "Authorization: Bearer ${VOIDBOX_DAEMON_TOKEN}" \
      -H "Content-Type: application/json" \
      -d '{"file":"/spec/run.yaml"}' \
      http://127.0.0.1:43100/v1/runs
    echo
    wait "$DAEMON_PID"
    ;;
  *)
    echo "unknown mode: $MODE (expected: run | serve-and-load)" >&2
    exit 2
    ;;
esac
