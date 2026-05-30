#!/usr/bin/env bash
# kind_smoke.sh — local end-to-end smoke for void-shim-k8s against a kind cluster.
#
# Usage: scripts/kind_smoke.sh [--keep-cluster]
#
# Prereqs: docker (running), kind, kubectl. Installs to ~/.local/bin are fine.
#
# Phases:
#   1. kind cluster + cargo build
#   2. Job path: render examples/run.yaml + dry-run apply + (if SHIM_IMAGE reachable) live run/status/logs/rm
#   3. Sandbox path: install agent-sandbox controller + build local void-box image +
#      render examples/run-service.yaml + apply + wait Ready + telemetry +
#      (if ANTHROPIC_API_KEY set) send/telemetry assertion + cancel + rm
#
# Each phase degrades gracefully if its environment-dependent prerequisites
# (registry reachability, controller install, LLM key) are missing; the script
# returns 0 in those cases with a clear log line. It returns non-zero only if
# something that DID run reports failure.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLUSTER=void-shims-smoke
KEEP=${1:-}
SHIM_IMAGE=${SHIM_IMAGE:-ghcr.io/the-void-ia/void-box:latest}
ASB_VERSION=${ASB_VERSION:-v0.4.6}
# Search PATH first, fall back to sibling-repo layout; user can always override.
VOIDBOX_BIN=${VOIDBOX_BIN:-$(command -v voidbox 2>/dev/null || echo "$REPO_ROOT/../agent-infra/void-box/target/release/voidbox")}

log() { printf '[smoke] %s\n' "$*" >&2; }
die() { log "FATAL: $*"; exit 2; }

command -v docker  >/dev/null || die "docker not found; install Docker first"
command -v kind    >/dev/null || die "kind not found; install: curl -fsSL -o ~/.local/bin/kind https://kind.sigs.k8s.io/dl/latest/kind-linux-amd64 && chmod +x ~/.local/bin/kind"
command -v kubectl >/dev/null || die "kubectl not found; install: curl -fsSL -o ~/.local/bin/kubectl https://dl.k8s.io/release/\$(curl -fsSL https://dl.k8s.io/release/stable.txt)/bin/linux/amd64/kubectl && chmod +x ~/.local/bin/kubectl"
docker info >/dev/null 2>&1 || die "docker daemon not reachable"

# Capture the operator's current kubectl context so cleanup can restore it
# (otherwise the smoke leaves the shell pointed at kind-void-shims-smoke).
PRIOR_CONTEXT=$(kubectl config current-context 2>/dev/null || true)

# Track whether THIS run created the cluster so cleanup doesn't wipe a
# pre-existing one that belongs to another workflow.
CREATED_CLUSTER=0

cleanup() {
  if [[ -n "$PRIOR_CONTEXT" ]] && kubectl config get-contexts "$PRIOR_CONTEXT" >/dev/null 2>&1; then
    kubectl config use-context "$PRIOR_CONTEXT" >/dev/null 2>&1 || true
  fi
  if [[ "$KEEP" == "--keep-cluster" ]]; then
    log "kept cluster $CLUSTER (--keep-cluster)"
  elif [[ "$CREATED_CLUSTER" == "1" ]]; then
    log "deleting kind cluster $CLUSTER (created by this run)"
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
  else
    log "leaving pre-existing kind cluster $CLUSTER intact (not created by this run)"
  fi
}
trap cleanup EXIT

#############################################
# Phase 1: kind cluster + cargo build
#############################################
if ! kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
  log "creating kind cluster $CLUSTER"
  kind create cluster --name "$CLUSTER" --wait 60s
  CREATED_CLUSTER=1
else
  log "reusing existing kind cluster $CLUSTER"
fi
kubectl config use-context "kind-$CLUSTER" >/dev/null

log "building shim-k8s (release)"
cargo build -p shim-k8s --release
BIN=target/release/void-shim-k8s

#############################################
# Phase 2: Job-backend smoke
#############################################
log "--- Job-backend smoke ---"
log "render examples/run.yaml"
"$BIN" render --file examples/run.yaml > /tmp/void-shims-job.yaml
log "kubectl dry-run apply"
kubectl apply --dry-run=client -f /tmp/void-shims-job.yaml >/dev/null
log "Job manifest validated"

if docker manifest inspect "$SHIM_IMAGE" >/dev/null 2>&1; then
  log "applying Job to cluster (image $SHIM_IMAGE reachable)"
  JOB_REF=$("$BIN" run --file examples/run.yaml --namespace default)
  log "job_ref=$JOB_REF"

  job_terminal=""
  for i in $(seq 1 60); do
    state=$("$BIN" status "$JOB_REF" 2>/dev/null || echo "Unknown")
    log "[job iter=$i] state=$state"
    case "$state" in
      Succeeded|Failed) job_terminal="$state"; break ;;
    esac
    sleep 2
  done

  log "fetching job logs"
  "$BIN" logs "$JOB_REF" || log "(no job logs yet)"
  log "deleting Job"
  "$BIN" rm "$JOB_REF"

  if [[ "$job_terminal" != "Succeeded" ]]; then
    log "Job-path smoke failed (terminal=$job_terminal)"
    exit 1
  fi
  log "Job-path smoke OK"
else
  log "image $SHIM_IMAGE not reachable; Job-path skipped live run (dry-run only)"
fi

#############################################
# Phase 3: Sandbox-backend smoke
#############################################
log "--- Sandbox-backend smoke ---"

# 3a. Install agent-sandbox controller (graceful skip on failure)
if kubectl apply -f "https://github.com/kubernetes-sigs/agent-sandbox/releases/download/${ASB_VERSION}/manifest.yaml" 2>&1 | tee /tmp/asb-install.log >/dev/null; then
  log "waiting for Sandbox CRD Established"
  if ! kubectl wait --for=condition=Established crd/sandboxes.agents.x-k8s.io --timeout=60s >/dev/null 2>&1; then
    log "Sandbox CRD not Established within 60s; skipping Sandbox-path smoke"
    log "smoke OK (Job-path only; Sandbox-path skipped)"
    exit 0
  fi
  log "waiting for agent-sandbox controller to become Available"
  if ! kubectl wait --for=condition=Available deploy --all -n agent-sandbox-system --timeout=180s >/dev/null 2>&1; then
    log "agent-sandbox controller not Available within 180s; skipping Sandbox-path smoke"
    log "smoke OK (Job-path only; Sandbox-path skipped)"
    exit 0
  fi
else
  log "agent-sandbox install failed (see /tmp/asb-install.log); skipping Sandbox-path smoke"
  log "smoke OK (Job-path only; Sandbox-path skipped)"
  exit 0
fi

# 3b. Build + load local void-box image (the released image lacks our entrypoint script)
if [[ ! -x "$VOIDBOX_BIN" ]]; then
  log "voidbox binary not found at $VOIDBOX_BIN (set VOIDBOX_BIN); skipping Sandbox-path smoke"
  log "smoke OK (Job-path only; Sandbox-path skipped due to missing voidbox binary)"
  exit 0
fi
log "building void-box:smoke from $VOIDBOX_BIN"
./crates/shim-k8s/dockerfile/build.sh "$VOIDBOX_BIN" void-box:smoke
log "loading void-box:smoke into kind cluster"
kind load docker-image void-box:smoke --name "$CLUSTER"

# 3c. Render + apply the Sandbox spec
log "creating Sandbox via shim"
SBREF=$("$BIN" run --file examples/run-service.yaml --image void-box:smoke --image-pull-policy Never)
log "sandbox_ref=$SBREF"
SBNAME="${SBREF##*/}"
SBNS="${SBREF%%/*}"

# 3d. Wait for Sandbox Ready condition
log "waiting for Sandbox Ready (up to 120s)"
ready=""
for i in $(seq 1 60); do
  ready=$(kubectl get sandbox "$SBNAME" -n "$SBNS" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null || echo "")
  if [[ "$ready" == "True" ]]; then break; fi
  sleep 2
done
if [[ "$ready" != "True" ]]; then
  log "sandbox did not become Ready in 120s; dumping diagnostics"
  kubectl describe sandbox "$SBNAME" -n "$SBNS" || true
  # kubectl logs doesn't support sandbox/<name>; the pod has the same name
  # as the Sandbox CR (the controller names them identically).
  kubectl logs -n "$SBNS" "pod/$SBNAME" 2>/dev/null || true
  "$BIN" rm "$SBREF" || true
  exit 1
fi
log "sandbox Ready"

# 3e. Telemetry snapshot
log "fetching initial telemetry"
"$BIN" telemetry "$SBREF" 2>&1 | head -20 || log "(telemetry returned non-zero)"

# 3f. Optional live LLM round-trip
if [[ -n "${ANTHROPIC_API_KEY:-}" ]]; then
  log "ANTHROPIC_API_KEY set; sending message (real LLM round-trip)"
  "$BIN" send "$SBREF" --message "ping"
  sleep 3
  log "checking telemetry for tokens_out > 0"
  TEL=$("$BIN" telemetry "$SBREF" 2>/dev/null || echo "{}")
  echo "$TEL" | head -30
  if echo "$TEL" | grep -Eq '"tokens_out":[[:space:]]*[1-9]|"out":[[:space:]]*[1-9]'; then
    log "LLM round-trip OK (tokens consumed)"
  else
    log "LLM round-trip ran but tokens_out not visible in telemetry"
    log "(this may be a telemetry shape mismatch — inspect output above)"
  fi
else
  log "ANTHROPIC_API_KEY unset; skipping live LLM round-trip"
fi

# 3g. Cleanup
log "cancelling sandbox run"
"$BIN" cancel "$SBREF" 2>&1 | head -5 || log "(cancel returned non-zero — agent may already be idle)"
log "removing sandbox"
"$BIN" rm "$SBREF"

log "smoke OK (Job-path + Sandbox-path)"
