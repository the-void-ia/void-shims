#!/usr/bin/env bash
# kind_smoke.sh — local end-to-end smoke for void-shim-k8s against a kind cluster.
#
# Usage: scripts/kind_smoke.sh [--keep-cluster]
#
# Prereqs: docker (running), kind, kubectl. Installs to ~/.local/bin are fine.
set -euo pipefail

CLUSTER=void-shims-smoke
KEEP=${1:-}
SHIM_IMAGE=${SHIM_IMAGE:-ghcr.io/the-void-ia/void-box:latest}

log() { printf '[smoke] %s\n' "$*" >&2; }
die() { log "FATAL: $*"; exit 2; }

command -v docker  >/dev/null || die "docker not found; install Docker first"
command -v kind    >/dev/null || die "kind not found; install: curl -fsSL -o ~/.local/bin/kind https://kind.sigs.k8s.io/dl/latest/kind-linux-amd64 && chmod +x ~/.local/bin/kind"
command -v kubectl >/dev/null || die "kubectl not found; install: curl -fsSL -o ~/.local/bin/kubectl https://dl.k8s.io/release/\$(curl -fsSL https://dl.k8s.io/release/stable.txt)/bin/linux/amd64/kubectl && chmod +x ~/.local/bin/kubectl"
docker info >/dev/null 2>&1 || die "docker daemon not reachable"

cleanup() {
  if [[ "$KEEP" != "--keep-cluster" ]]; then
    log "deleting kind cluster $CLUSTER"
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
  else
    log "kept cluster $CLUSTER (--keep-cluster)"
  fi
}
trap cleanup EXIT

# 1. ensure cluster
if ! kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
  log "creating kind cluster $CLUSTER"
  kind create cluster --name "$CLUSTER" --wait 60s
else
  log "reusing existing kind cluster $CLUSTER"
fi
kubectl config use-context "kind-$CLUSTER" >/dev/null

# 2. build shim
log "building shim-k8s (release)"
cargo build -p shim-k8s --release

BIN=target/release/void-shim-k8s

# 3. render + dry-run apply
log "render examples/run.yaml"
"$BIN" render --file examples/run.yaml > /tmp/void-shims-job.yaml
log "kubectl dry-run apply"
kubectl apply --dry-run=client -f /tmp/void-shims-job.yaml >/dev/null
log "manifest validated"

# 4. probe whether the container image is reachable; if not, stop after dry-run
if ! docker manifest inspect "$SHIM_IMAGE" >/dev/null 2>&1; then
  log "image $SHIM_IMAGE not reachable from this host; skipping live run"
  log "smoke OK (dry-run only)"
  exit 0
fi

# 5. live run → status loop → logs → rm
log "applying Job to cluster"
RUN_REF=$("$BIN" run --file examples/run.yaml --namespace default)
log "run_ref=$RUN_REF"

terminal_state=""
for i in $(seq 1 60); do
  state=$("$BIN" status "$RUN_REF" 2>/dev/null || echo "Unknown")
  log "iter=$i state=$state"
  case "$state" in
    Succeeded|Failed) terminal_state="$state"; break ;;
  esac
  sleep 2
done

log "fetching logs"
"$BIN" logs "$RUN_REF" || log "(no logs yet)"

log "deleting Job"
"$BIN" rm "$RUN_REF"

if [[ "$terminal_state" == "Succeeded" ]]; then
  log "smoke OK (live run Succeeded)"
  exit 0
elif [[ "$terminal_state" == "Failed" ]]; then
  log "smoke ran but Job reported Failed (inspect logs above)"
  exit 1
else
  log "smoke timed out before Job reached terminal state"
  exit 1
fi
